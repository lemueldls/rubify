pub mod pen;
pub mod renderer;
pub mod ttc;

use anyhow::{Context, Result};
use fontcull_font_types::NameId;
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    FileRef, FontData, FontRef, TableProvider, TopLevelTable,
    collections::IntSet,
    tables::{glyf::Glyf, loca::Loca},
    types::{GlyphId, Tag},
};
use fontcull_skrifa::{MetadataProvider, instance::Size};
use fontcull_write_fonts::{
    FontBuilder, dump_table,
    from_obj::ToOwnedObj,
    tables::{glyf::SimpleGlyph, head::Head},
};
use indicatif::ProgressStyle;
use kurbo::BezPath;
use rayon::iter::{IntoParallelIterator, ParallelBridge, ParallelIterator};
use rustc_hash::FxHashMap;
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{pen::PathPen, renderer::RubyRenderer};

pub struct ProcessedFont {
    pub data: Vec<u8>,
    pub file_name: Option<String>,
}

pub fn process_font_file(
    file: FileRef,
    renderer: &dyn RubyRenderer,
    subset: bool,
    split: bool,
) -> Result<Vec<ProcessedFont>> {
    match file {
        FileRef::Font(font) => {
            let data = process_font_ref(&font, renderer)?;
            let data = if subset {
                info!("Subsetting font");

                subset_by_renderers(&data, renderer)?
            } else {
                data
            };

            Ok(vec![ProcessedFont {
                data,
                file_name: None,
            }])
        }
        FileRef::Collection(collection) => {
            if split {
                // Split mode: write each font as a separate TTF file
                let collection_span = info_span!("split_fonts_in_collection");
                collection_span.pb_set_style(
                    &ProgressStyle::with_template("{msg} [{wide_bar:.green/cyan}] {pos}/{len}")
                        .unwrap(),
                );
                collection_span.pb_set_length(collection.len() as u64);
                collection_span.pb_set_message("Splitting collection");

                let split_span_enter = collection_span.enter();

                let fonts = collection
                    .iter()
                    .enumerate()
                    .par_bridge()
                    .map(|(idx, font)| {
                        collection_span.pb_inc(1);

                        let font = font.context("Failed to read font")?;
                        let mut data = process_font_ref(&font, renderer)?;

                        if subset {
                            collection_span.pb_set_message("Subsetting font");
                            data = subset_by_renderers(&data, renderer)?;
                        }

                        // Generate output filename
                        let file_name = if let Ok(name_table) = font.name() {
                            // Try to get family name from name table
                            name_table
                                .name_record()
                                .iter()
                                .find(|n| n.name_id() == NameId::POSTSCRIPT_NAME)
                                .and_then(|rec| rec.string(name_table.string_data()).ok())
                                .map(|name| format!("{name}.ttf"))
                                .unwrap_or_else(|| format!("font-{idx}.ttf"))
                        } else {
                            format!("font-{idx}.ttf")
                        };

                        Ok(ProcessedFont {
                            data,
                            file_name: Some(file_name),
                        })
                    })
                    .collect::<Result<Vec<ProcessedFont>>>();

                drop(split_span_enter);
                drop(collection_span);

                fonts
            } else {
                let collection_span = info_span!("process_fonts_in_collection");
                collection_span.pb_set_style(
                    &ProgressStyle::with_template("{msg} [{wide_bar:.green/cyan}] {pos}/{len}")
                        .unwrap(),
                );
                collection_span.pb_set_length(collection.len() as u64);
                collection_span.pb_set_message("Processing collection");

                let process_span_enter = collection_span.enter();

                let processed = collection
                    .iter()
                    .par_bridge()
                    .map(|font| {
                        collection_span.pb_inc(1);
                        collection_span.pb_set_message("Processing font");

                        let font = font.context("Failed to read font")?;

                        let mut data = process_font_ref(&font, renderer)?;

                        if subset {
                            collection_span.pb_set_message("Subsetting font");
                            data = subset_by_renderers(&data, renderer)?;
                        }

                        Ok(data.into_boxed_slice())
                    })
                    .collect::<Result<Vec<Box<[u8]>>>>()?;

                drop(process_span_enter);

                let fonts = processed
                    .iter()
                    .map(|data| FontRef::new(data).context("Failed to create font ref"))
                    .collect::<Result<Vec<FontRef>>>()?;

                info_span!("Building TTC");

                let data = ttc::build_collection(&fonts).context("Failed to build TTC")?;

                Ok(vec![ProcessedFont {
                    data,
                    file_name: None,
                }])
            }
        }
    }
}

