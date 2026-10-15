use std::ops::RangeInclusive;

use ::pinyin::ToPinyin;
use anyhow::Result;
use fontcull_read_fonts::FontRef;
use kurbo::BezPath;

use super::{
    BaselineTargets, CJK_EXT_A_RANGE, CJK_RANGE, RubyPosition, RubyRenderer, utils::RubyPainter,
};

pub struct PinyinRenderer<'a> {
    painter: RubyPainter<'a>,
}

impl<'a> PinyinRenderer<'a> {
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

impl RubyRenderer for PinyinRenderer<'_> {
    fn reading_for(&self, ch: char) -> Option<String> {
        ch.to_pinyin().map(|p| p.with_tone().to_string())
    }

    fn prepare_text(&self, reading: &str, _following: &str) -> Option<String> {
        if reading.is_empty() {
            None
        } else {
            Some(reading.to_string())
        }
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
        &[CJK_RANGE, CJK_EXT_A_RANGE]
    }
}
