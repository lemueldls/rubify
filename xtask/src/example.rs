//! Renders the README's `example.png`.
//!
//! `cargo xtask example` downloads a pinned font (LXGW WenKai, OFL-1.1),
//! subsets it to the glyphs the specimen needs, runs the real annotation
//! pipeline over it (once per ruby set), and rasterizes the result by shaping
//! each line and filling the shaped glyph outlines.
//!
//! Ruby is baked into the glyphs by rubify, so drawing the annotated font is
//! all it takes to see it. Word readings (hiragana) fire through the GSUB
//! rules, which is why the lines are shaped with rustybuzz.

use std::fs;

use anyhow::{Context, Result, bail};
use fontcull_font_types::NameId;
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    FileRef, FontRef, TableProvider,
    collections::IntSet,
    types::{GlyphId, Tag},
};
use fontcull_skrifa::{MetadataProvider, OutlineGlyphCollection, instance::Size};
use kurbo::{BezPath, PathEl, Shape};
use rubify::{
    pen::PathPen,
    renderer::{
        RubyPosition, RubyRenderer, hiragana::HiraganaRenderer, pinyin::PinyinRenderer,
        romaji::RomajiRenderer,
    },
    words::{SelectedWord, parse_notation},
};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::{EXAMPLE_FONT, EXAMPLE_FONT_URL, cache_dir, fetch, project_root};

/// Base glyph size, in pixels per em.
const EM: f32 = 104.0;
/// Ruby size as a fraction of the base.
const RUBY_SCALE: f64 = 0.42;
/// Condense ruby horizontally so it stays within its base span, down to this
/// fraction of the natural size.
const RUBY_FIT_FLOOR: f64 = 0.55;
/// Space between the base glyph and its ruby, in em.
const RUBY_GUTTER: f64 = 0.05;
/// Extra space between base glyphs, in em, so neighbouring ruby can breathe.
const TRACK_EM: f32 = 0.09;
/// Extra space between two specimen lines, in pixels.
const LINE_GAP: f32 = 46.0;
/// Page margins, in pixels.
const MARGIN_X: f32 = 96.0;
const MARGIN_Y: f32 = 96.0;

const PAPER: (u8, u8, u8) = (0xfa, 0xf7, 0xf0);
const INK: (u8, u8, u8) = (0x24, 0x21, 0x1c);

fn color((r, g, b): (u8, u8, u8)) -> Color {
    Color::from_rgba8(r, g, b, 0xff)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Pinyin,
    Romaji,
    Hiragana,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Pass {
    script: Script,
    position: RubyPosition,
}

const PY_TOP: Pass = Pass {
    script: Script::Pinyin,
    position: RubyPosition::Top,
};
const HG_TOP: Pass = Pass {
    script: Script::Hiragana,
    position: RubyPosition::Top,
};
const RM_BOTTOM: Pass = Pass {
    script: Script::Romaji,
    position: RubyPosition::Bottom,
};

struct Specimen {
    text: &'static str,
    passes: &'static [Pass],
    /// Inline notation (`日[に]本[ほん]`) for the word-based renderers.
    words: &'static [&'static str],
}

/// The specimen: a Tang line in pinyin, a Genji opening in hiragana and an
/// iroha line in romaji.
const SPECIMEN: &[Specimen] = &[
    Specimen {
        text: "舉頭望明月，低頭思故鄉。",
        passes: &[PY_TOP],
        words: &[],
    },
    Specimen {
        text: "祇園精舎の鐘の声",
        passes: &[HG_TOP],
        words: &["祇園精舎[ぎおんしょうじゃ]", "鐘[かね]", "声[こえ]"],
    },
    Specimen {
        text: "いろはにほへとちりぬるを",
        passes: &[RM_BOTTOM],
        words: &[],
    },
];

/// A shaped glyph, in font units: the id and the offset from the pen.
struct Shaped {
    gid: GlyphId,
    x: f32,
    y: f32,
}

/// One annotated font, its shaped run and the ink it leaves on the page.
struct Rendered {
    fonts: Vec<Vec<u8>>,
    runs: Vec<Vec<Shaped>>,
    /// Ink bounds in font units: `(x0, y0, x1, y1)`, x already offset.
    bounds: (f32, f32, f32, f32),
}