pub fn process_font_ref(font: &FontRef, renderer: &dyn RubyRenderer) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx()?;
    let maxp = font.maxp()?;
    let outlines = font.outline_glyphs();
    let upem = font.head()?.units_per_em() as f64;

    let ranges = renderer.ranges();
    let gid_char_map: FxHashMap<GlyphId, char> = charmap
        .mappings()
        .filter_map(|(c, gid)| {
            if gid != GlyphId::NOTDEF && ranges.iter().any(|r| r.contains(&c)) {
                std::char::from_u32(c).map(|ch| (gid, ch))
            } else {
                None
            }
        })
        .collect();

    let num_glyphs = maxp.num_glyphs() as usize;

    // Raw glyf data so untouched glyphs can be copied verbatim instead of
    // re-drawing and re-encoding them (preserves hinting and composite glyphs).
    let glyf_raw = font.glyf().ok().map(|g| g.offset_data());
    let loca = font.loca(None).ok();

    // Precompute advances once instead of indexing hmtx per glyph.
    let advances: Vec<f64> = hmtx
        .h_metrics()
        .iter()
        .map(|m| m.advance.get() as f64)
        .collect();

    let progress_style = ProgressStyle::with_template(
        "{spinner:.green} {msg} {wide_bar:.cyan/blue} {pos:>7}/{len:7}",
    )?
    .progress_chars("##-");

    let glyphs_span = info_span!("process_glyphs");
    glyphs_span.pb_set_style(&progress_style);
    glyphs_span.pb_set_length(num_glyphs as u64);

    if let Some(ttc_index) = font.ttc_index() {
        glyphs_span.pb_set_message(&format!("Processing glyphs ({})", ttc_index));
    } else {
        glyphs_span.pb_set_message("Processing glyphs");
    }

    let glyphs_span_enter = glyphs_span.enter();

    let new_glyphs: Vec<Vec<u8>> = (0..num_glyphs)
        .into_par_iter()
        .map(|gid_u32| {
            glyphs_span.pb_inc(1);

            let gid = GlyphId::new(gid_u32 as u32);
            let mut final_path = BezPath::new();

            if let Some(ch) = gid_char_map.get(&gid).copied() {
                if let Some(glyph) = outlines.get(gid) {
                    let mut pen = PathPen::new();

                    if glyph.draw(Size::unscaled(), &mut pen).is_ok() {
                        final_path = pen.path;
                    }
                }

                let orig_advance = advances.get(gid_u32).copied().unwrap_or(upem);

                renderer
                    .annotate(ch, &mut final_path, orig_advance, upem)
                    .context("Failed to annotate")?;
            }

            if !final_path.elements().is_empty()
                && let Ok(simple) = SimpleGlyph::from_bezpath(&final_path)
                && let Ok(bytes) = dump_table(&simple)
            {
                return Ok(bytes);
            }

            // Fall back to copying the original glyph data verbatim.
            copy_original_glyph(&loca, &glyf_raw, gid_u32)
        })
        .collect::<Result<Vec<Vec<u8>>>>()?;

    drop(glyphs_span_enter);
    drop(glyphs_span);

    let mut glyf_out: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(num_glyphs + 1);
    offsets.push(0);

    for bytes in new_glyphs {
        if !bytes.is_empty() {
            glyf_out.extend_from_slice(&bytes);
        }
        offsets.push(glyf_out.len() as u32);
    }

    let loca_out = fontcull_write_fonts::tables::loca::Loca::new(offsets);
    let loca_fmt = loca_out.format() as i16;

    let mut font_builder = FontBuilder::new();

    for record in font.table_directory.table_records() {
        let tag = record.tag();

        // Skip glyf/loca - we'll insert rebuilt data later
        if tag == Glyf::TAG || tag == Loca::TAG {
            continue;
        }

        if tag == Head::TAG {
            if let Ok(head) = font.head() {
                let mut head: Head = head.to_owned_obj(font_file_data);

                head.index_to_loc_format = loca_fmt;
                head.checksum_adjustment = 0;

                font_builder
                    .add_table(&head)
                    .context("Failed to add head table")?;
            }

            continue;
        }

        if let Some(data) = font.data_for_tag(tag) {
            font_builder.add_raw(tag, data.as_bytes().to_vec());
        }
    }

    font_builder
        .add_raw(Glyf::TAG, glyf_out)
        .add_raw(Loca::TAG, dump_table(&loca_out)?);

    Ok(font_builder.build())
}

/// Copy the original raw glyf bytes for a glyph, preserving hinting and
/// composite structure. Returns empty bytes for empty/missing glyphs.
fn copy_original_glyph(
    loca: &Option<Loca<'_>>,
    glyf_raw: &Option<FontData<'_>>,
    gid: usize,
) -> Result<Vec<u8>> {
    if let (Some(loca), Some(raw)) = (loca, glyf_raw) {
        let start = loca.get_raw(gid).map(|s| s as usize).unwrap_or(0);
        let end = loca.get_raw(gid + 1).map(|e| e as usize).unwrap_or(start);

        if end > start
            && let Some(data) = raw.slice(start..end)
        {
            return Ok(data.as_bytes().to_vec());
        }
    }

    Ok(Vec::new())
}

pub fn subset_by_renderers(font_data: &[u8], renderer: &dyn RubyRenderer) -> Result<Vec<u8>> {
    let font = FontRef::new(font_data).context("Failed to parse font for subsetting")?;

    // Build unicodes set based on provided character sets
    let mut unicodes = IntSet::<u32>::empty();

    for range in renderer.ranges() {
        for c in range.clone() {
            unicodes.insert(c);
        }
    }

    let glyph_ids = IntSet::<GlyphId>::empty();
    let drop_tables = IntSet::<Tag>::empty();
    let no_subset_tables = IntSet::<Tag>::empty();
    let passthrough_tables = IntSet::<Tag>::empty();
    let name_ids = IntSet::<NameId>::empty();
    let name_languages = IntSet::<u16>::empty();

    let plan = Plan::new(
        &glyph_ids,
        &unicodes,
        &font,
        SubsetFlags::default(),
        &drop_tables,
        &no_subset_tables,
        &passthrough_tables,
        &name_ids,
        &name_languages,
    );

    subset_font(&font, &plan).context("Subset error")
}

#[cfg(feature = "woff2")]
pub fn convert_to_woff2(font_data: &[u8]) -> Result<Vec<u8>> {
    woofwoof::compress(font_data, &[], 11, true).context("WOFF2 compression failed")
}
