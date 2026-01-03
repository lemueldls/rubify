use ::pinyin::ToPinyin;
use anyhow::{Context, Result};
use kurbo::{Affine, BezPath, Shape};
use read_fonts::{FontRef, TableProvider};
use skrifa::MetadataProvider;

use crate::{PathPen, ruby::RubyRenderer};

pub struct PinyinRenderer<'a> {
    font: FontRef<'a>,
    upem: f64,
    scale_ratio: f64,
}

impl<'a> PinyinRenderer<'a> {
    pub fn new(font: FontRef<'a>, scale_ratio: f64, _main_upem: f64) -> Result<Self> {
        let upem = font.head()?.units_per_em() as f64;

        Ok(Self {
            font,
            upem,
            scale_ratio,
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
            let mut pinyin_paths: Vec<(skrifa::GlyphId, BezPath)> = Vec::new();
            let mut all_found = true;

            let cmap = self.font.charmap();
            let outlines = self.font.outline_glyphs();
            let hmtx = self.font.hmtx().context("Missing pinyin font hmtx")?;

            for pc in pinyin_text.chars() {
                match cmap.map(pc) {
                    Some(pgid) if pgid.to_u32() != 0 => {
                        if let Some(pglyph) = outlines.get(pgid) {
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
                // scale factor relative to the pinyin font's UPEM
                let p_scale_factor = (self.scale_ratio * main_upem) / self.upem;

                let mut total_pinyin_width = 0.0;

                for (pgid, _) in &pinyin_paths {
                    let adv = hmtx
                        .h_metrics()
                        .get(pgid.to_u32() as usize)
                        .map(|m| m.advance.get())
                        .unwrap_or(self.upem as u16) as f64;

                    total_pinyin_width += adv * p_scale_factor;
                }

                let bbox = final_path.bounding_box();
                let target_y = bbox.y1 + (main_upem * 0.1); // 10% EM padding
                let mut current_x = (orig_advance - total_pinyin_width) / 2.0;

                for (pgid, mut p_path) in pinyin_paths {
                    let adv = hmtx
                        .h_metrics()
                        .get(pgid.to_u32() as usize)
                        .map(|m| m.advance.get())
                        .unwrap_or(self.upem as u16) as f64;

                    let xform =
                        Affine::translate((current_x, target_y)) * Affine::scale(p_scale_factor);

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

        Ok(())
    }
}
