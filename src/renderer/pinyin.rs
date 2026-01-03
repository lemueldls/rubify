use ::pinyin::ToPinyin;
use anyhow::{Context, Result};
use kurbo::{Affine, BezPath, Shape};
use read_fonts::{FontRef, TableProvider};
use skrifa::MetadataProvider;

use crate::{PathPen, renderer::RubyRenderer};

pub struct PinyinRenderer<'a> {
    font: FontRef<'a>,
    upem: f64,
    /// fraction of main font size to use for the ruby font (e.g. 0.7 = 70%)
    scale_ratio: f64,
    /// gap (in em units) between the base glyph and the ruby text
    gutter_em: f64,
    /// optional delimiter char to split ruby text into parts
    delimiter: Option<char>,
    /// spacing between parts (in em units)
    spacing_em: f64,
}

impl<'a> PinyinRenderer<'a> {
    pub fn new(
        font: FontRef<'a>,
        scale_ratio: f64,
        gutter_em: f64,
        delimiter: Option<char>,
        spacing_em: f64,
    ) -> Result<Self> {
        let upem = font.head()?.units_per_em() as f64;

        Ok(Self {
            font,
            upem,
            scale_ratio,
            gutter_em,
            delimiter,
            spacing_em,
        })
    }
}

impl<'a> RubyRenderer for PinyinRenderer<'a> {
    fn annotate(
        &self,
        ch: char,
        final_path: &mut BezPath,
        orig_advance: f64,
        main_upem: f64,
    ) -> Result<()> {
        if let Some(p) = ch.to_pinyin() {
            let pinyin_text = p.with_tone();

            // split into parts if a delimiter is provided, otherwise treat the whole text as one part
            let parts: Vec<String> = if let Some(d) = self.delimiter {
                pinyin_text.split(d).map(|s| s.to_string()).collect()
            } else {
                vec![pinyin_text.to_string()]
            };

            if parts.is_empty() {
                return Ok(());
            }

            let cmap = self.font.charmap();
            let outlines = self.font.outline_glyphs();
            let hmtx = self.font.hmtx().context("Missing pinyin font hmtx")?;

            // For each part, collect its glyphs and their paths
            let mut parts_paths: Vec<Vec<(skrifa::GlyphId, BezPath)>> = Vec::new();
            let mut all_found = true;

            for part in &parts {
                let mut part_paths: Vec<(skrifa::GlyphId, BezPath)> = Vec::new();

                for pc in part.chars() {
                    match cmap.map(pc) {
                        Some(pgid) if pgid.to_u32() != 0 => {
                            if let Some(pglyph) = outlines.get(pgid) {
                                let mut ppen = PathPen::new();

                                if pglyph
                                    .draw(skrifa::instance::Size::unscaled(), &mut ppen)
                                    .is_ok()
                                {
                                    part_paths.push((pgid, ppen.path));
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

                if !all_found {
                    break;
                }

                parts_paths.push(part_paths);
            }

            if all_found && !parts_paths.is_empty() {
                // scale factor relative to the pinyin font's UPEM
                let p_scale_factor = (self.scale_ratio * main_upem) / self.upem;

                // width of each part (in final scaled units)
                let mut parts_widths: Vec<f64> = Vec::new();

                for part_paths in &parts_paths {
                    let mut part_width = 0.0;

                    for (pgid, _) in part_paths {
                        let adv = hmtx
                            .h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(self.upem as u16) as f64;

                        part_width += adv * p_scale_factor;
                    }

                    parts_widths.push(part_width);
                }

                let spacing_units = self.spacing_em * main_upem; // spacing between parts in font units
                let total_pinyin_width = parts_widths.iter().sum::<f64>()
                    + spacing_units * (parts_widths.len().saturating_sub(1) as f64);

                let bbox = final_path.bounding_box();
                // gutter is in ems; position y above the glyph bbox
                let target_y = bbox.y1 + (self.gutter_em * main_upem);
                let mut current_x = (orig_advance - total_pinyin_width) / 2.0;

                // render each part in order, separated by spacing
                for (i, part_paths) in parts_paths.into_iter().enumerate() {
                    for (pgid, mut p_path) in part_paths {
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

                        let adv = hmtx
                            .h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(self.upem as u16) as f64;

                        current_x += adv * p_scale_factor;
                    }

                    // after part, add spacing before next part (except after last)
                    if i + 1 < parts_widths.len() {
                        current_x += spacing_units;
                    }
                }
            }
        }

        Ok(())
    }
}
