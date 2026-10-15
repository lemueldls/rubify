use std::ops::RangeInclusive;

use anyhow::Result;
use fontcull_read_fonts::FontRef;
use kurbo::BezPath;

use super::{
    BaselineTargets, HIRAGANA_RANGE, KATAKANA_RANGE, RubyPosition, RubyRenderer, utils::RubyPainter,
};

/// Hiragana U+3041..U+3096 sit exactly 0x60 below their katakana.
const KATAKANA_OFFSET: u32 = 0x60;

pub struct HiraganaRenderer<'a> {
    painter: RubyPainter<'a>,
}

impl<'a> HiraganaRenderer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        font: FontRef<'a>,
        scale_ratio: f64,
        gutter_em: f64,
        position: RubyPosition,
        baseline_offset_em: f64,
        fit: bool,
        fit_floor: f64,
        tight: bool,
    ) -> Result<Self> {
        Ok(Self {
            painter: RubyPainter::new(
                font,
                scale_ratio,
                gutter_em,
                position,
                baseline_offset_em,
                fit,
                fit_floor,
                tight,
            )?,
        })
    }
}

/// Convert katakana to hiragana (the prolonged sound mark stays as is).
pub fn to_hiragana(katakana: &str) -> String {
    katakana
        .chars()
        .map(|c| {
            let cp = c as u32;

            if (0x30a1..=0x30f6).contains(&cp) {
                char::from_u32(cp - KATAKANA_OFFSET).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

impl RubyRenderer for HiraganaRenderer<'_> {
    fn reading_for(&self, _ch: char) -> Option<String> {
        // A lone kanji has no reliable reading, so nothing is annotated
        // per character: hiragana ruby comes from words (the dictionary or a
        // user supplied word list).
        None
    }

    fn prepare_text(&self, reading: &str, _following: &str) -> Option<String> {
        if reading.is_empty() {
            return None;
        }

        Some(to_hiragana(reading))
    }

    fn required_targets(
        &self,
        reading: &str,
        following: &str,
        base_top: f64,
        base_bottom: f64,
        main_upem: f64,
    ) -> Result<BaselineTargets> {
        let Some(text) = self.prepare_text(reading, following) else {
            return Ok(BaselineTargets::default());
        };

        Ok(self
            .painter
            .required_targets(&text, base_top, base_bottom, main_upem)
            .unwrap_or_default())
    }

    fn draw_reading(
        &self,
        reading: &str,
        following: &str,
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        targets: &BaselineTargets,
    ) -> Result<()> {
        if let Some(text) = self.prepare_text(reading, following) {
            self.painter
                .draw(&text, final_path, span_x, span_advance, main_upem, targets);
        }

        Ok(())
    }

    fn can_draw_text(&self, text: &str) -> bool {
        self.painter.collect(text).is_some()
    }

    fn ranges(&self) -> &[RangeInclusive<u32>] {
        &[HIRAGANA_RANGE, KATAKANA_RANGE]
    }
}
