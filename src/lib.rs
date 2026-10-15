pub mod dictionary;
pub mod gsub;
pub mod pen;
pub mod plan;
pub mod renderer;
pub mod ttc;
pub mod words;

use anyhow::{Context, Result};
use fontcull_font_types::NameId;
use fontcull_klippa::{Plan, SubsetFlags, subset_font};
use fontcull_read_fonts::{
    FileRef, FontData, FontRead, FontRef, TableProvider, TopLevelTable,
    collections::IntSet,
    tables::{glyf::Glyf, loca::Loca},
    types::{GlyphId, Tag},
};
use fontcull_skrifa::{MetadataProvider, instance::Size};
use fontcull_write_fonts::{
    FontBuilder, dump_table,
    from_obj::{ToOwnedObj, ToOwnedTable},
    tables::{
        glyf::SimpleGlyph,
        gsub::Gsub,
        head::Head,
        hhea::Hhea,
        hmtx::{Hmtx, LongMetric},
        loca::Loca as WriteLoca,
        maxp::Maxp,
        os2::Os2,
        post::Post,
        vhea::Vhea,
        vmtx::Vmtx,
    },
};
use indicatif::ProgressStyle;
use kurbo::{BezPath, Shape};
use rayon::iter::{
    IntoParallelIterator, IntoParallelRefIterator, ParallelBridge, ParallelIterator,
};
use rustc_hash::FxHashMap;
use tracing::{info, info_span, warn};
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{
    pen::PathPen,
    plan::GlyphSpec,
    renderer::{BaselineTargets, RubyRenderer},
    words::SelectedWord,
};

pub struct ProcessedFont {
    pub data: Vec<u8>,
    pub file_name: Option<String>,
}

/// What to do with one input font file.
pub struct FontProcessing<'a> {
    pub renderer: &'a dyn RubyRenderer,
    /// Words to turn into annotations (dictionary and custom ones).
    pub words: Option<&'a [SelectedWord]>,
    /// Raise the vertical metrics so ruby is not clipped by tight line spacing.
    pub adjust_metrics: bool,
    /// Keep only annotated characters and their ruby.
    pub subset: bool,
    /// Split collections into one file per font.
    pub split: bool,
}

/// Subset first (when asked), then annotate: the word rules are built on the
/// small font, so the subsetter never has to understand them.
fn process_one(font: &FontRef, options: &FontProcessing) -> Result<Vec<u8>> {
    if options.subset {
        info!("Subsetting font");

        let subset = subset_by_renderers(font, options.renderer, options.words)?;
        let subset = FontRef::new(&subset).context("Failed to parse the subset font")?;

        process_font_ref_with(
            &subset,
            options.renderer,
            options.words,
            options.adjust_metrics,
        )
    } else {
        process_font_ref_with(
            font,
            options.renderer,
            options.words,
            options.adjust_metrics,
        )
    }
}

pub fn process_font_file(file: FileRef, options: &FontProcessing) -> Result<Vec<ProcessedFont>> {
    match file {
        FileRef::Font(font) => {
            let data = process_one(&font, options)?;

            Ok(vec![ProcessedFont {
                data,
                file_name: None,
            }])
        }
        FileRef::Collection(collection) => {
            let collection_span = info_span!("process_fonts_in_collection");
            collection_span.pb_set_style(
                &ProgressStyle::with_template("{msg} [{wide_bar:.green/cyan}] {pos}/{len}")
                    .unwrap(),
            );
            collection_span.pb_set_length(collection.len() as u64);
            collection_span.pb_set_message("Processing collection");

            let process_span_enter = collection_span.enter();

            let processed = collection
                .iter()
                .enumerate()
                .par_bridge()
                .map(|(idx, font)| {
                    collection_span.pb_inc(1);

                    let font = font.context("Failed to read font")?;
                    let data = process_one(&font, options)?;

                    let file_name = if options.split {
                        // Generate output filename
                        if let Ok(name_table) = font.name() {
                            name_table
                                .name_record()
                                .iter()
                                .find(|n| n.name_id() == NameId::POSTSCRIPT_NAME)
                                .and_then(|rec| rec.string(name_table.string_data()).ok())
                                .map(|name| format!("{name}.ttf"))
                                .unwrap_or_else(|| format!("font-{idx}.ttf"))
                        } else {
                            format!("font-{idx}.ttf")
                        }
                    } else {
                        String::new()
                    };

                    Ok(ProcessedFont {
                        data,
                        file_name: options.split.then_some(file_name),
                    })
                })
                .collect::<Result<Vec<ProcessedFont>>>();

            drop(process_span_enter);
            drop(collection_span);

            let fonts = processed?;

            if options.split {
                return Ok(fonts);
            }

            let data = fonts
                .iter()
                .map(|font| FontRef::new(&font.data).context("Failed to create font ref"))
                .collect::<Result<Vec<FontRef>>>()?;

            info_span!("Building TTC");

            let data = ttc::build_collection(&data).context("Failed to build TTC")?;

            Ok(vec![ProcessedFont {
                data,
                file_name: None,
            }])
        }
    }
}

