use std::sync::Mutex;

use fontcull_read_fonts::FontRef;
use fontcull_skrifa::{GlyphId, MetadataProvider, instance::Size};
use kurbo::{BezPath, Shape};

use crate::renderer::RubyPosition;

pub type GlyphPaths = Vec<(GlyphId, BezPath)>;

/// Collect glyph paths; returns None if any glyph cannot be found or drawn.
pub fn collect_glyph_paths(font: &FontRef, text: String) -> Option<GlyphPaths> {
    let cmap = font.charmap();
    let outlines = font.outline_glyphs();

    let mut glyph_paths: Vec<(GlyphId, BezPath)> = Vec::new();

    for pc in text.chars() {
        match cmap.map(pc) {
            Some(pgid) if pgid.to_u32() != 0 => {
                if let Some(pglyph) = outlines.get(pgid) {
                    let mut ppen = crate::PathPen::new();
                    let res = pglyph.draw(Size::unscaled(), &mut ppen);

                    if res.is_ok() {
                        glyph_paths.push((pgid, ppen.path));
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }

    Some(glyph_paths)
}

/// Compute widths for each text given a closure to get advance (in font units).
pub fn compute_glyph_widths(
    glyph_paths: &GlyphPaths,
    p_scale_factor: f64,
    mut get_adv: impl FnMut(GlyphId) -> f64,
) -> Vec<f64> {
    let mut text_widths: Vec<f64> = Vec::new();

    for (pgid, _) in glyph_paths {
        let mut text_width = 0.0;
        let adv = get_adv(*pgid);
        text_width += adv * p_scale_factor;
        text_widths.push(text_width);
    }

    text_widths
}

/// Render top/bottom annotated text into `final_path`.
#[allow(clippy::too_many_arguments)]
pub fn render_top_bottom(
    final_path: &mut BezPath,
    glyph_paths: GlyphPaths,
    text_widths: &[f64],
    p_scale_factor: f64,
    main_upem: f64,
    orig_advance: f64,
    position: RubyPosition,
    gutter_em: f64,
    baseline_offset_em: f64,
    tight: bool,
    cached_top: &Mutex<Option<f64>>,
    cached_bottom: &Mutex<Option<f64>>,
    mut get_adv: impl FnMut(GlyphId) -> f64,
) {
    let total_width = text_widths.iter().sum::<f64>();

    let bbox = final_path.bounding_box();
    let gutter_units = gutter_em * main_upem;
    let approx_height = main_upem * (p_scale_factor * (1.0 / (p_scale_factor.max(0.00001)))) * 0.8; // conservative

    let baseline_offset_units = baseline_offset_em * main_upem;

    // Measure min/max y of the pinyin glyphs in unscaled font units

    let mut min_y: f64 = f64::INFINITY;
    let mut max_y: f64 = f64::NEG_INFINITY;

    for (_pgid, p_path) in &glyph_paths {
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
        max_y = approx_height / p_scale_factor;
    }

    let min_y_scaled = min_y * p_scale_factor;
    let max_y_scaled = max_y * p_scale_factor;

    let required_top_target = bbox.y1 + gutter_units + baseline_offset_units - min_y_scaled;
    let required_bottom_target = bbox.y0 - gutter_units - baseline_offset_units - max_y_scaled;

    let target_y = if tight {
        if position == RubyPosition::Top {
            bbox.y1 + gutter_units
        } else {
            bbox.y0 - gutter_units - approx_height
        }
    } else {
        if position == RubyPosition::Top {
            let mut cached = cached_top.lock().unwrap();

            if let Some(prev) = *cached {
                let newv = prev.max(required_top_target);
                *cached = Some(newv);

                newv
            } else {
                *cached = Some(required_top_target);

                required_top_target
            }
        } else {
            let mut cached = cached_bottom.lock().unwrap();
            if let Some(prev) = *cached {
                let newv = prev.min(required_bottom_target);
                *cached = Some(newv);

                newv
            } else {
                *cached = Some(required_bottom_target);

                required_bottom_target
            }
        }
    };

    let mut current_x = (orig_advance - total_width) / 2.0;

    for (pgid, mut p_path) in glyph_paths.into_iter() {
        let xform =
            kurbo::Affine::translate((current_x, target_y)) * kurbo::Affine::scale(p_scale_factor);

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

        let adv = get_adv(pgid);
        current_x += adv * p_scale_factor;
    }
}

/// Render side-positioned annotations (left/right, up/down stacking)
#[allow(clippy::too_many_arguments)]
pub fn render_side(
    final_path: &mut BezPath,
    glyph_paths: &GlyphPaths,
    p_scale_factor: f64,
    main_upem: f64,
    orig_advance: f64,
    position: RubyPosition,
    gutter_em: f64,
    bbox_center_y: f64,
    get_adv: &mut impl FnMut(GlyphId) -> f64,
) {
    let mut glyph_list: Vec<(f64, BezPath)> = Vec::new();

    for (pgid, p_path) in glyph_paths {
        let adv = get_adv(*pgid);
        glyph_list.push((adv * p_scale_factor, p_path.clone()));
    }

    if glyph_list.is_empty() {
        return;
    }

    let max_glyph_width = glyph_list.iter().map(|(w, _)| *w).fold(0.0f64, f64::max);
    let vertical_step = main_upem * p_scale_factor * 0.8;
    let gutter_units = gutter_em * main_upem;

    let start_x = match position {
        RubyPosition::LeftDown | RubyPosition::LeftUp => -(max_glyph_width + gutter_units),
        _ => orig_advance + gutter_units,
    };

    let n = glyph_list.len() as f64;
    let mut current_y = match position {
        RubyPosition::LeftDown | RubyPosition::RightDown => {
            bbox_center_y + ((n - 1.0) / 2.0) * vertical_step
        }
        _ => bbox_center_y - ((n - 1.0) / 2.0) * vertical_step,
    };

    for (w, mut p_path) in glyph_list {
        let tx = start_x + (max_glyph_width - w) / 2.0;

        let xform =
            kurbo::Affine::translate((tx, current_y)) * kurbo::Affine::scale(p_scale_factor);

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

        match position {
            RubyPosition::LeftDown | RubyPosition::RightDown => current_y -= vertical_step,
            _ => current_y += vertical_step,
        }
    }
}