pub fn run() -> Result<()> {
    let cache = cache_dir();
    let root = project_root();
    let out = root.join("example.png");

    let font_path = fetch(&cache, EXAMPLE_FONT, EXAMPLE_FONT_URL)?;
    let base = fs::read(&font_path).with_context(|| format!("Failed to read {font_path:?}"))?;
    let upem = FontRef::new(&base)
        .context("Failed to parse the example font")?
        .head()?
        .units_per_em() as f32;

    // Keep the specimen glyphs, plus everything the ruby might need to draw.
    let mut unicodes = IntSet::<u32>::empty();

    for spec in SPECIMEN {
        for ch in spec.text.chars() {
            unicodes.insert(ch as u32);
        }
    }

    for range in [
        0x20..=0x7e,     // ASCII
        0xa0..=0x24f,    // Latin-1, Extended-A and -B: pinyin tone marks
        0x300..=0x36f,   // combining diacritics
        0x2000..=0x206f, // general punctuation
        0x3000..=0x303f, // CJK punctuation
        0x3040..=0x30ff, // kana
        0xff00..=0xffef, // half- and fullwidth forms
    ] {
        for c in range {
            unicodes.insert(c);
        }
    }

    let subset = subset_charset(&base, &unicodes).context("Failed to subset the example font")?;

    let scale = EM / upem;
    let track = TRACK_EM * upem;
    let mut lines: Vec<Rendered> = Vec::with_capacity(SPECIMEN.len());

    for spec in SPECIMEN {
        let words = parse_words(spec.words)?;
        let words = (!words.is_empty()).then_some(words.as_slice());
        let mut fonts = Vec::new();
        let mut runs = Vec::new();
        let mut bounds: Option<(f32, f32, f32, f32)> = None;

        for pass in spec.passes {
            let data = annotate(&subset, *pass, words).with_context(|| {
                format!(
                    "Failed to annotate ({} / {:?})",
                    script_name(pass.script),
                    pass.position
                )
            })?;
            let run = shape_run(&data, spec.text, track)?;

            if let Some(ink) = ink_bounds(&data, &run)? {
                bounds = Some(union(bounds, ink));
            }

            fonts.push(data);
            runs.push(run);
        }

        lines.push(Rendered {
            fonts,
            runs,
            bounds: bounds.unwrap_or((0.0, 0.0, 0.0, 0.0)),
        });
    }

    let content_width = lines
        .iter()
        .map(|line| (line.bounds.2 - line.bounds.0) * scale)
        .fold(0.0_f32, f32::max);

    let width = MARGIN_X * 2.0 + content_width;

    // Vertical layout: one tight box per line.
    let mut y = MARGIN_Y;
    let mut line_tops = Vec::with_capacity(lines.len());

    for line in &lines {
        let ascent = line.bounds.3 * scale;
        let descent = -line.bounds.1 * scale;
        line_tops.push((y, ascent, descent));
        y += ascent + descent + LINE_GAP;
    }

    let height = y - LINE_GAP + MARGIN_Y;

    let mut pixmap = Pixmap::new(width.ceil() as u32, height.ceil() as u32)
        .context("Image dimensions overflowed")?;
    pixmap.fill(color(PAPER));

    for (line, (top, ascent, _)) in lines.iter().zip(&line_tops) {
        let line_width = (line.bounds.2 - line.bounds.0) * scale;
        let origin_x = MARGIN_X + (content_width - line_width) * 0.5 - line.bounds.0 * scale;
        let baseline = top + ascent;

        for (data, run) in line.fonts.iter().zip(&line.runs) {
            draw(
                &mut pixmap,
                data,
                run,
                origin_x,
                baseline,
                scale,
                color(INK),
            )?;
        }
    }

    let png = pixmap.encode_png().context("Failed to encode PNG")?;
    fs::write(&out, png).with_context(|| format!("Failed to write {out:?}"))?;
    println!("Wrote {out:?} ({}×{})", width.ceil(), height.ceil());

    Ok(())
}

fn script_name(script: Script) -> &'static str {
    match script {
        Script::Pinyin => "pinyin",
        Script::Romaji => "romaji",
        Script::Hiragana => "hiragana",
    }
}

fn parse_words(notations: &[&str]) -> Result<Vec<SelectedWord>> {
    notations
        .iter()
        .map(|notation| {
            let (form, spans) = parse_notation(notation)?;
            Ok(SelectedWord { form, spans })
        })
        .collect()
}

/// Subset `data` to the glyphs for `unicodes`.
fn subset_charset(data: &[u8], unicodes: &IntSet<u32>) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;
    let glyph_ids = IntSet::<GlyphId>::empty();

    let drop_tables: IntSet<Tag> = ["vhea", "vmtx", "VORG"]
        .into_iter()
        .map(|tag| Tag::new_checked(tag.as_bytes()))
        .collect::<Result<_, _>>()?;

    let scripts = IntSet::<Tag>::empty();
    let features = IntSet::<Tag>::empty();
    let name_ids = IntSet::<NameId>::empty();
    let name_languages = IntSet::<u16>::empty();

    let plan = Plan::new(
        &glyph_ids,
        unicodes,
        &font,
        SubsetFlags::default(),
        &drop_tables,
        &scripts,
        &features,
        &name_ids,
        &name_languages,
    );

    Ok(subset_font(&font, &plan)?)
}

