pub mod renderer;

use std::{collections::HashMap, ops::RangeInclusive};

use fontcull_font_types::{NameId, TTC_HEADER_TAG};
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    CollectionRef, FileRef, FontRead, FontRef, TTCHeader, TableProvider, TopLevelTable,
    collections::IntSet,
    types::{GlyphId, Tag},
};
use fontcull_skrifa::{MetadataProvider, outline::OutlinePen};
use fontcull_write_fonts::{
    FontBuilder,
    from_obj::{ToOwnedObj, ToOwnedTable},
    tables::{
        glyf::{Glyf, GlyfLocaBuilder, Glyph, SimpleGlyph},
        head::Head,
        loca::Loca,
        name::Name,
    },
};
use indicatif::{ParallelProgressIterator, ProgressIterator, ProgressStyle};
use kurbo::BezPath;
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use rayon::iter::{ParallelBridge, ParallelIterator};
use rustc_hash::FxHashMap;
use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use woofwoof;

use crate::renderer::RubyRenderer;

pub fn process_font_file(
    file: FileRef,
    char_ranges: &[RangeInclusive<u32>],
    renderers: &[Box<dyn RubyRenderer>],
    subset: bool,
    display_name: Option<&str>,
) -> Result<Vec<u8>> {
    match file {
        FileRef::Font(font) => {
            // process_single_font(font, char_ranges, renderers, subset, display_name)
            let data = process_single_font(font, char_ranges, renderers, subset, display_name)?;
            let font = FontRef::new(&data).into_diagnostic()?;
            build_ttc_safe(&[font])
        }
        FileRef::Collection(collection) => {
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

                    let data =
                        process_single_font(font, char_ranges, renderers, subset, display_name)?;
                    let data = Box::leak(data.into_boxed_slice());

                    FontRef::new(data).into_diagnostic()
                })
                .collect::<Result<Vec<FontRef>>>()?;

            drop(process_span_enter);

            build_ttc_safe(&fonts)
        }
    }
}

