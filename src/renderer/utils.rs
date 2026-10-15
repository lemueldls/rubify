use std::sync::{Arc, Mutex, RwLock};

use fontcull_read_fonts::{FontRef, TableProvider};
use fontcull_skrifa::{
    GlyphId, MetadataProvider, OutlineGlyphCollection, charmap::Charmap, instance::Size,
};
use kurbo::{Affine, BezPath, Shape};
use rustc_hash::FxHashMap;

use crate::{
    PathPen,
    renderer::{BaselineTargets, RubyPosition},
};

/// A drawn ruby glyph: the glyph id and its outline, shared between clones.
pub type GlyphPath = Arc<(GlyphId, BezPath)>;

pub type GlyphPaths = Vec<GlyphPath>;

/// Cache of drawn ruby-font glyph paths, keyed by character.
///
/// The same characters (pinyin letters, romaji letters, kana) are drawn over
/// and over for every annotated base glyph; caching avoids re-parsing and
/// re-drawing them for each occurrence. Entries are shared, so handing them
/// out costs an `Arc` clone.
#[derive(Default)]
pub struct GlyphPathCache {
    by_char: RwLock<FxHashMap<char, Option<GlyphPath>>>,
}

fn draw_char(cmap: &Charmap, outlines: &OutlineGlyphCollection, pc: char) -> Option<GlyphPath> {
    let pgid = cmap.map(pc)?;

    if pgid.to_u32() == 0 {
        return None;
    }

    let pglyph = outlines.get(pgid)?;
    let mut ppen = PathPen::new();

    pglyph.draw(Size::unscaled(), &mut ppen).ok()?;

    Some(Arc::new((pgid, ppen.path)))
}

/// Collect glyph paths; returns `None` if any glyph cannot be found or drawn.
pub fn collect_glyph_paths(
    font: &FontRef,
    text: &str,
    cache: &GlyphPathCache,
) -> Option<GlyphPaths> {
    let mut glyph_paths: GlyphPaths = Vec::with_capacity(text.chars().count());

    for pc in text.chars() {
        // Fast path: most characters are already drawn.
        if let Some(entry) = cache.by_char.read().unwrap().get(&pc) {
            glyph_paths.push(entry.clone()?);
            continue;
        }

        let mut guard = cache.by_char.write().unwrap();
        let entry = guard
            .entry(pc)
            .or_insert_with(|| draw_char(&font.charmap(), &font.outline_glyphs(), pc));

        glyph_paths.push(entry.clone()?);
    }

    Some(glyph_paths)
}

/// Raw metrics of a rendered annotation text, in ruby font units.
pub struct TextMetrics {
    /// Advance of every glyph, in ruby font units.
    pub widths: Vec<f64>,
    /// Sum of `widths`.
    pub total: f64,
    /// Lowest ink of the glyphs.
    pub min_y: f64,
    /// Highest ink of the glyphs.
    pub max_y: f64,
}

/// Draws ruby text for one span of a base glyph.
///
/// Shared by every renderer: the renderer converts its reading notation to
/// text, and this painter handles placement, scaling and (optional) fitting.
pub struct RubyPainter<'a> {
    /// The font the ruby characters are taken from.
    font: FontRef<'a>,
    /// Units per em of the ruby font.
    pub upem: f64,
    /// Fraction of the main font size to use for the ruby font (0.4 = 40%).
    pub scale_ratio: f64,
    /// Gap (em units) between the base glyph and the ruby text.
    pub gutter_em: f64,
    /// Baseline offset in em units to fine tune the annotation baseline.
    pub baseline_offset_em: f64,
    /// Position of the ruby relative to the base glyph.
    pub position: RubyPosition,
    /// When true, condense overflowing ruby horizontally to fit its span.
    pub fit: bool,
    /// Lowest scale `fit` condenses to before letting the ruby overhang.
    pub fit_floor: f64,
    /// When true, use tight per-glyph placement instead of a shared baseline.
    pub tight: bool,
    /// Advances of the ruby font, indexed by glyph id.
    advances: Vec<f64>,
    /// Cache of drawn ruby glyph paths, keyed by character.
    cache: GlyphPathCache,
    /// Cache of rendered text metrics, keyed by the rendered text.
    metrics: Mutex<FxHashMap<String, Option<Arc<TextMetrics>>>>,
}