/// Run rubify over the subset font, baking one ruby set into it. Word readings
/// (hiragana) become GSUB rules that a shaper applies.
fn annotate(subset: &[u8], pass: Pass, words: Option<&[SelectedWord]>) -> Result<Vec<u8>> {
    let file = FileRef::new(subset)?;
    let font = match file {
        FileRef::Font(font) => font,
        FileRef::Collection(_) => bail!("the subset is not a single font"),
    };

    let renderer: Box<dyn RubyRenderer + '_> = match pass.script {
        Script::Pinyin => Box::new(PinyinRenderer::new(
            font.clone(),
            RUBY_SCALE,
            RUBY_GUTTER,
            pass.position,
            0.0,
            true,
            RUBY_FIT_FLOOR,
            false,
        )?),
        Script::Romaji => Box::new(RomajiRenderer::new(
            font.clone(),
            RUBY_SCALE,
            RUBY_GUTTER,
            pass.position,
            0.0,
            true,
            RUBY_FIT_FLOOR,
            false,
        )?),
        Script::Hiragana => Box::new(HiraganaRenderer::new(
            font.clone(),
            RUBY_SCALE,
            RUBY_GUTTER,
            pass.position,
            0.0,
            true,
            RUBY_FIT_FLOOR,
            false,
        )?),
    };

    rubify::process_font_ref_with(&font, &*renderer, words, false)
}

/// Shape `text` left to right, returning the glyphs. `track` is extra space
/// added after every glyph, in font units.
fn shape_run(data: &[u8], text: &str, track: f32) -> Result<Vec<Shaped>> {
    let face = rustybuzz::Face::from_slice(data, 0).context("Failed to shape: bad font")?;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);

    let glyphs = rustybuzz::shape(&face, &[], buffer);
    let mut pen = 0.0_f32;
    let mut run = Vec::new();

    for (info, pos) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
        run.push(Shaped {
            gid: GlyphId::new(info.glyph_id),
            x: pen + pos.x_offset as f32,
            y: pos.y_offset as f32,
        });
        pen += pos.x_advance as f32 + track;
    }

    Ok(run)
}

/// Union of the glyph ink boxes for a shaped `run`, in font units.
fn ink_bounds(data: &[u8], run: &[Shaped]) -> Result<Option<(f32, f32, f32, f32)>> {
    let font = FontRef::new(data)?;
    let outlines = font.outline_glyphs();
    let mut bounds: Option<(f32, f32, f32, f32)> = None;

    for glyph in run {
        if let Some(path) = outline(&outlines, glyph.gid) {
            let box_ = path.bounding_box();
            let ink = (
                box_.x0 as f32 + glyph.x,
                box_.y0 as f32 + glyph.y,
                box_.x1 as f32 + glyph.x,
                box_.y1 as f32 + glyph.y,
            );
            bounds = Some(union(bounds, ink));
        }
    }

    Ok(bounds)
}

fn union(a: Option<(f32, f32, f32, f32)>, b: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    match a {
        Some((x0, y0, x1, y1)) => (x0.min(b.0), y0.min(b.1), x1.max(b.2), y1.max(b.3)),
        None => b,
    }
}

fn outline(outlines: &OutlineGlyphCollection, gid: GlyphId) -> Option<BezPath> {
    let glyph = outlines.get(gid)?;
    let mut pen = PathPen::new();
    glyph.draw(Size::unscaled(), &mut pen).ok()?;

    if pen.path.elements().is_empty() {
        None
    } else {
        Some(pen.path)
    }
}

/// Fill every glyph of `run` at `(origin_x, baseline)`.
fn draw(
    pixmap: &mut Pixmap,
    data: &[u8],
    run: &[Shaped],
    origin_x: f32,
    baseline: f32,
    scale: f32,
    color: Color,
) -> Result<()> {
    let font = FontRef::new(data)?;
    let outlines = font.outline_glyphs();

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

    for glyph in run {
        let Some(path) = outline(&outlines, glyph.gid).and_then(|path| to_skia(&path)) else {
            continue;
        };

        let transform = Transform::from_row(
            scale,
            0.0,
            0.0,
            -scale,
            origin_x + glyph.x * scale,
            baseline - glyph.y * scale,
        );
        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
    }

    Ok(())
}

/// Convert a kurbo outline to a tiny-skia path (font units; the caller applies
/// the scale and baseline flip).
fn to_skia(path: &BezPath) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();

    for element in path.elements() {
        match *element {
            PathEl::MoveTo(p) => builder.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => builder.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(c, p) => builder.quad_to(c.x as f32, c.y as f32, p.x as f32, p.y as f32),
            PathEl::CurveTo(c0, c1, p) => builder.cubic_to(
                c0.x as f32,
                c0.y as f32,
                c1.x as f32,
                c1.y as f32,
                p.x as f32,
                p.y as f32,
            ),
            PathEl::ClosePath => builder.close(),
        }
    }

    builder.finish()
}