pub fn process_font_ref(font: &FontRef, renderer: &dyn RubyRenderer) -> Result<Vec<u8>> {
    process_font_ref_with(font, renderer, None, false)
}
struct AnnotatedGlyph {
    base_path: BezPath,
    reading: String,
    advance: f64,
    targets: BaselineTargets,
}

struct WordGlyphStage {
    spec: GlyphSpec,
    base_path: BezPath,
    advance: f64,
    spans: Vec<DrawSpan>,
}

struct DrawSpan {
    reading: String,
    following: String,
    x: f64,
    advance: f64,
    targets: BaselineTargets,
}

struct WordGlyphOutput {
    bytes: Vec<u8>,
    metric: LongMetric,
    top: f64,
}

pub fn process_font_ref_with(
    font: &FontRef,
    renderer: &dyn RubyRenderer,
    words: Option<&[SelectedWord]>,
    adjust_metrics: bool,
) -> Result<Vec<u8>> {
    let font_file_data = font.table_directory.offset_data();
    let charmap = font.charmap();
    let hmtx = font.hmtx()?;
    let maxp = font.maxp()?;
    let outlines = font.outline_glyphs();
    let upem = font.head()?.units_per_em() as f64;

    let ranges = renderer.ranges();
    let gid_char_map: FxHashMap<GlyphId, char> = charmap
        .mappings()
        .filter_map(|(c, gid)| {
            if gid != GlyphId::NOTDEF && ranges.iter().any(|r| r.contains(&c)) {
                std::char::from_u32(c).map(|ch| (gid, ch))
            } else {
                None
            }
        })
        .collect();

    let char_to_gid: FxHashMap<char, GlyphId> = charmap
        .mappings()
        .filter_map(|(c, gid)| {
            if gid != GlyphId::NOTDEF {
                std::char::from_u32(c).map(|ch| (ch, gid))
            } else {
                None
            }
        })
        .collect();

    let num_glyphs = maxp.num_glyphs() as usize;

    // Raw glyf data so untouched glyphs can be copied verbatim instead of
    // re-drawing and re-encoding them (preserves hinting and composite glyphs).
    let glyf_raw = font.glyf().ok().map(|g| g.offset_data());
    let loca = font.loca(None).ok();

    // Precompute advances once instead of indexing hmtx per glyph. Glyphs past
    // `numberOfHMetrics` use the last advance, per the spec.
    let h_metrics = hmtx.h_metrics();
    let default_advance = h_metrics
        .iter()
        .last()
        .map(|m| m.advance.get())
        .unwrap_or(upem as u16);
    let advances: Vec<f64> = (0..num_glyphs)
        .map(|gid| {
            h_metrics
                .get(gid)
                .map(|m| m.advance.get() as f64)
                .unwrap_or(default_advance as f64)
        })
        .collect();

    let progress_style = ProgressStyle::with_template(
        "{spinner:.green} {msg} {wide_bar:.cyan/blue} {pos:>7}/{len:7}",
    )?
    .progress_chars("##-");

    let glyphs_span = info_span!("process_glyphs");
    glyphs_span.pb_set_style(&progress_style);
    glyphs_span.pb_set_length(num_glyphs as u64);

    if let Some(ttc_index) = font.ttc_index() {
        glyphs_span.pb_set_message(&format!("Processing glyphs ({})", ttc_index));
    } else {
        glyphs_span.pb_set_message("Processing glyphs");
    }

    let glyphs_span_enter = glyphs_span.enter();

    // Pass 1 (characters): draw the base outlines and measure the ruby, so
    // every annotation in the font can share one baseline. This is
    // deterministic, unlike a running max updated while glyphs are processed
    // in parallel.
    let annotated: Vec<Option<AnnotatedGlyph>> = (0..num_glyphs)
        .into_par_iter()
        .map(|gid_u32| -> Result<Option<AnnotatedGlyph>> {
            let gid = GlyphId::new(gid_u32 as u32);

            let Some(ch) = gid_char_map.get(&gid).copied() else {
                return Ok(None);
            };

            let Some(reading) = renderer.reading_for(ch) else {
                return Ok(None);
            };

            let mut base_path = BezPath::new();

            if let Some(glyph) = outlines.get(gid) {
                let mut pen = PathPen::new();

                if glyph.draw(Size::unscaled(), &mut pen).is_ok() {
                    base_path = pen.path;
                }
            }

            if base_path.elements().is_empty() {
                return Ok(None);
            }

            let bbox = base_path.bounding_box();
            let targets = renderer
                .required_targets(&reading, "", bbox.y1, bbox.y0, upem)
                .unwrap_or_default();

            Ok(Some(AnnotatedGlyph {
                base_path,
                reading,
                advance: advances.get(gid_u32).copied().unwrap_or(upem),
                targets,
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    // Plan the word glyphs and draw their base outlines.
    let plan = words.map(|words| {
        plan::build_plan(words, &char_to_gid, &|reading, following| {
            let text = renderer.prepare_text(reading, following)?;

            renderer.can_draw_text(&text).then_some(text)
        })
    });

    let word_stage: Vec<WordGlyphStage> = match &plan {
        Some(plan) if !plan.glyphs.is_empty() => plan
            .glyphs
            .par_iter()
            .map(|spec| -> Result<Option<WordGlyphStage>> {
                let Some((base_path, advance)) =
                    compose_base_path(&outlines, &spec.gids, &advances)
                else {
                    return Ok(None);
                };

                let bbox = base_path.bounding_box();
                let mut spans = Vec::with_capacity(spec.spans.len());
                let mut x = 0.0;

                for (index, span) in spec.spans.iter().enumerate() {
                    let mut span_advance = 0.0;

                    for gid in &spec.gids[span.start..span.end] {
                        span_advance +=
                            advances.get(gid.to_u32() as usize).copied().unwrap_or(upem);
                    }

                    let following = spec.followings.get(index).cloned().unwrap_or_default();

                    let targets = renderer
                        .required_targets(&span.ruby, &following, bbox.y1, bbox.y0, upem)
                        .unwrap_or_default();

                    spans.push(DrawSpan {
                        reading: span.ruby.clone(),
                        following,
                        x,
                        advance: span_advance,
                        targets,
                    });

                    x += span_advance;
                }

                Ok(Some(WordGlyphStage {
                    spec: spec.clone(),
                    base_path,
                    advance,
                    spans,
                }))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect(),
        _ => Vec::new(),
    };

    let mut targets = BaselineTargets::default();

    for glyph in annotated.iter().flatten() {
        targets.merge(glyph.targets);
    }

    for glyph in &word_stage {
        for span in &glyph.spans {
            targets.merge(span.targets);
        }
    }

    // Pass 2 (characters): draw the ruby with the shared baseline and encode.
    let first_new_gid = num_glyphs as u16;

    let char_outputs: Vec<(Vec<u8>, f64)> = (0..num_glyphs)
        .into_par_iter()
        .map(|gid_u32| {
            glyphs_span.pb_inc(1);

            if let Some(glyph) = &annotated[gid_u32] {
                let mut final_path = glyph.base_path.clone();

                if renderer
                    .draw_reading(
                        &glyph.reading,
                        "",
                        &mut final_path,
                        0.0,
                        glyph.advance,
                        upem,
                        &targets,
                    )
                    .is_ok()
                    && !final_path.elements().is_empty()
                    && let Ok(simple) = SimpleGlyph::from_bezpath(&final_path)
                    && let Ok(bytes) = dump_table(&simple)
                {
                    let top = final_path.bounding_box().y1;
                    return (bytes, top);
                }
            }

            // Fall back to copying the original glyph data verbatim.
            (
                copy_original_glyph(&loca, &glyf_raw, gid_u32).unwrap_or_default(),
                f64::NEG_INFINITY,
            )
        })
        .collect();

    let mut new_glyphs: Vec<Vec<u8>> = Vec::with_capacity(num_glyphs + word_stage.len());
    let mut max_ruby_top = f64::NEG_INFINITY;

    for (bytes, top) in char_outputs {
        max_ruby_top = max_ruby_top.max(top);
        new_glyphs.push(bytes);
    }

    // Pass 2 (words): draw and encode the shared word glyphs.
    let word_outputs: Vec<Result<WordGlyphOutput>> = word_stage
        .par_iter()
        .map(|glyph| {
            let mut final_path = glyph.base_path.clone();

            for span in &glyph.spans {
                renderer
                    .draw_reading(
                        &span.reading,
                        &span.following,
                        &mut final_path,
                        span.x,
                        span.advance,
                        upem,
                        &targets,
                    )
                    .context("Failed to draw word annotation")?;
            }

            let bbox = final_path.bounding_box();
            let simple = SimpleGlyph::from_bezpath(&final_path)
                .map_err(|error| anyhow::anyhow!("Malformed word glyph outline: {error:?}"))?;
            let bytes = dump_table(&simple).context("Failed to write a word glyph")?;

            let advance = glyph.advance.round().clamp(0.0, u16::MAX as f64) as u16;
            let side_bearing = bbox.x0.floor().clamp(i16::MIN as f64, i16::MAX as f64) as i16;

            Ok(WordGlyphOutput {
                bytes,
                metric: LongMetric::new(advance, side_bearing),
                top: bbox.y1,
            })
        })
        .collect();

    let mut word_metrics: Vec<LongMetric> = Vec::with_capacity(word_stage.len());

    for (stage, output) in word_stage.iter().zip(word_outputs) {
        let output = output.with_context(|| {
            format!(
                "Failed to build the glyph for {}",
                stage.spec.bases.iter().collect::<String>()
            )
        })?;

        word_metrics.push(output.metric);
        max_ruby_top = max_ruby_top.max(output.top);
        new_glyphs.push(output.bytes);
    }

    drop(glyphs_span_enter);
    drop(glyphs_span);

    if let Some(plan) = &plan
        && plan.skipped > 0
    {
        info!("Skipped {} word(s) the font cannot draw", plan.skipped);
    }

    let glyph_count = new_glyphs.len();

    if glyph_count > u16::MAX as usize {
        anyhow::bail!(
            "The font would have {glyph_count} glyphs, over the 65535 limit. Use --subset, --no-places or --common-only."
        );
    }

    let mut glyf_out: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(glyph_count + 1);
    offsets.push(0);

    for bytes in new_glyphs {
        if !bytes.is_empty() {
            glyf_out.extend_from_slice(&bytes);
        }
        offsets.push(glyf_out.len() as u32);
    }

    let loca_out = WriteLoca::new(offsets);
    let loca_fmt = loca_out.format() as i16;

    // Prepare the GSUB addition before writing tables.
    let existing_gsub: Option<Vec<u8>> = font
        .data_for_tag(Gsub::TAG)
        .map(|data| data.as_bytes().to_vec());

    let gsub_bytes: Option<Vec<u8>> = match &plan {
        Some(plan) if !plan.glyphs.is_empty() => {
            let base_lookup_count = existing_gsub
                .as_deref()
                .and_then(|bytes| Gsub::read(FontData::new(bytes)).ok())
                .map(|gsub| gsub.lookup_list.lookups.len() as u16)
                .unwrap_or(0);

            let addition = gsub::build_addition(plan, first_new_gid, base_lookup_count);

            if addition.is_empty() {
                None
            } else {
                Some(
                    gsub::merge(existing_gsub.as_deref(), addition)
                        .context("Failed to merge GSUB rules")?,
                )
            }
        }
        _ => None,
    };

    let mut font_builder = FontBuilder::new();
    let rebuild_tables = !word_metrics.is_empty();
    let mut wrote_gsub = false;

    // Re-encoded glyphs no longer match the font's variation data (and
    // appended glyphs have none), so a variable font is flattened to its
    // default instance rather than shipping inconsistent deltas.
    let flattened = rebuild_tables || annotated.iter().any(|glyph| glyph.is_some());

    if flattened && font.gvar().is_ok() {
        warn!(
            "Dropping variation data: rubify re-encodes glyphs, so the variable font is flattened to its default instance"
        );
    }

    for record in font.table_directory.table_records() {
        let tag = record.tag();

        // Skip glyf/loca - we'll insert rebuilt data later
        if tag == Glyf::TAG || tag == Loca::TAG {
            continue;
        }

        if flattened
            && (tag == fontcull_write_fonts::tables::gvar::Gvar::TAG
                || tag == fontcull_write_fonts::tables::hvar::Hvar::TAG
                || tag == fontcull_write_fonts::tables::vvar::Vvar::TAG)
        {
            continue;
        }

        if tag == Head::TAG {
            if let Ok(head) = font.head() {
                let mut head: Head = head.to_owned_obj(font_file_data);

                head.index_to_loc_format = loca_fmt;
                head.checksum_adjustment = 0;

                font_builder
                    .add_table(&head)
                    .context("Failed to add head table")?;
            }

            continue;
        }

        if tag == Hmtx::TAG && rebuild_tables {
            let mut metrics: Hmtx = font.hmtx()?.to_owned_table();

            // Appended glyphs get their own long metrics, which means every
            // glyph before them must have one too: promote the trailing side
            // bearings using the last advance, then append ours.
            let default_advance = metrics.h_metrics.last().map(|m| m.advance).unwrap_or(0);

            for side_bearing in &metrics.left_side_bearings {
                metrics
                    .h_metrics
                    .push(LongMetric::new(default_advance, *side_bearing));
            }

            metrics.left_side_bearings.clear();
            metrics.h_metrics.extend(word_metrics.iter().cloned());

            font_builder
                .add_table(&metrics)
                .context("Failed to add hmtx table")?;

            continue;
        }

        if tag == Maxp::TAG && rebuild_tables {
            let mut maxp: Maxp = font.maxp()?.to_owned_table();

            maxp.num_glyphs = glyph_count as u16;
            font_builder
                .add_table(&maxp)
                .context("Failed to add maxp table")?;

            continue;
        }

        if tag == Post::TAG && rebuild_tables {
            let mut post: Post = font.post()?.to_owned_table();

            if let Some(indices) = post.glyph_name_index.as_mut() {
                indices.extend(std::iter::repeat_n(0u16, word_metrics.len()));
            }

            if let Some(count) = post.num_glyphs.as_mut() {
                *count = glyph_count as u16;
            }

            font_builder
                .add_table(&post)
                .context("Failed to add post table")?;

            continue;
        }

        if tag == Vmtx::TAG
            && rebuild_tables
            && let Ok(vmtx) = font.vmtx()
        {
            let mut vmtx: Vmtx = vmtx.to_owned_table();

            for _ in 0..word_metrics.len() {
                vmtx.v_metrics.push(LongMetric::new(upem as u16, 0));
            }

            font_builder
                .add_table(&vmtx)
                .context("Failed to add vmtx table")?;

            if let Ok(vhea) = font.vhea() {
                let mut vhea: Vhea = vhea.to_owned_table();
                vhea.number_of_long_ver_metrics = vmtx.v_metrics.len() as u16;

                font_builder
                    .add_table(&vhea)
                    .context("Failed to add vhea table")?;
            }

            continue;
        }

        if tag == Gsub::TAG
            && let Some(bytes) = &gsub_bytes
        {
            font_builder.add_raw(tag, bytes.clone());
            wrote_gsub = true;
            continue;
        }

        if tag == Hhea::TAG && (rebuild_tables || adjust_metrics) {
            if let Ok(hhea) = font.hhea() {
                let mut hhea: Hhea = hhea.to_owned_table();

                if rebuild_tables {
                    // Every glyph gets a long metric once we append ours.
                    hhea.number_of_h_metrics = (num_glyphs + word_metrics.len()) as u16;
                }

                if adjust_metrics {
                    let top = max_ruby_top.ceil();

                    if top.is_finite() && f64::from(hhea.ascender.to_i16()) < top {
                        hhea.ascender = fontcull_write_fonts::types::FWord::new(
                            top.clamp(i16::MIN as f64, i16::MAX as f64) as i16,
                        );
                    }
                }

                font_builder
                    .add_table(&hhea)
                    .context("Failed to add hhea table")?;
            }

            continue;
        }

        if adjust_metrics && tag == Os2::TAG {
            if let Ok(os2) = font.os2() {
                let mut os2: Os2 = os2.to_owned_table();
                let top = max_ruby_top.ceil();

                if top.is_finite() && f64::from(os2.us_win_ascent) < top {
                    os2.us_win_ascent = top.clamp(0.0, u16::MAX as f64) as u16;
                }

                font_builder
                    .add_table(&os2)
                    .context("Failed to add OS/2 table")?;
            }

            continue;
        }

        if let Some(data) = font.data_for_tag(tag) {
            font_builder.add_raw(tag, data.as_bytes().to_vec());
        }
    }

    font_builder
        .add_raw(Glyf::TAG, glyf_out)
        .add_raw(Loca::TAG, dump_table(&loca_out)?);

    // The input font may have had no GSUB at all.
    if !wrote_gsub && let Some(bytes) = &gsub_bytes {
        font_builder.add_raw(Gsub::TAG, bytes.clone());
    }

    Ok(font_builder.build())
}

/// Draw base glyphs side by side, advancing by each glyph's width; `None`
/// when one of them has no outline.
fn compose_base_path(
    outlines: &fontcull_skrifa::OutlineGlyphCollection,
    gids: &[GlyphId],
    advances: &[f64],
) -> Option<(BezPath, f64)> {
    let mut path = BezPath::new();
    let mut x = 0.0;

    for gid in gids {
        let glyph = outlines.get(*gid)?;
        let mut pen = PathPen::new();

        glyph.draw(Size::unscaled(), &mut pen).ok()?;

        if pen.path.elements().is_empty() {
            return None;
        }

        for element in pen.path.elements() {
            match element {
                kurbo::PathEl::MoveTo(p) => path.move_to(*p + kurbo::Vec2::new(x, 0.0)),
                kurbo::PathEl::LineTo(p) => path.line_to(*p + kurbo::Vec2::new(x, 0.0)),
                kurbo::PathEl::QuadTo(p1, p2) => path.quad_to(
                    *p1 + kurbo::Vec2::new(x, 0.0),
                    *p2 + kurbo::Vec2::new(x, 0.0),
                ),
                kurbo::PathEl::CurveTo(p1, p2, p3) => path.curve_to(
                    *p1 + kurbo::Vec2::new(x, 0.0),
                    *p2 + kurbo::Vec2::new(x, 0.0),
                    *p3 + kurbo::Vec2::new(x, 0.0),
                ),
                kurbo::PathEl::ClosePath => path.close_path(),
            }
        }

        x += advances.get(gid.to_u32() as usize).copied().unwrap_or(0.0);
    }

    Some((path, x))
}

/// Copy the original raw glyf bytes for a glyph, preserving hinting and
/// composite structure. Returns empty bytes for empty/missing glyphs.
pub(crate) fn copy_original_glyph(
    loca: &Option<Loca<'_>>,
    glyf_raw: &Option<FontData<'_>>,
    gid: usize,
) -> Result<Vec<u8>> {
    if let (Some(loca), Some(raw)) = (loca, glyf_raw) {
        let start = loca.get_raw(gid).map(|s| s as usize).unwrap_or(0);
        let end = loca.get_raw(gid + 1).map(|e| e as usize).unwrap_or(start);

        if end > start
            && let Some(data) = raw.slice(start..end)
        {
            return Ok(data.as_bytes().to_vec());
        }
    }

    Ok(Vec::new())
}

/// Subset a font to the characters the renderer (and the words) need, before
/// annotation. Word rules are not built yet, so the subsetter never sees them.
pub fn subset_by_renderers(
    font: &FontRef,
    renderer: &dyn RubyRenderer,
    words: Option<&[SelectedWord]>,
) -> Result<Vec<u8>> {
    // Build unicodes set based on provided character sets
    let mut unicodes = IntSet::<u32>::empty();

    for range in renderer.ranges() {
        for c in range.clone() {
            unicodes.insert(c);
        }
    }

    // The characters of every word that may be annotated.
    if let Some(words) = words {
        for word in words {
            for ch in word.form.chars() {
                unicodes.insert(ch as u32);
            }
        }
    }

    let glyph_ids = IntSet::<GlyphId>::empty();
    let drop_tables: IntSet<Tag> = ["vhea", "vmtx", "VORG"]
        .into_iter()
        .map(|tag| Tag::new_checked(tag.as_bytes()))
        .collect::<Result<_, _>>()?;
    let layout_scripts = IntSet::<Tag>::empty();
    let layout_features = IntSet::<Tag>::empty();
    let name_ids = IntSet::<NameId>::empty();
    let name_languages = IntSet::<u16>::empty();

    let plan = Plan::new(
        &glyph_ids,
        &unicodes,
        font,
        SubsetFlags::default(),
        &drop_tables,
        &layout_scripts,
        &layout_features,
        &name_ids,
        &name_languages,
    );

    subset_font(font, &plan).context("Subset error")
}

#[cfg(feature = "woff2")]
pub fn convert_to_woff2(font_data: &[u8]) -> Result<Vec<u8>> {
    woofwoof::compress(font_data, [], 11, true).context("WOFF2 compression failed")
}
