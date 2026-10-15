#[cfg(feature = "pinyin")]
pub mod pinyin;
#[cfg(feature = "romaji")]
pub mod romaji;

pub mod hiragana;
pub mod utils;

use std::ops::RangeInclusive;

use anyhow::Result;
use facet::Facet;
use kurbo::BezPath;

/// Consistent baseline targets, in main font units.
///
/// `top` is the annotation baseline y (used for `RubyPosition::Top`) and
/// `bottom` the one for `RubyPosition::Bottom`. They are computed in a first
/// pass over every annotation so that all ruby in one font shares a baseline.
#[derive(Clone, Copy, Debug, Default)]
pub struct BaselineTargets {
    pub top: Option<f64>,
    pub bottom: Option<f64>,
}

impl BaselineTargets {
    pub fn merge(&mut self, other: BaselineTargets) {
        if let Some(top) = other.top {
            self.top = Some(self.top.map_or(top, |current| current.max(top)));
        }

        if let Some(bottom) = other.bottom {
            self.bottom = Some(self.bottom.map_or(bottom, |current| current.min(bottom)));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.top.is_none() && self.bottom.is_none()
    }
}

/// A pluggable renderer that can add "ruby" annotations.
pub trait RubyRenderer: Send + Sync {
    /// Reading text for a single base character, or `None` when this renderer
    /// has nothing to annotate on it.
    fn reading_for(&self, ch: char) -> Option<String>;

    /// Annotation text for a span reading (kana or pinyin). `following` holds
    /// the readings after this span in the same word, for conversion that
    /// depends on context (e.g. sokuon in romaji).
    fn prepare_text(&self, reading: &str, following: &str) -> Option<String>;

    /// Required baseline targets for drawing `reading` over a base span whose
    /// ink spans `base_top`..`base_bottom` (main font units).
    fn required_targets(
        &self,
        reading: &str,
        following: &str,
        base_top: f64,
        base_bottom: f64,
        main_upem: f64,
    ) -> Result<BaselineTargets>;

    /// Draw `reading` into `final_path`, centered over the span at
    /// `span_x` with width `span_advance`.
    #[allow(clippy::too_many_arguments)]
    fn draw_reading(
        &self,
        reading: &str,
        following: &str,
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        targets: &BaselineTargets,
    ) -> Result<()>;

    /// Whether the ruby font can draw an annotation text.
    fn can_draw_text(&self, text: &str) -> bool;

    /// Returns the character ranges that this renderer can annotate.
    fn ranges(&self) -> &[RangeInclusive<u32>];
}

/// Positioning options for ruby annotations relative to the base glyph.
#[derive(Facet, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RubyPosition {
    Top,
    Bottom,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
}

pub const CJK_RANGE: RangeInclusive<u32> = 0x4e00..=0x9fff;
pub const CJK_EXT_A_RANGE: RangeInclusive<u32> = 0x3400..=0x4dbf;
pub const HIRAGANA_RANGE: RangeInclusive<u32> = 0x3040..=0x309f;
pub const KATAKANA_RANGE: RangeInclusive<u32> = 0x30a0..=0x30ff;
