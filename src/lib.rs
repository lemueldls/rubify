use std::{collections::HashMap, ops::RangeInclusive};

use anyhow::{Context, Result, anyhow};
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
// Imports for subsetting and woff2
use fontcull_read_fonts as frf;
use fontcull_read_fonts::collections::int_set::IntSet;
use kurbo::{Affine, BezPath, Shape};
use pinyin::ToPinyin;
use read_fonts::{
    FileRef, FontRef, TableProvider,
    types::{GlyphId, Tag},
};
use skrifa::{MetadataProvider, outline::OutlinePen};
use woofwoof;
use write_fonts::{
    FontBuilder,
    from_obj::ToOwnedObj,
    tables::glyf::{GlyfLocaBuilder, Glyph as WriteGlyph, SimpleGlyph as WriteSimpleGlyph},
};

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

struct PathPen {
    path: BezPath,
}

impl PathPen {
    fn new() -> Self {
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

pub fn process_font_file(font_data: &[u8], pinyin_font_data: Option<&[u8]>) -> Result<Vec<u8>> {
    let file =
        FileRef::new(font_data).map_err(|e| anyhow!("Failed to parse input font file: {:?}", e))?;

    let fonts: Vec<_> = file.fonts().collect();
    if fonts.is_empty() {
        return Err(anyhow!("No fonts found in input file"));
    }

    let font_ref = fonts[0]
        .clone()
        .map_err(|e| anyhow!("Failed to load first font: {:?}", e))?;

    let pinyin_font_ref = if let Some(data) = pinyin_font_data {
        let pfile =
            FileRef::new(data).map_err(|e| anyhow!("Failed to parse pinyin font file: {:?}", e))?;
        let pfonts: Vec<_> = pfile.fonts().collect();

        if pfonts.is_empty() {
            return Err(anyhow!("No fonts found in pinyin font file"));
        }

        Some(
            pfonts[0]
                .clone()
                .map_err(|e| anyhow!("Failed to load first font from pinyin font file: {:?}", e))?,
        )
    } else {
        None
    };

    process_single_font(font_ref, pinyin_font_ref)
}

fn process_single_font(font: FontRef, pinyin_font: Option<FontRef>) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx().context("Missing hmtx")?;
    let maxp = font.maxp().context("Missing maxp")?;
    let outlines = font.outline_glyphs();
    let num_glyphs = maxp.num_glyphs();
    let upem = font.head()?.units_per_em() as f64;

    // Providers for pinyin (default to main font)
    let p_font_ref = pinyin_font.as_ref().unwrap_or(&font);
    let p_charmap = p_font_ref.charmap();
    let p_outlines = p_font_ref.outline_glyphs();
    let p_hmtx = p_font_ref.hmtx().context("Missing pinyin font hmtx")?;
    let p_upem = p_font_ref.head()?.units_per_em() as f64;

    // Normalizing scale factor:
    // We want pinyin to be 30% of main font's EM height.
    // Scale = (0.3 * MainUPEM) / PinyinUPEM
    let p_scale_factor = (0.3 * upem) / p_upem;

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
            if let Some(p) = ch.to_pinyin() {
                let pinyin_text = p.with_tone();
                let mut pinyin_paths: Vec<(skrifa::GlyphId, BezPath)> = Vec::new();
                let mut all_found = true;

                for pc in pinyin_text.chars() {
                    match p_charmap.map(pc) {
                        Some(pgid) if pgid.to_u32() != 0 => {
                            if let Some(pglyph) = p_outlines.get(pgid) {
                                let mut ppen = PathPen::new();

                                if pglyph
                                    .draw(skrifa::instance::Size::unscaled(), &mut ppen)
                                    .is_ok()
                                {
                                    pinyin_paths.push((pgid, ppen.path));
                                } else {
                                    all_found = false;

                                    break;
                                }
                            } else {
                                all_found = false;

                                break;
                            }
                        }
                        _ => {
                            all_found = false;

                            break;
                        }
                    }
                }

                if all_found && !pinyin_paths.is_empty() {
                    let orig_advance = hmtx
                        .h_metrics()
                        .get(gid.to_u32() as usize)
                        .map(|m| m.advance.get())
                        .unwrap_or(upem as u16) as f64;

                    let mut total_pinyin_width = 0.0;

                    for (pgid, _) in &pinyin_paths {
                        let adv = p_hmtx
                            .h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(p_upem as u16) as f64;

                        total_pinyin_width += adv * p_scale_factor;
                    }

                    let bbox = final_path.bounding_box();
                    let target_y = bbox.y1 + (upem * 0.1); // 10% EM padding
                    let mut current_x = (orig_advance - total_pinyin_width) / 2.0;

                    for (pgid, mut p_path) in pinyin_paths {
                        let adv = p_hmtx
                            .h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(p_upem as u16) as f64;
                        let xform = Affine::translate((current_x, target_y))
                            * Affine::scale(p_scale_factor);

                        p_path.apply_affine(xform);

                        for el in p_path.elements() {
                            match el {
                                kurbo::PathEl::MoveTo(p) => final_path.move_to(*p),
                                kurbo::PathEl::LineTo(p) => final_path.line_to(*p),
                                kurbo::PathEl::QuadTo(p1, p2) => final_path.quad_to(*p1, *p2),
                                kurbo::PathEl::CurveTo(p1, p2, p3) => {
                                    final_path.curve_to(*p1, *p2, *p3)
                                }
                                kurbo::PathEl::ClosePath => final_path.close_path(),
                            }
                        }

                        current_x += adv * p_scale_factor;
                    }
                }
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

        if tag == TAG_GLYF || tag == TAG_LOCA {
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

    Ok(builder.build())
}

pub fn subset_cjk(font_data: &[u8]) -> Result<Vec<u8>> {
    let file =
        frf::FileRef::new(font_data).map_err(|_| anyhow!("Failed to parse font for subsetting"))?;
    let font = file
        .fonts()
        .next()
        .context("No font found for subsetting")?
        .map_err(|e| anyhow!("Read error: {:?}", e))?;

    // Build unicodes set: CJK + ASCII + Pinyin
    let mut unicodes = IntSet::<u32>::empty();

    for c in CJK_RANGE {
        unicodes.insert(c);
    }
    for c in ASCII_RANGE {
        unicodes.insert(c);
    }
    for c in LATIN_EXTENDED_RANGE {
        unicodes.insert(c);
    }
    for c in COMBINING_DIACRITICS_RANGE {
        unicodes.insert(c);
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
