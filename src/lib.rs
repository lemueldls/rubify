pub mod renderer;

use std::ops::RangeInclusive;

use fontcull_font_types::NameId;
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    FileRef, FontRef, TableProvider, TopLevelTable,
    collections::IntSet,
    tables::cff::Cff,
    types::{GlyphId, Tag},
};
use fontcull_skrifa::{MetadataProvider, outline::OutlinePen};
use fontcull_write_fonts::{
    FontBuilder, dump_table,
    from_obj::{FromObjRef, ToOwnedObj},
    tables::{
        glyf::{Glyf, GlyfLocaBuilder, Glyph, SimpleGlyph},
        head::Head,
        loca::Loca,
        name::Name,
    },
};
use indicatif::ProgressStyle;
use kurbo::BezPath;
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use rayon::iter::{ParallelBridge, ParallelIterator};
use rustc_hash::FxHashMap;
use tracing::info_span;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use woofwoof;

use crate::renderer::RubyRenderer;

pub fn process_font_file(
    file: FileRef,
    char_ranges: &[RangeInclusive<u32>],
    renderers: &[Box<dyn RubyRenderer>],
) -> Result<Vec<u8>> {
    match file {
        FileRef::Font(font) => process_single_font(font, char_ranges, renderers),
        FileRef::Collection(collection) => {
            let head = collection
                .iter()
                .next()
                .wrap_err("No fonts in collection")?
                .into_diagnostic()?;

            let family_name = get_family_name(&head).unwrap();

            let collection_span = info_span!("process_fonts_in_collection");
            collection_span.pb_set_style(
                &ProgressStyle::with_template(
                    "[{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap(),
            );
            collection_span.pb_set_length(collection.len() as u64);
            collection_span.pb_set_message("Processing font collection");

            let process_span_enter = collection_span.enter();

            let fonts = collection
                .iter()
                .par_bridge()
                .map(|font| {
                    collection_span.pb_inc(1);

                    let font = font.map_err(|err| miette!("Failed to read font: {err:?}"))?;

                    let data = process_single_font(font, char_ranges, renderers)?;
                    let data = Box::leak(data.into_boxed_slice());

                    FontRef::new(data).into_diagnostic()
                })
                .collect::<Result<Vec<FontRef>>>()?;

            drop(process_span_enter);

            build_ttc(&fonts, &family_name)
        }
    }
}

fn process_single_font(
    font: FontRef,
    char_ranges: &[RangeInclusive<u32>],
    renderers: &[Box<dyn RubyRenderer>],
) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx().into_diagnostic()?;
    let maxp = font.maxp().into_diagnostic()?;
    let outlines = font.outline_glyphs();
    let upem = font.head().into_diagnostic()?.units_per_em() as f64;

    let gid_char_map = char_ranges
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
    )
    .into_diagnostic()?
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
            for renderer in renderers {
                let orig_advance = hmtx
                    .h_metrics()
                    .get(gid.to_u32() as usize)
                    .map(|m| m.advance.get())
                    .unwrap_or(upem as u16) as f64;

                renderer
                    .annotate(ch, &mut final_path, orig_advance, upem)
                    .wrap_err("Failed to annotate")?;
            }
        }

        let write_glyph = if !has_content && final_path.elements().is_empty() {
            Glyph::Empty
        } else {
            match SimpleGlyph::from_bezpath(&final_path) {
                Ok(s) => Glyph::Simple(s),
                Err(_) => Glyph::Empty,
            }
        };

        glyf_loca_builder
            .add_glyph(&write_glyph)
            .into_diagnostic()?;
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
                    .into_diagnostic()
                    .wrap_err("Failed to add head table")?;
            }

            continue;
        }

        if let Some(data) = font.data_for_tag(tag) {
            font_builder.add_raw(tag, data.as_bytes().to_vec());
        }
    }

    font_builder
        .add_table(&glyf_data)
        .into_diagnostic()
        .wrap_err("Failed to add glyf table")?
        .add_table(&loca_data)
        .into_diagnostic()
        .wrap_err("Failed to add loca table")?;

    Ok(font_builder.build())
}

