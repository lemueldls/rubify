pub mod pen;
pub mod renderer;
pub mod ttc;

use anyhow::{Context, Result};
use fontcull_font_types::NameId;
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    FileRef, FontRef, TableProvider, TopLevelTable,
    collections::IntSet,
    types::{GlyphId, Tag},
};
use fontcull_skrifa::MetadataProvider;
use fontcull_write_fonts::{
    FontBuilder,
    from_obj::ToOwnedObj,
    tables::{
        glyf::{Glyf, GlyfLocaBuilder, Glyph, SimpleGlyph},
        head::Head,
        loca::Loca,
    },
};
use indicatif::ProgressStyle;
use kurbo::BezPath;
use rayon::iter::{ParallelBridge, ParallelIterator};
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
    renderer: &Box<dyn RubyRenderer>,
    subset: bool,
    split: bool,
) -> Result<Vec<ProcessedFont>> {
    match file {
        FileRef::Font(font) => {
            let data = process_font_ref(&font, &renderer)?;
            let data = if subset {
                info!("Subsetting font");

                subset_by_renderers(&data, &renderer)?
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
                    .map(|(idx, font)| {
                        collection_span.pb_inc(1);

                        let font = font.context("Failed to read font")?;
                        let mut data = process_font_ref(&font, &renderer)?;

                        if subset {
                            collection_span.pb_set_message("Subsetting font");
                            data = subset_by_renderers(&data, &renderer)?;
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

                let fonts = collection
                    .iter()
                    .par_bridge()
                    .map(|font| {
                        collection_span.pb_inc(1);
                        collection_span.pb_set_message("Processing font");

                        let font = font.context("Failed to read font")?;

                        let mut data = process_font_ref(&font, &renderer)?;

                        if subset {
                            collection_span.pb_set_message("Subsetting font");
                            data = subset_by_renderers(&data, &renderer)?;
                        }

                        let data = Box::leak(data.into_boxed_slice());

                        FontRef::new(data).context("Failed to create font ref")
                    })
                    .collect::<Result<Vec<FontRef>>>()?;

                drop(process_span_enter);

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

pub fn process_font_ref(font: &FontRef, renderer: &Box<dyn RubyRenderer>) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx()?;
    let maxp = font.maxp()?;
    let outlines = font.outline_glyphs();
    let upem = font.head()?.units_per_em() as f64;

    let gid_char_map = renderer
        .ranges()
        .iter()
        .cloned()
        .flat_map(|range| {
            range.filter_map(|c_u32| {
                std::char::from_u32(c_u32).and_then(|c| {
                    charmap
                        .map(c)
                        .and_then(|gid| (gid != GlyphId::NOTDEF).then_some((gid, c)))
                })
            })
        })
        .collect::<FxHashMap<GlyphId, char>>();

    // let glyphs = if subset {
    //     gid_char_map.keys().copied().collect::<Vec<GlyphId>>()
    // } else {
    //     (0..(maxp.num_glyphs() as u32))
    //         .map(GlyphId::new)
    //         .collect::<Vec<GlyphId>>()
    // };

    let glyphs = (0..(maxp.num_glyphs() as u32))
        .map(GlyphId::new)
        .collect::<Vec<GlyphId>>();

    let progress_style = ProgressStyle::with_template(
        "{spinner:.green} {msg} {wide_bar:.cyan/blue} {pos:>7}/{len:7}",
    )?
    .progress_chars("##-");

    let glyphs_span = info_span!("process_glyphs");
    glyphs_span.pb_set_style(&progress_style);
    glyphs_span.pb_set_length(glyphs.len() as u64);

    if let Some(ttc_index) = font.ttc_index() {
        glyphs_span.pb_set_message(&format!("Processing glyphs ({})", ttc_index));
    } else {
        glyphs_span.pb_set_message("Processing glyphs");
    }

    let glyphs_span_enter = glyphs_span.enter();

    let mut glyf_loca_builder = GlyfLocaBuilder::new();

    for gid in glyphs {
        glyphs_span.pb_inc(1);

        let mut final_path = BezPath::new();
        let mut has_content = false;

        if let Some(glyph) = outlines.get(fontcull_skrifa::GlyphId::new(gid.to_u32())) {
            let mut pen = PathPen::new();

            match glyph.draw(fontcull_skrifa::instance::Size::unscaled(), &mut pen) {
                Ok(_) => {
                    final_path = pen.path;
                    has_content = true;
                }
                Err(_) => {}
            }
        }

        if let Some(&ch) = gid_char_map.get(&gid) {
            let orig_advance = hmtx
                .h_metrics()
                .get(gid.to_u32() as usize)
                .map(|m| m.advance.get())
                .unwrap_or(upem as u16) as f64;

            renderer
                .annotate(ch, &mut final_path, orig_advance, upem)
                .context("Failed to annotate")?;
        }

        let write_glyph = if !has_content && final_path.elements().is_empty() {
            Glyph::Empty
        } else {
            match SimpleGlyph::from_bezpath(&final_path) {
                Ok(s) => Glyph::Simple(s),
                Err(_) => Glyph::Empty,
            }
        };

        glyf_loca_builder.add_glyph(&write_glyph)?;
    }

    drop(glyphs_span_enter);
    drop(glyphs_span);

    let (glyf_data, loca_data, loca_fmt) = glyf_loca_builder.build();

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

                head.index_to_loc_format = loca_fmt as i16;
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
        .add_table(&glyf_data)
        .context("Failed to add glyf table")?
        .add_table(&loca_data)
        .context("Failed to add loca table")?;

    Ok(font_builder.build())
}

pub fn subset_by_renderers(font_data: &[u8], renderer: &Box<dyn RubyRenderer>) -> Result<Vec<u8>> {
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

fn get_all_name_records(font: &FontRef) -> Result<Vec<(NameId, String)>> {
    let name_table = font.name().context("No name table found")?;
    let string_data = name_table.string_data();

    let records = name_table
        .name_record()
        .iter()
        .filter_map(|rec| {
            rec.string(string_data)
                .ok()
                .map(|s| (rec.name_id(), s.to_string()))
        })
        .collect();

    Ok(records)
}