impl<'a> RubyPainter<'a> {
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
    ) -> anyhow::Result<Self> {
        let upem = font.head()?.units_per_em() as f64;

        // Advances are looked up per glyph, over and over; read them once.
        // Glyphs past `numberOfHMetrics` use the last advance, per the spec.
        let advances = match font.hmtx() {
            Ok(hmtx) => {
                let count = font.maxp().map(|maxp| maxp.num_glyphs()).unwrap_or(0) as usize;
                let h_metrics = hmtx.h_metrics();
                let default = h_metrics
                    .last()
                    .map(|m| m.advance.get() as f64)
                    .unwrap_or(upem);

                (0..count)
                    .map(|gid| {
                        h_metrics
                            .get(gid)
                            .map(|m| m.advance.get() as f64)
                            .unwrap_or(default)
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
        };

        Ok(Self {
            font,
            upem,
            scale_ratio,
            gutter_em,
            position,
            baseline_offset_em,
            fit,
            fit_floor,
            tight,
            advances,
            cache: GlyphPathCache::default(),
            metrics: Mutex::new(FxHashMap::default()),
        })
    }

    /// Scale factor from ruby font units to main font units.
    pub fn scale_factor(&self, main_upem: f64) -> f64 {
        (self.scale_ratio * main_upem) / self.upem
    }

    fn advance_of(&self, gid: GlyphId) -> f64 {
        self.advances
            .get(gid.to_u32() as usize)
            .copied()
            .unwrap_or(self.upem)
    }

    /// Collect glyph paths and advances for a text, plus the total width
    /// (all in ruby font units).
    pub fn collect(&self, text: &str) -> Option<(GlyphPaths, Vec<f64>, f64)> {
        let paths = collect_glyph_paths(&self.font, text, &self.cache)?;
        let widths: Vec<f64> = paths.iter().map(|path| self.advance_of(path.0)).collect();
        let total = widths.iter().sum();

        Some((paths, widths, total))
    }

    /// Metrics for a rendered text, cached by the text itself.
    fn text_metrics(&self, text: &str) -> Option<Arc<TextMetrics>> {
        if let Some(cached) = self.metrics.lock().unwrap().get(text) {
            return cached.clone();
        }

        let (paths, widths, total) = self.collect(text)?;

        let mut min_y: f64 = f64::INFINITY;
        let mut max_y: f64 = f64::NEG_INFINITY;

        for path in paths.iter() {
            for el in path.1.elements() {
                match el {
                    kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) => {
                        min_y = min_y.min(p.y);
                        max_y = max_y.max(p.y);
                    }
                    kurbo::PathEl::QuadTo(p1, p2) => {
                        min_y = min_y.min(p1.y).min(p2.y);
                        max_y = max_y.max(p1.y).max(p2.y);
                    }
                    kurbo::PathEl::CurveTo(p1, p2, p3) => {
                        min_y = min_y.min(p1.y).min(p2.y).min(p3.y);
                        max_y = max_y.max(p1.y).max(p2.y).max(p3.y);
                    }
                    kurbo::PathEl::ClosePath => {}
                }
            }
        }

        if !min_y.is_finite() {
            min_y = 0.0;
        }

        if !max_y.is_finite() {
            max_y = self.upem * 0.8;
        }

        let metrics = Arc::new(TextMetrics {
            widths,
            total,
            min_y,
            max_y,
        });

        self.metrics
            .lock()
            .unwrap()
            .insert(text.to_string(), Some(metrics.clone()));

        Some(metrics)
    }

    /// Baseline targets required for drawing `text` over a base span.
    pub fn required_targets(
        &self,
        text: &str,
        base_top: f64,
        base_bottom: f64,
        main_upem: f64,
    ) -> Option<BaselineTargets> {
        if !matches!(self.position, RubyPosition::Top | RubyPosition::Bottom) {
            return None;
        }

        let metrics = self.text_metrics(text)?;

        let p_scale = self.scale_factor(main_upem);
        let gutter = self.gutter_em * main_upem;
        let offset = self.baseline_offset_em * main_upem;

        let mut targets = BaselineTargets::default();

        match self.position {
            RubyPosition::Top => {
                targets.top = Some(base_top + gutter + offset - metrics.min_y * p_scale);
            }
            RubyPosition::Bottom => {
                targets.bottom = Some(base_bottom - gutter - offset - metrics.max_y * p_scale);
            }
            _ => {}
        }

        Some(targets)
    }

    /// Draw `text` centered over the span at `span_x` with width `span_advance`.
    pub fn draw(
        &self,
        text: &str,
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        targets: &BaselineTargets,
    ) -> bool {
        let Some(metrics) = self.text_metrics(text) else {
            return false;
        };

        let Some((paths, widths, _total)) = self.collect(text) else {
            return false;
        };

        if matches!(self.position, RubyPosition::Top | RubyPosition::Bottom) {
            self.draw_top_bottom(
                paths,
                &widths,
                &metrics,
                final_path,
                span_x,
                span_advance,
                main_upem,
                targets,
            );
        } else {
            self.draw_side(paths, &widths, final_path, span_advance, main_upem);
        }

        true
    }

    /// Horizontal scale applied to the ruby text to make it fit its span.
    fn x_scale(&self, total_width: f64, span_advance: f64) -> f64 {
        if !self.fit || span_advance <= 0.0 || total_width <= span_advance {
            return 1.0;
        }

        (span_advance / total_width).max(self.fit_floor.min(1.0))
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_top_bottom(
        &self,
        paths: GlyphPaths,
        widths: &[f64],
        metrics: &TextMetrics,
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        targets: &BaselineTargets,
    ) {
        let p_scale = self.scale_factor(main_upem);
        let total_width_units = metrics.total * p_scale;
        let x_scale = self.x_scale(total_width_units, span_advance);

        let bbox = final_path.bounding_box();
        let gutter = self.gutter_em * main_upem;
        let offset = self.baseline_offset_em * main_upem;

        let target_y = match self.position {
            RubyPosition::Top => targets
                .top
                .unwrap_or(bbox.y1 + gutter + offset - metrics.min_y * p_scale),
            RubyPosition::Bottom => targets
                .bottom
                .unwrap_or(bbox.y0 - gutter - offset - metrics.max_y * p_scale),
            _ => return,
        };

        let mut current_x = span_x + (span_advance - total_width_units * x_scale) / 2.0;

        for (path, width) in paths.iter().zip(widths) {
            let xform = Affine::translate((current_x, target_y))
                * Affine::scale_non_uniform(p_scale * x_scale, p_scale);

            append_transformed(final_path, &path.1, xform);

            current_x += width * p_scale * x_scale;
        }
    }

    /// Render side-positioned annotations (left/right, up/down stacking).
    fn draw_side(
        &self,
        paths: GlyphPaths,
        widths: &[f64],
        final_path: &mut BezPath,
        orig_advance: f64,
        main_upem: f64,
    ) {
        let p_scale = self.scale_factor(main_upem);

        if paths.is_empty() {
            return;
        }

        let bbox = final_path.bounding_box();
        let center_y = (bbox.y0 + bbox.y1) / 2.0;

        let max_glyph_width = widths.iter().map(|w| w * p_scale).fold(0.0f64, f64::max);
        let vertical_step = main_upem * p_scale * 0.8;
        let gutter_units = self.gutter_em * main_upem;

        let start_x = match self.position {
            RubyPosition::LeftDown | RubyPosition::LeftUp => -(max_glyph_width + gutter_units),
            _ => orig_advance + gutter_units,
        };

        let n = paths.len() as f64;
        let mut current_y = match self.position {
            RubyPosition::LeftDown | RubyPosition::RightDown => {
                center_y + ((n - 1.0) / 2.0) * vertical_step
            }
            _ => center_y - ((n - 1.0) / 2.0) * vertical_step,
        };

        for (path, width) in paths.iter().zip(widths) {
            let w = width * p_scale;
            let tx = start_x + (max_glyph_width - w) / 2.0;

            let xform = Affine::translate((tx, current_y)) * Affine::scale(p_scale);

            append_transformed(final_path, &path.1, xform);

            match self.position {
                RubyPosition::LeftDown | RubyPosition::RightDown => current_y -= vertical_step,
                _ => current_y += vertical_step,
            }
        }
    }
}

/// Append `source` into `target`, with `transform` applied, without a
/// temporary path allocation.
fn append_transformed(target: &mut BezPath, source: &BezPath, transform: Affine) {
    for el in source.elements() {
        match el {
            kurbo::PathEl::MoveTo(p) => target.move_to(transform * *p),
            kurbo::PathEl::LineTo(p) => target.line_to(transform * *p),
            kurbo::PathEl::QuadTo(p1, p2) => {
                target.quad_to(transform * *p1, transform * *p2);
            }
            kurbo::PathEl::CurveTo(p1, p2, p3) => {
                target.curve_to(transform * *p1, transform * *p2, transform * *p3);
            }
            kurbo::PathEl::ClosePath => target.close_path(),
        }
    }
}
