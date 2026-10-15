use std::sync::Mutex;

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

pub type GlyphPaths = Vec<(GlyphId, BezPath)>;

/// Cache of drawn ruby-font glyph paths, keyed by character.
///
/// The same characters (pinyin letters, romaji letters, kana) are drawn over
/// and over for every annotated base glyph; caching avoids re-parsing and
/// re-drawing them for each occurrence.
#[derive(Default)]
pub struct GlyphPathCache {
    by_char: Mutex<FxHashMap<char, Option<(GlyphId, BezPath)>>>,
}

fn draw_char(
    cmap: &Charmap,
    outlines: &OutlineGlyphCollection,
    pc: char,
) -> Option<(GlyphId, BezPath)> {
    let pgid = cmap.map(pc)?;

    if pgid.to_u32() == 0 {
        return None;
    }

    let pglyph = outlines.get(pgid)?;
    let mut ppen = PathPen::new();

    pglyph.draw(Size::unscaled(), &mut ppen).ok()?;

    Some((pgid, ppen.path))
}

/// Collect glyph paths; returns `None` if any glyph cannot be found or drawn.
pub fn collect_glyph_paths(
    font: &FontRef,
    text: &str,
    cache: &GlyphPathCache,
) -> Option<GlyphPaths> {
    let cmap = font.charmap();
    let outlines = font.outline_glyphs();

    let mut glyph_paths: Vec<(GlyphId, BezPath)> = Vec::new();

    for pc in text.chars() {
        let entry = {
            let mut guard = cache.by_char.lock().unwrap();

            if let Some(entry) = guard.get(&pc) {
                entry.clone()
            } else {
                let drawn = draw_char(&cmap, &outlines, pc);
                guard.insert(pc, drawn.clone());
                drawn
            }
        };

        glyph_paths.push(entry?);
    }

    Some(glyph_paths)
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
    /// Cache of drawn ruby glyph paths, keyed by character.
    cache: GlyphPathCache,
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

        Ok(Self {
            font,
            upem,
            scale_ratio,
            gutter_em,
            baseline_offset_em,
            position,
            fit,
            fit_floor,
            tight,
            cache: GlyphPathCache::default(),
        })
    }

    /// Scale factor from ruby font units to main font units.
    pub fn scale_factor(&self, main_upem: f64) -> f64 {
        (self.scale_ratio * main_upem) / self.upem
    }

    fn advance_of(&self, gid: GlyphId) -> f64 {
        let Ok(hmtx) = self.font.hmtx() else {
            return self.upem;
        };

        hmtx.h_metrics()
            .get(gid.to_u32() as usize)
            .map(|m| m.advance.get())
            .unwrap_or(self.upem as u16) as f64
    }

    /// Collect glyph paths and advances for a text, plus the total width
    /// (all in ruby font units).
    pub fn collect(&self, text: &str) -> Option<(GlyphPaths, Vec<f64>, f64)> {
        let paths = collect_glyph_paths(&self.font, text, &self.cache)?;
        let mut widths = Vec::with_capacity(paths.len());
        let mut total = 0.0;

        for (pgid, _) in &paths {
            let width = self.advance_of(*pgid);
            total += width;
            widths.push(width);
        }

        Some((paths, widths, total))
    }

    fn unscaled_extents(&self, paths: &GlyphPaths) -> (f64, f64) {
        let mut min_y: f64 = f64::INFINITY;
        let mut max_y: f64 = f64::NEG_INFINITY;

        for (_pgid, p_path) in paths {
            for el in p_path.elements() {
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

        (min_y, max_y)
    }

    /// Baseline targets required for drawing `text` over a base span.
    pub fn required_targets(
        &self,
        text: &str,
        base_top: f64,
        base_bottom: f64,
        main_upem: f64,
    ) -> Option<BaselineTargets> {
        if !matches!(self.position, RubyPosition::Top | RubyPosition::Bottom) || self.tight {
            return None;
        }

        let (paths, _, _) = self.collect(text)?;
        let (min_y, max_y) = self.unscaled_extents(&paths);

        let p_scale = self.scale_factor(main_upem);
        let gutter = self.gutter_em * main_upem;
        let offset = self.baseline_offset_em * main_upem;
        let min_y_scaled = min_y * p_scale;
        let max_y_scaled = max_y * p_scale;

        let mut targets = BaselineTargets::default();

        match self.position {
            RubyPosition::Top => {
                targets.top = Some(base_top + gutter + offset - min_y_scaled);
            }
            RubyPosition::Bottom => {
                targets.bottom = Some(base_bottom - gutter - offset - max_y_scaled);
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
        let Some((paths, _widths, total_width)) = self.collect(text) else {
            return false;
        };

        if matches!(self.position, RubyPosition::Top | RubyPosition::Bottom) {
            self.draw_top_bottom(
                paths,
                final_path,
                span_x,
                span_advance,
                main_upem,
                total_width,
                targets,
            );
        } else {
            self.draw_side(paths, final_path, span_advance, main_upem);
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
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        total_width: f64,
        targets: &BaselineTargets,
    ) {
        let p_scale = self.scale_factor(main_upem);
        let total_width_units = total_width * p_scale;
        let x_scale = self.x_scale(total_width_units, span_advance);

        let base_top = final_path.bounding_box().y1;
        let base_bottom = final_path.bounding_box().y0;

        let target_y = match self.position {
            RubyPosition::Top => targets.top.unwrap_or_else(|| {
                self.required_targets_of(&paths, p_scale, main_upem, base_top, base_bottom)
                    .top
                    .unwrap_or(base_top)
            }),
            RubyPosition::Bottom => targets.bottom.unwrap_or_else(|| {
                self.required_targets_of(&paths, p_scale, main_upem, base_top, base_bottom)
                    .bottom
                    .unwrap_or(base_bottom)
            }),
            _ => return,
        };

        let mut current_x = span_x + (span_advance - total_width_units * x_scale) / 2.0;

        for (pgid, mut p_path) in paths.into_iter() {
            let xform = Affine::translate((current_x, target_y))
                * Affine::scale_non_uniform(p_scale * x_scale, p_scale);

            p_path.apply_affine(xform);

            for el in p_path.elements() {
                match el {
                    kurbo::PathEl::MoveTo(p) => final_path.move_to(*p),
                    kurbo::PathEl::LineTo(p) => final_path.line_to(*p),
                    kurbo::PathEl::QuadTo(p1, p2) => final_path.quad_to(*p1, *p2),
                    kurbo::PathEl::CurveTo(p1, p2, p3) => final_path.curve_to(*p1, *p2, *p3),
                    kurbo::PathEl::ClosePath => final_path.close_path(),
                }
            }

            current_x += self.advance_of(pgid) * p_scale * x_scale;
        }
    }

    fn required_targets_of(
        &self,
        paths: &GlyphPaths,
        p_scale: f64,
        main_upem: f64,
        base_top: f64,
        base_bottom: f64,
    ) -> BaselineTargets {
        let (min_y, max_y) = self.unscaled_extents(paths);
        let gutter = self.gutter_em * main_upem;
        let offset = self.baseline_offset_em * main_upem;
        let mut targets = BaselineTargets::default();

        match self.position {
            RubyPosition::Top => {
                targets.top = Some(base_top + gutter + offset - min_y * p_scale);
            }
            RubyPosition::Bottom => {
                targets.bottom = Some(base_bottom - gutter - offset - max_y * p_scale);
            }
            _ => {}
        }

        targets
    }

    /// Render side-positioned annotations (left/right, up/down stacking).
    fn draw_side(
        &self,
        paths: GlyphPaths,
        final_path: &mut BezPath,
        orig_advance: f64,
        main_upem: f64,
    ) {
        let p_scale = self.scale_factor(main_upem);

        let mut glyph_list: Vec<(f64, BezPath)> = Vec::new();

        for (pgid, p_path) in &paths {
            glyph_list.push((self.advance_of(*pgid) * p_scale, p_path.clone()));
        }

        if glyph_list.is_empty() {
            return;
        }

        let bbox = final_path.bounding_box();
        let center_y = (bbox.y0 + bbox.y1) / 2.0;

        let max_glyph_width = glyph_list.iter().map(|(w, _)| *w).fold(0.0f64, f64::max);
        let vertical_step = main_upem * p_scale * 0.8;
        let gutter_units = self.gutter_em * main_upem;

        let start_x = match self.position {
            RubyPosition::LeftDown | RubyPosition::LeftUp => -(max_glyph_width + gutter_units),
            _ => orig_advance + gutter_units,
        };

        let n = glyph_list.len() as f64;
        let mut current_y = match self.position {
            RubyPosition::LeftDown | RubyPosition::RightDown => {
                center_y + ((n - 1.0) / 2.0) * vertical_step
            }
            _ => center_y - ((n - 1.0) / 2.0) * vertical_step,
        };

        for (w, mut p_path) in glyph_list {
            let tx = start_x + (max_glyph_width - w) / 2.0;

            let xform = Affine::translate((tx, current_y)) * Affine::scale(p_scale);

            p_path.apply_affine(xform);

            for el in p_path.elements() {
                match el {
                    kurbo::PathEl::MoveTo(p) => final_path.move_to(*p),
                    kurbo::PathEl::LineTo(p) => final_path.line_to(*p),
                    kurbo::PathEl::QuadTo(p1, p2) => final_path.quad_to(*p1, *p2),
                    kurbo::PathEl::CurveTo(p1, p2, p3) => final_path.curve_to(*p1, *p2, *p3),
                    kurbo::PathEl::ClosePath => final_path.close_path(),
                }
            }

            match self.position {
                RubyPosition::LeftDown | RubyPosition::RightDown => current_y -= vertical_step,
                _ => current_y += vertical_step,
            }
        }
    }
}