pub fn subset_by_renderers(
    font_data: &[u8],
    renderers: &[Box<dyn RubyRenderer>],
) -> Result<Vec<u8>> {
    let file =
        FileRef::new(font_data).map_err(|_| miette!("Failed to parse font for subsetting"))?;
    let font = file
        .fonts()
        .next()
        .wrap_err("No font found for subsetting")?
        .map_err(|e| miette!("Read error: {:?}", e))?;

    // Build unicodes set based on provided character sets
    let mut unicodes = IntSet::<u32>::empty();

    for renderer in renderers {
        for range in renderer.ranges() {
            for c in range.clone() {
                unicodes.insert(c);
            }
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

    subset_font(&font, &plan).map_err(|e| miette!("Subset error: {:?}", e))
}

pub fn convert_to_woff2(font_data: &[u8]) -> Result<Vec<u8>> {
    woofwoof::compress(font_data, &[], 11, true).ok_or_else(|| miette!("WOFF2 compression failed"))
}

pub struct PathPen {
    pub path: BezPath,
}

impl PathPen {
    pub fn new() -> Self {
        Self {
            path: BezPath::new(),
        }
    }
}

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to((x as f64, y as f64));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to((x as f64, y as f64));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.path
            .quad_to((cx0 as f64, cy0 as f64), (x as f64, y as f64));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.path.curve_to(
            (cx0 as f64, cy0 as f64),
            (cx1 as f64, cy1 as f64),
            (x as f64, y as f64),
        );
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}

pub fn build_ttc(fonts: &[FontRef], family_name: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();

    // TTC header
    out.extend_from_slice(b"ttcf"); // Tag
    out.extend_from_slice(&1u16.to_be_bytes()); // Major
    out.extend_from_slice(&0u16.to_be_bytes()); // Minor
    out.extend_from_slice(&(fonts.len() as u32).to_be_bytes());

    let offset_table_start = out.len();

    for _ in 0..fonts.len() {
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    let mut font_offsets = Vec::new();
    let mut table_cache: FxHashMap<(Tag, Vec<u8>), u32> = FxHashMap::default();
    let mut table_data_block = Vec::new();

    // Process and rewrite each font
    for font in fonts {
        font_offsets.push(out.len() as u32);
        let records = font.table_directory().table_records();
        let num_tables = records.len() as u16;

        // Write OffsetTable header
        out.extend_from_slice(&0x00010000u32.to_be_bytes()); // sfntVersion
        out.extend_from_slice(&num_tables.to_be_bytes());
        let entry_selector = (num_tables as f32).log2().floor() as u16;
        let search_range = (2u16.pow(entry_selector as u32)) * 16;
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&entry_selector.to_be_bytes());
        out.extend_from_slice(&(num_tables * 16 - search_range).to_be_bytes());

        for record in records {
            let tag = record.tag();
            let mut data = font
                .table_data(tag)
                .ok_or_else(|| miette!("Table missing"))?
                .as_ref()
                .to_vec();

            // Rewrite name table
            if tag == Name::TAG {
                let name_table = font.name().into_diagnostic()?;
                let mut new_name = Name::from_obj_ref(&name_table, font.data());

                for rec in new_name.name_record.iter_mut() {
                    match rec.name_id {
                        NameId::FAMILY_NAME => {
                            rec.string = family_name.to_string().into();
                        }
                        _ => {}
                    }
                }

                data = dump_table(&new_name).into_diagnostic()?;
            }

            // Only share tables that are usually safe and heavy
            let can_share = matches!(tag, Glyf::TAG | Cff::TAG | Loca::TAG);

            let rel_offset = if can_share {
                if let Some(&off) = table_cache.get(&(tag, data.clone())) {
                    off
                } else {
                    while table_data_block.len() % 4 != 0 {
                        table_data_block.push(0);
                    }

                    let off = table_data_block.len() as u32;
                    table_cache.insert((tag, data.clone()), off);
                    table_data_block.extend(&data);

                    off
                }
            } else {
                while table_data_block.len() % 4 != 0 {
                    table_data_block.push(0);
                }

                let off = table_data_block.len() as u32;
                table_data_block.extend(&data);

                off
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&record.checksum().to_be_bytes());
            out.extend_from_slice(&rel_offset.to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        }
    }

    // Fix up absolute offsets

    let data_block_start = out.len() as u32;

    for (i, &off) in font_offsets.iter().enumerate() {
        let pos = offset_table_start + (i * 4);
        out[pos..pos + 4].copy_from_slice(&off.to_be_bytes());
    }

    for &f_off in &font_offsets {
        let num_tables = u16::from_be_bytes(
            out[f_off as usize + 4..f_off as usize + 6]
                .try_into()
                .into_diagnostic()?,
        );

        for i in 0..num_tables {
            let off_pos = (f_off as usize + 12) + (i as usize * 16) + 8;
            let rel = u32::from_be_bytes(out[off_pos..off_pos + 4].try_into().into_diagnostic()?);
            out[off_pos..off_pos + 4].copy_from_slice(&(data_block_start + rel).to_be_bytes());
        }
    }

    out.extend(table_data_block);

    Ok(out)
}

pub fn get_family_name(font: &FontRef) -> Option<String> {
    let name_table = font.name().ok()?;

    // Attempt to find Name ID 1 (Family Name)
    name_table
        .name_record()
        .iter()
        .filter(|rec| rec.name_id() == NameId::FAMILY_NAME)
        .find_map(|rec| {
            // Convert the raw bytes to a String
            let name = rec.string(name_table.string_data()).ok()?;

            Some(name.to_string())
        })
}
