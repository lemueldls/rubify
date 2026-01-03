#[cfg(feature = "pinyin")]
pub mod pinyin;

use anyhow::Result;
use kurbo::BezPath;

/// A pluggable renderer that can add "ruby" annotations (small text above characters).
/// Implementations (such as pinyin) will be provided behind features.
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
}
