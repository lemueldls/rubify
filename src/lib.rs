use std::{collections::HashMap, ops::RangeInclusive};

use anyhow::{Context, Result, anyhow};
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
// Imports for subsetting and woff2
use fontcull_read_fonts as frf;
use fontcull_read_fonts::collections::int_set::IntSet;
use kurbo::BezPath;
use read_fonts::{
    FileRef, FontRef, TableProvider,
    types::{GlyphId, Tag},
};

pub mod renderer;
use skrifa::{MetadataProvider, outline::OutlinePen};
use woofwoof;
use write_fonts::{
    FontBuilder,
    from_obj::ToOwnedObj,
    tables::glyf::{GlyfLocaBuilder, Glyph as WriteGlyph, SimpleGlyph as WriteSimpleGlyph},
};

use crate::renderer::RubyRenderer;

// CJK
const CJK_RANGE: RangeInclusive<u32> = 0x4e00..=0x9fff;
// ASCII (Basic Latin)
const ASCII_RANGE: RangeInclusive<u32> = 0x0020..=0x007e;
// Pinyin chars (generic approach: just include all Latin Extended A/B which covers pinyin)
const LATIN_EXTENDED_RANGE: RangeInclusive<u32> = 0x0080..=0x024f;
// Also Combining Diacritical Marks: 0300–036F
const COMBINING_DIACRITICS_RANGE: RangeInclusive<u32> = 0x0300..=0x036f;

const TAG_GLYF: Tag = Tag::new(b"glyf");
const TAG_LOCA: Tag = Tag::new(b"loca");
const TAG_HEAD: Tag = Tag::new(b"head");

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

const TAG_NAME: Tag = Tag::new(b"name");

pub fn process_font_file(
    font_data: &[u8],
    renderers: Vec<impl RubyRenderer>,
    display_name: Option<&str>,
) -> Result<Vec<u8>> {
    let file =
        FileRef::new(font_data).map_err(|e| anyhow!("Failed to parse input font file: {:?}", e))?;

    let fonts: Vec<_> = file.fonts().collect();
    if fonts.is_empty() {
        return Err(anyhow!("No fonts found in input file"));
    }

    let font_ref = fonts[0]
        .clone()
        .map_err(|e| anyhow!("Failed to load first font: {:?}", e))?;

    process_single_font(font_ref, renderers, display_name)
}

fn process_single_font(
    font: FontRef,
    renderers: Vec<impl RubyRenderer>,
    display_name: Option<&str>,
) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx().context("Missing hmtx")?;
    let maxp = font.maxp().context("Missing maxp")?;
    let outlines = font.outline_glyphs();
    let num_glyphs = maxp.num_glyphs();
    let upem = font.head()?.units_per_em() as f64;

    let mut gid_to_char: HashMap<GlyphId, char> = HashMap::new();

    for c_u32 in CJK_RANGE {
        if let Some(c) = std::char::from_u32(c_u32) {
            let gid = charmap.map(c);
            if let Some(gid) = gid {
                if gid.to_u32() != 0 {
                    gid_to_char.insert(GlyphId::new(gid.to_u32()), c);
                }
            }
        }
    }

    let mut new_glyphs = Vec::new();

    for gid_u16 in 0..num_glyphs {
        let gid = GlyphId::new(gid_u16 as u32);

        let mut final_path = BezPath::new();
        let mut has_content = false;

        if let Some(glyph) = outlines.get(skrifa::GlyphId::new(gid.to_u32())) {
            let mut pen = PathPen::new();

            match glyph.draw(skrifa::instance::Size::unscaled(), &mut pen) {
                Ok(_) => {
                    final_path = pen.path;
                    has_content = true;
                }
                Err(_) => {}
            }
        }

        if let Some(&ch) = gid_to_char.get(&gid) {
            for renderer in &renderers {
                let orig_advance = hmtx
                    .h_metrics()
                    .get(gid.to_u32() as usize)
                    .map(|m| m.advance.get())
                    .unwrap_or(upem as u16) as f64;

                renderer
                    .annotate(ch, &mut final_path, orig_advance, upem)
                    .context("Failed to annotate")?;
            }
        }

        let write_glyph = if !has_content && final_path.elements().is_empty() {
            WriteGlyph::Empty
        } else {
            match WriteSimpleGlyph::from_bezpath(&final_path) {
                Ok(s) => WriteGlyph::Simple(s),
                Err(_) => WriteGlyph::Empty,
            }
        };

        new_glyphs.push(write_glyph);
    }

    let mut builder = FontBuilder::new();
    let mut glyf_loca_builder = GlyfLocaBuilder::new();

    for glyph in new_glyphs {
        let _ = glyf_loca_builder.add_glyph(&glyph);
    }

    let (glyf_data, loca_data, loca_fmt) = glyf_loca_builder.build();

    for record in font.table_directory.table_records() {
        let tag = record.tag();

        // Skip glyf/loca - we'll insert rebuilt data later
        if tag == TAG_GLYF || tag == TAG_LOCA {
            continue;
        }

        // Also skip name if we plan to override it
        if tag == TAG_NAME && display_name.is_some() {
            continue;
        }

        if tag == TAG_HEAD {
            if let Ok(head) = font.head() {
                let mut head: write_fonts::tables::head::Head = head.to_owned_obj(font_file_data);
                head.index_to_loc_format = loca_fmt as i16;
                head.checksum_adjustment = 0;
                let _ = builder.add_table(&head);
            }

            continue;
        }
        if let Some(data) = font.data_for_tag(tag) {
            builder.add_raw(tag, data.as_bytes().to_vec());
        }
    }

    let _ = builder.add_table(&glyf_data);
    let _ = builder.add_table(&loca_data);

    // If requested, override the 'name' table with the supplied display name.
    if let Some(name) = display_name {
        let name_bytes = make_name_table(name);
        let _ = builder.add_raw(TAG_NAME, name_bytes);
    }

    Ok(builder.build())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RubyCharacters {
    Pinyin,
}

pub fn subset_by_sets(
    font_data: &[u8],
    sets: &std::collections::HashSet<RubyCharacters>,
) -> Result<Vec<u8>> {
    let file =
        frf::FileRef::new(font_data).map_err(|_| anyhow!("Failed to parse font for subsetting"))?;
    let font = file
        .fonts()
        .next()
        .context("No font found for subsetting")?
        .map_err(|e| anyhow!("Read error: {:?}", e))?;

    // Build unicodes set based on provided character sets
    let mut unicodes = IntSet::<u32>::empty();

    if sets.contains(&RubyCharacters::Pinyin) {
        for c in CJK_RANGE {
            unicodes.insert(c);
        }
    }

    use fontcull_font_types::{GlyphId as FrfGlyphId, NameId as FrfNameId, Tag as FrfTag};

    let glyph_ids = IntSet::<FrfGlyphId>::empty();
    let drop_tables = IntSet::<FrfTag>::empty();
    let no_subset_tables = IntSet::<FrfTag>::empty();
    let passthrough_tables = IntSet::<FrfTag>::empty();
    let name_ids = IntSet::<FrfNameId>::empty();
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

    subset_font(&font, &plan).map_err(|e| anyhow!("Subset error: {:?}", e))
}

pub fn convert_to_woff2(font_data: &[u8]) -> Result<Vec<u8>> {
    woofwoof::compress(font_data, &[], 11, true).ok_or_else(|| anyhow!("WOFF2 compression failed"))
}
