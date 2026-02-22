#[cfg(feature = "pinyin")]
pub mod pinyin;
#[cfg(feature = "romaji")]
pub mod romaji;

pub mod utils;

use std::ops::RangeInclusive;

use anyhow::Result;
use facet::Facet;
use kurbo::BezPath;

/// A pluggable renderer that can add "ruby" annotations.
pub trait RubyRenderer: Send + Sync {
    /// Given a base character `ch`, add annotation paths (if any) into `final_path`.
    /// `orig_advance` is the glyph advance in font units; `main_upem` is the main font UPEM.
    fn annotate(
        &self,
        ch: char,
        final_path: &mut BezPath,
        orig_advance: f64,
        main_upem: f64,
    ) -> Result<()>;

    /// Returns the character ranges that this renderer can annotate.
    fn ranges(&self) -> &[RangeInclusive<u32>];
}

/// Positioning options for ruby annotations relative to the base glyph.
#[derive(Facet, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RubyPosition {
    Top,
    Bottom,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
}

const CJK_RANGE: RangeInclusive<u32> = 0x4e00..=0x9fff;
const HIRAGANA_RANGE: RangeInclusive<u32> = 0x3040..=0x309f;
const KATAKANA_RANGE: RangeInclusive<u32> = 0x30a0..=0x30ff;
