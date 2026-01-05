use std::sync::Mutex;

use fontcull_read_fonts::{FontRef, TableProvider};
use kurbo::{BezPath, Shape};
use miette::{IntoDiagnostic, Result, WrapErr};
use romaji::RomajiExt;

use super::{HIRAGANA_RANGE, KATAKANA_RANGE, RubyPosition, RubyRenderer, utils};

pub struct RomajiRenderer<'a> {
    font: FontRef<'a>,
    upem: f64,
    scale_ratio: f64,
    gutter_em: f64,
    position: RubyPosition,
    baseline_offset_em: f64,
    tight: bool,
    cached_top_target: Mutex<Option<f64>>,
    cached_bottom_target: Mutex<Option<f64>>,
}

impl<'a> RomajiRenderer<'a> {
    pub fn new(
        font: FontRef<'a>,
        scale_ratio: f64,
        gutter_em: f64,
        position: RubyPosition,
        baseline_offset_em: f64,
        tight: bool,
    ) -> Result<Self> {
        let upem = font.head().into_diagnostic()?.units_per_em() as f64;

        Ok(Self {
            font,
            upem,
            scale_ratio,
            gutter_em,
            position,
            baseline_offset_em,
            tight,
            cached_top_target: Mutex::new(None),
            cached_bottom_target: Mutex::new(None),
        })
    }
}

impl<'a> RubyRenderer for RomajiRenderer<'a> {
    fn annotate(
        &self,
        ch: char,
        final_path: &mut BezPath,
        orig_advance: f64,
        main_upem: f64,
    ) -> Result<()> {
        let kana = ch.to_string();

        // Use `romaji` crate to convert the character to romaji.
        let romaji_text = kana.to_romaji();
        if romaji_text.is_empty() || kana == romaji_text || kana == "-" {
            return Ok(());
        }

        let hmtx = self
            .font
            .hmtx()
            .into_diagnostic()
            .wrap_err("Missing romaji font hmtx")?;

        let glyph_paths = match utils::collect_glyph_paths(&self.font, romaji_text) {
            Some(p) => p,
            None => return Ok(()),
        };

        let p_scale_factor = (self.scale_ratio * main_upem) / self.upem;

        let parts_widths = utils::compute_glyph_widths(
            &glyph_paths,
            p_scale_factor,
            |pgid: fontcull_skrifa::GlyphId| {
                hmtx.h_metrics()
                    .get(pgid.to_u32() as usize)
                    .map(|m| m.advance.get())
                    .unwrap_or(self.upem as u16) as f64
            },
        );

        match self.position {
            RubyPosition::Top | RubyPosition::Bottom => {
                utils::render_top_bottom(
                    final_path,
                    glyph_paths,
                    &parts_widths,
                    p_scale_factor,
                    main_upem,
                    orig_advance,
                    self.position,
                    self.gutter_em,
                    self.baseline_offset_em,
                    self.tight,
                    &self.cached_top_target,
                    &self.cached_bottom_target,
                    |pgid: fontcull_skrifa::GlyphId| {
                        hmtx.h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(self.upem as u16) as f64
                    },
                );
            }
            RubyPosition::LeftDown
            | RubyPosition::LeftUp
            | RubyPosition::RightDown
            | RubyPosition::RightUp => {
                let bbox = final_path.bounding_box();
                let center_y = (bbox.y0 + bbox.y1) / 2.0;

                utils::render_side(
                    final_path,
                    &glyph_paths,
                    p_scale_factor,
                    main_upem,
                    orig_advance,
                    self.position,
                    self.gutter_em,
                    center_y,
                    &mut |pgid: fontcull_skrifa::GlyphId| {
                        hmtx.h_metrics()
                            .get(pgid.to_u32() as usize)
                            .map(|m| m.advance.get())
                            .unwrap_or(self.upem as u16) as f64
                    },
                );
            }
        }

        Ok(())
    }

    fn ranges(&self) -> &[std::ops::RangeInclusive<u32>] {
        &[HIRAGANA_RANGE, KATAKANA_RANGE]
    }
}