fn process_single_font(
    font: FontRef,
    char_ranges: &[RangeInclusive<u32>],
    renderers: &[Box<dyn RubyRenderer>],
    subset: bool,
    display_name: Option<&str>,
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

        // Also skip name if we plan to override it
        if tag == Name::TAG && display_name.is_some() {
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

    // If requested, override the 'name' table with the supplied display name.
    if let Some(name) = display_name {
        let name_bytes = make_name_table(name);
        font_builder.add_raw(Name::TAG, name_bytes);
    }

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

fn make_name_table(display_name: &str) -> Vec<u8> {
    // Build a simple 'name' table with two Windows (platform 3) UTF-16BE records
    let utf16: Vec<u8> = display_name
        .encode_utf16()
        .flat_map(|u| u.to_be_bytes().to_vec())
        .collect();

    let len = utf16.len() as u16;
    let mut table = Vec::new();

    // format (u16) = 0, count (u16) = 2, stringOffset (u16)
    let count: u16 = 2;
    let string_offset: u16 = 6 + 12 * count; // header (6) + 12 bytes per record

    table.extend_from_slice(&0u16.to_be_bytes());
    table.extend_from_slice(&count.to_be_bytes());
    table.extend_from_slice(&string_offset.to_be_bytes());

    // Record 1: platformID=3 (Windows), encodingID=1 (UTF-16), lang=0x0409 (en-US), nameID=1 (Font Family)
    table.extend_from_slice(&3u16.to_be_bytes()); // platform
    table.extend_from_slice(&1u16.to_be_bytes()); // encoding
    table.extend_from_slice(&0x0409u16.to_be_bytes()); // language
    table.extend_from_slice(&1u16.to_be_bytes()); // nameID
    table.extend_from_slice(&len.to_be_bytes()); // length
    table.extend_from_slice(&0u16.to_be_bytes()); // offset

    // Record 2: same but nameID=4 (Full font name), offset = len of first
    table.extend_from_slice(&3u16.to_be_bytes()); // platform
    table.extend_from_slice(&1u16.to_be_bytes()); // encoding
    table.extend_from_slice(&0x0409u16.to_be_bytes()); // language
    table.extend_from_slice(&4u16.to_be_bytes()); // nameID
    table.extend_from_slice(&len.to_be_bytes()); // length
    table.extend_from_slice(&len.to_be_bytes()); // offset (after first string)

    // Append strings: first the family name, then the full name (we use the same value)
    table.extend_from_slice(&utf16);
    table.extend_from_slice(&utf16);

    table
}

fn make_ttc_header_table(num_fonts: u32) -> Vec<u8> {
    let mut table = Vec::new();

    // TTC Header
    table.extend_from_slice(b"ttcf"); // tag
    table.extend_from_slice(&0x00010000u32.to_be_bytes()); // version 1.0
    table.extend_from_slice(&num_fonts.to_be_bytes()); // numFonts

    // Offset table for each font (we'll fill with zeros for now)
    for _ in 0..num_fonts {
        table.extend_from_slice(&0u32.to_be_bytes());
    }

    table
}

pub fn combine_to_ttc(all_fonts: Vec<FontRef>) -> Result<Vec<u8>> {
    // let mut all_fonts = Vec::new();

    // // 1. Flatten all inputs into individual FontRefs
    // for font in collection {
    //     if let Ok(ttc_header) = TTCHeader::read(font.data()) {
    //         // It's a collection, extract each font
    //         for i in 0..ttc_header.num_fonts() {
    //             if let Ok(font) = collection.get(i) {
    //                 all_fonts.push(font);
    //             }
    //         }
    //     } else {
    //         // It's a single font
    //         all_fonts.push(font);
    //     }
    // }

    // 2. Setup output buffer and TTC Header (Version 1.0)
    let mut out = Vec::new();
    out.extend_from_slice(b"ttcf");
    out.extend_from_slice(&1u16.to_be_bytes()); // Major
    out.extend_from_slice(&0u16.to_be_bytes()); // Minor
    out.extend_from_slice(&(all_fonts.len() as u32).to_be_bytes());

    let offset_table_start = out.len();
    for _ in 0..all_fonts.len() {
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    let mut font_offsets = Vec::new();
    let mut table_cache: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut table_data_block = Vec::new();

    // 3. Process each font
    for font in all_fonts {
        font_offsets.push(out.len() as u32);

        let records = font.table_directory().table_records();
        let num_tables = records.len() as u16;

        // sfntVersion and Directory Header
        out.extend_from_slice(&0x00010000u32.to_be_bytes());
        out.extend_from_slice(&num_tables.to_be_bytes());
        // Simple helper for search metrics (or hardcode/calculate as shown previously)
        let entry_selector = (num_tables as f32).log2().floor() as u16;
        let search_range = (2u16.pow(entry_selector as u32)) * 16;
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&entry_selector.to_be_bytes());
        out.extend_from_slice(&(num_tables * 16 - search_range).to_be_bytes());

        for record in records {
            let tag = record.tag();
            let data = font.table_data(tag).unwrap().as_ref().to_vec();

            // Shared table deduplication
            let relative_offset = if let Some(&existing_rel_offset) = table_cache.get(&data) {
                existing_rel_offset
            } else {
                let new_rel_offset = table_data_block.len() as u32;
                table_cache.insert(data.clone(), new_rel_offset);

                // 4-byte alignment padding
                while table_data_block.len() % 4 != 0 {
                    table_data_block.push(0);
                }
                table_data_block.extend(data);
                new_rel_offset
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&record.checksum().to_be_bytes());
            out.extend_from_slice(&relative_offset.to_be_bytes()); // Temp relative offset
            out.extend_from_slice(&(record.length()).to_be_bytes());
        }
    }

    // 4. Final Patching
    let data_block_start = out.len() as u32;

    // Patch Font Directory Offsets in Header
    for (i, &off) in font_offsets.iter().enumerate() {
        let pos = offset_table_start + (i * 4);
        out[pos..pos + 4].copy_from_slice(&off.to_be_bytes());
    }

    // Patch Absolute Table Offsets in Directories
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

pub fn build_ttc(fonts: &[FontRef]) -> Result<Vec<u8>> {
    let mut out = Vec::new();

    // 1. Write TTC Header (Version 1.0)
    out.extend_from_slice(b"ttcf"); // Tag
    out.extend_from_slice(&1u16.to_be_bytes()); // Major Version
    out.extend_from_slice(&0u16.to_be_bytes()); // Minor Version
    out.extend_from_slice(&(fonts.len() as u32).to_be_bytes()); // Num fonts

    // Placeholder for offsets (to be filled later)
    let offset_table_start = out.len();
    for _ in 0..fonts.len() {
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    let mut font_offsets = Vec::new();
    let mut table_cache: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut table_data_block = Vec::new();

    // 2. Process each font to build its Table Directory
    for font in fonts {
        font_offsets.push(out.len() as u32);

        let dir = font.table_directory();
        let records = dir.table_records();

        // Write OffsetTable (searchRange, entrySelector, rangeShift based on record count)
        let num_tables = records.len() as u16;
        let entry_selector = (num_tables as f32).log2().floor() as u16;
        let search_range = (2u16.pow(entry_selector as u32)) * 16;
        let range_shift = num_tables * 16 - search_range;

        out.extend_from_slice(&0x00010000u32.to_be_bytes()); // sfntVersion (TrueType)
        out.extend_from_slice(&num_tables.to_be_bytes());
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&entry_selector.to_be_bytes());
        out.extend_from_slice(&range_shift.to_be_bytes());

        // 3. Write Table Records and collect data
        for record in records {
            let tag = record.tag();
            let data = font
                .table_data(tag)
                .ok_or_else(|| miette!("Missing table data for tag {:?}", tag))?
                // .ok_or("Missing table data")?
                .as_ref()
                .to_vec();
            let checksum = record.checksum(); // Re-use existing checksum
            let length = data.len() as u32;

            // Deduplication: check if we already have this exact table data
            let table_offset = if let Some(&existing_offset) = table_cache.get(&data) {
                existing_offset
            } else {
                // New table: record its future offset relative to the start of the file
                // We'll calculate the final offset after we know where the data block starts
                let new_offset = table_data_block.len() as u32;
                table_cache.insert(data.clone(), new_offset);

                let start_padding = (4 - (table_data_block.len() % 4)) % 4;
                table_data_block.extend(std::iter::repeat(0).take(start_padding));
                table_data_block.extend(data);
                new_offset
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&checksum.to_be_bytes());
            // Store temporary relative offset; we will fix this in a final pass
            out.extend_from_slice(&table_offset.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
        }
    }

    // 4. Fix up offsets
    let data_block_start = out.len() as u32;

    // Fix Font Offsets in the TTC Header
    for (i, &offset) in font_offsets.iter().enumerate() {
        let pos = offset_table_start + (i * 4);
        out[pos..pos + 4].copy_from_slice(&offset.to_be_bytes());
    }

    // Fix Table Offsets in each Font's Directory
    // This requires iterating back through the written 'out' buffer.
    // Each record is 16 bytes: Tag(4), Checksum(4), Offset(4), Length(4).
    // The Offset starts 12 bytes after the start of the Font's directory.
    for &f_offset in &font_offsets {
        let num_tables = u16::from_be_bytes(
            out[f_offset as usize + 4..f_offset as usize + 6]
                .try_into()
                .into_diagnostic()?,
        );
        for i in 0..num_tables {
            let record_pos = (f_offset as usize + 12) + (i as usize * 16);
            let rel_offset = u32::from_be_bytes(
                out[record_pos + 8..record_pos + 12]
                    .try_into()
                    .into_diagnostic()?,
            );
            let abs_offset = data_block_start + rel_offset;
            out[record_pos + 8..record_pos + 12].copy_from_slice(&abs_offset.to_be_bytes());
        }
    }

    // 5. Append the actual table data
    out.extend(table_data_block);

    Ok(out)
}

pub fn build_ttc_safe(fonts: &[FontRef]) -> Result<Vec<u8>> {
    let mut out = Vec::new();

    // 1. TTC Header
    out.extend_from_slice(b"ttcf");
    out.extend_from_slice(&1u16.to_be_bytes()); // Major
    out.extend_from_slice(&0u16.to_be_bytes()); // Minor
    out.extend_from_slice(&(fonts.len() as u32).to_be_bytes());

    let offset_table_start = out.len();
    for _ in 0..fonts.len() {
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    let mut font_offsets = Vec::new();
    // Key = (Table Tag, Table Bytes), Value = Offset in table_data_block
    // We include the Tag in the key to ensure we don't accidentally share
    // data between different types of tables.
    let mut table_cache: HashMap<(Tag, Vec<u8>), u32> = HashMap::new();
    let mut table_data_block = Vec::new();

    // 2. Process Fonts
    for font in fonts {
        font_offsets.push(out.len() as u32);

        let records = font.table_directory().table_records();
        let num_tables = records.len() as u16;

        // Directory Header
        out.extend_from_slice(&0x00010000u32.to_be_bytes());
        out.extend_from_slice(&num_tables.to_be_bytes());

        let entry_selector = (num_tables as f32).log2().floor() as u16;
        let search_range = (2u16.pow(entry_selector as u32)) * 16;
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&entry_selector.to_be_bytes());
        out.extend_from_slice(&(num_tables * 16 - search_range).to_be_bytes());

        for record in records {
            let tag = record.tag();
            let data = font
                .table_data(tag)
                .ok_or_else(|| miette!("Data error"))?
                .as_ref()
                .to_vec();

            // Only deduplicate high-value tables to avoid metrics corruption
            let can_share = matches!(&tag.to_be_bytes(), b"glyf" | b"CFF " | b"CFF2");

            let relative_offset = if can_share {
                if let Some(&off) = table_cache.get(&(tag, data.clone())) {
                    off
                } else {
                    let off = table_data_block.len() as u32;
                    // Ensure 4-byte alignment for the next table
                    while table_data_block.len() % 4 != 0 {
                        table_data_block.push(0);
                    }
                    let aligned_off = table_data_block.len() as u32;
                    table_cache.insert((tag, data.clone()), aligned_off);
                    table_data_block.extend(data);
                    aligned_off
                }
            } else {
                // For metrics (hmtx, vmtx, kern) and identity tables (name, post),
                // always write them uniquely per font.
                while table_data_block.len() % 4 != 0 {
                    table_data_block.push(0);
                }
                let off = table_data_block.len() as u32;
                table_data_block.extend(data);
                off
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&record.checksum().to_be_bytes());
            out.extend_from_slice(&relative_offset.to_be_bytes());
            out.extend_from_slice(&(record.length()).to_be_bytes());
        }
    }

    // 3. Final Absolute Patching
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
