use std::{fs, path::PathBuf, str::FromStr};

use anyhow::{Context, Error, Result, anyhow};
use facet::Facet;
use figue::{self as args, FigueBuiltins};
use fontcull_read_fonts::FileRef;
use glob::glob;
use indicatif::ProgressStyle;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rubify::{
    FontProcessing,
    dictionary::{self, Dictionary, Lang},
    renderer::{self, RubyPosition, RubyRenderer},
    words::{SelectedWord, SelectionOptions, parse_word_list, select_words},
};
use rustc_hash::FxHashSet;
use tracing::{info, info_span};
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Facet)]
struct Cli {
    /// Input paths or glob patterns (can be repeated), e.g. 'fonts/*.ttf' or 'file.ttf'
    #[facet(args::positional)]
    inputs: Vec<String>,

    /// Output directory
    #[facet(args::named, args::short = 'o')]
    out: PathBuf,

    /// Ruby characters: pinyin, romaji or hiragana.
    #[facet(args::named)]
    ruby: String,

    /// Separate font file to use for ruby characters
    #[facet(args::named)]
    font: Option<PathBuf>,

    /// Subset the font to include only annotated characters.
    #[facet(args::named, default = false)]
    subset: bool,

    /// Split font collection (TTC) into separate TTF files instead of rebuilding as TTC.
    #[facet(args::named, default = false)]
    split: bool,

    /// Convert all outputs to WOFF2
    #[cfg(feature = "woff2")]
    #[facet(args::named, default = false)]
    woff2: bool,

    /// Where to place ruby characters relative to base glyph.
    #[facet(args::named, default = "top")]
    position: String,

    /// Scale ratio for ruby characters (fraction of main font size).
    #[facet(args::named, default = 0.4)]
    scale: f64,

    /// Gutter (in em) between base glyph and ruby characters.
    #[facet(args::named, default = 0.0)]
    gutter: f64,

    /// When set, use tight per-character placement. By default we use a consistent baseline.
    #[facet(args::named, default = false)]
    tight: bool,

    /// Fine-tune baseline offset (in em units). Positive moves annotation further away from base glyph.
    #[facet(args::named, default = 0.0)]
    offset: f64,

    /// Squeeze ruby horizontally so it fits its character (or word).
    #[facet(args::named, default = false)]
    fit: bool,

    /// Lowest scale the fitting ruby is squeezed to. Below it the ruby overhangs; 0 fits exactly, however narrow it gets.
    #[facet(args::named, default = 0.0)]
    fit_floor: f64,

    /// Raise the ascender so ruby is not clipped by tight line spacing.
    #[facet(args::named, default = false)]
    adjust_metrics: bool,

    /// Word list file(s) in inline notation (`日[に]本[ほん]`) or `word<TAB>reading`. Added on top of the bundled dictionary.
    #[facet(args::named, default)]
    words: Vec<PathBuf>,

    /// Compiled dictionary file to use instead of the bundled one.
    #[facet(args::named)]
    dict: Option<PathBuf>,

    /// Do not use any bundled dictionary; only --words lists are annotated.
    #[facet(args::named, default = false)]
    no_dict: bool,

    /// Keep only words in the dictionary's frequency window (drops place names and unranked words).
    #[facet(args::named, default = false)]
    common_only: bool,

    /// Do not annotate place names.
    #[facet(args::named, default = false)]
    no_places: bool,

    /// Do not annotate unranked words.
    #[facet(args::named, default = false)]
    no_untagged: bool,

    /// Skip words the reader is taken to know: N5..N1 keeps that level and all easier ones bare.
    #[facet(args::named)]
    skip_jlpt: Option<String>,

    /// Lowest newspaper frequency bucket to annotate (1..48).
    #[facet(args::named, default = 1)]
    nf_min: u16,

    /// Highest newspaper frequency bucket to annotate.
    #[facet(args::named, default = 48)]
    nf_max: u16,

    /// Comma separated JMdict priority tags that select a word whatever its bucket.
    #[facet(args::named, default = "ichi1,news1,spec1,gai1")]
    select_tags: String,

    /// Tatoeba uses that select a word whatever its bucket (0 disables).
    #[facet(args::named, default = 3)]
    min_tatoeba: u16,

    /// Forms to leave bare (can be repeated).
    #[facet(args::named, default)]
    bare: Vec<String>,

    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

pub enum Ruby {
    #[cfg(feature = "pinyin")]
    Pinyin,
    #[cfg(feature = "romaji")]
    Romaji,
    #[cfg(feature = "hiragana")]
    Hiragana,
}

impl FromStr for Ruby {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            #[cfg(feature = "pinyin")]
            "pinyin" => Ok(Ruby::Pinyin),
            #[cfg(feature = "romaji")]
            "romaji" => Ok(Ruby::Romaji),
            #[cfg(feature = "hiragana")]
            "hiragana" => Ok(Ruby::Hiragana),
            other => Err(anyhow!("Unknown ruby characters argument: {other}")),
        }
    }
}

fn position_from_str(s: &str) -> Result<RubyPosition> {
    match s.to_lowercase().as_str() {
        "top" => Ok(RubyPosition::Top),
        "bottom" => Ok(RubyPosition::Bottom),
        "leftdown" => Ok(RubyPosition::LeftDown),
        "leftup" => Ok(RubyPosition::LeftUp),
        "rightdown" => Ok(RubyPosition::RightDown),
        "rightup" => Ok(RubyPosition::RightUp),
        other => Err(anyhow!("Unknown ruby position argument: {other}")),
    }
}

fn skip_jlpt_from_str(s: &str) -> Result<u8> {
    match s.to_uppercase().as_str() {
        "N5" => Ok(5),
        "N4" => Ok(4),
        "N3" => Ok(3),
        "N2" => Ok(2),
        "N1" => Ok(1),
        other => Err(anyhow!("Unknown JLPT level argument: {other}")),
    }
}

/// The language a renderer expects word readings in.
#[allow(unreachable_patterns)]
fn ruby_lang(ruby: &Ruby) -> Lang {
    match ruby {
        #[cfg(feature = "pinyin")]
        Ruby::Pinyin => Lang::Zh,
        #[cfg(feature = "romaji")]
        Ruby::Romaji => Lang::Ja,
        #[cfg(feature = "hiragana")]
        Ruby::Hiragana => Lang::Ja,
        // Without renderer features the binary cannot run, but it must build.
        _ => Lang::Ja,
    }
}

/// Load the dictionary and choose the words to annotate.
fn select_annotation_words(cli: &Cli, ruby: &Ruby) -> Result<Option<Vec<SelectedWord>>> {
    let mut custom = Vec::new();

    for path in &cli.words {
        let content = fs::read_to_string(path)
            .with_context(|| anyhow!("Failed to read word list {path:?}"))?;
        custom.extend(
            parse_word_list(&content)
                .with_context(|| anyhow!("Failed to parse word list {path:?}"))?,
        );
    }

    let dictionary = if let Some(path) = &cli.dict {
        dictionary::load_path(path)?
    } else if cli.no_dict {
        Dictionary::default()
    } else {
        dictionary::load_bundled(ruby_lang(ruby)).unwrap_or_default()
    };

    if dictionary.entries.is_empty() && custom.is_empty() {
        return Ok(None);
    }

    let mut removed: FxHashSet<String> = dictionary.bare.iter().cloned().collect();

    for form in &cli.bare {
        removed.insert(form.clone());
    }

    let mut options = SelectionOptions {
        custom,
        removed,
        ..SelectionOptions::default()
    };

    options.nf_min = cli.nf_min;
    options.nf_max = cli.nf_max;
    options.min_tatoeba = cli.min_tatoeba;
    options.select_tags = cli
        .select_tags
        .split(',')
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();

    if cli.skip_jlpt.is_some() {
        options.skip_jlpt = cli
            .skip_jlpt
            .as_deref()
            .map(skip_jlpt_from_str)
            .transpose()?;
    }

    if cli.common_only {
        options.places = false;
        options.untagged = false;
    } else {
        options.places = !cli.no_places;
        options.untagged = !cli.no_untagged;
    }

    let selected = select_words(&dictionary, &options);

    if selected.is_empty() {
        Ok(None)
    } else {
        info!("Annotating {} word patterns", selected.len());

        Ok(Some(selected))
    }
}

fn main() -> Result<()> {
    let indicatif_layer = IndicatifLayer::new();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rubify=info,tower_buffer=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer()))
        .with(indicatif_layer)
        .init();

    let cli: Cli = args::from_std_args().unwrap();

    let ruby = Ruby::from_str(&cli.ruby)
        .with_context(|| anyhow!("Failed to parse --ruby argument: {}", cli.ruby))?;

    let mut input_paths: FxHashSet<PathBuf> = FxHashSet::default();

    for pattern in &cli.inputs {
        let entries =
            glob(pattern).with_context(|| anyhow!("Failed to expand glob pattern: {pattern:?}"))?;

        for entry in entries {
            match entry {
                Ok(path) if path.is_file() => {
                    input_paths.insert(path);
                }
                _ => {}
            }
        }
    }

    if input_paths.is_empty() {
        return Err(anyhow!("No input files found"));
    }

    if !cli.out.exists() {
        fs::create_dir_all(&cli.out)
            .with_context(|| anyhow!("Failed to create out-dir: {:?}", cli.out))?;
    }

    #[cfg(feature = "woff2")]
    if cli.woff2 && !cli.split {
        anyhow::bail!(
            "WOFF2 output is only supported when --split is enabled, because we don't currently support converting TTC collections to WOFF2. Please enable --split or disable --woff2."
        );
    }

    let words = select_annotation_words(&cli, &ruby)?;

    info!("Processing {} inputs -> {:?}", input_paths.len(), cli.out);

    let inputs_span = info_span!("process_fonts_in_inputs");
    inputs_span.pb_set_style(
        &ProgressStyle::with_template(
            "{msg} [{wide_bar:.cyan/blue}] {pos}/{len} [{elapsed_precise}]",
        )
        .unwrap(),
    );
    inputs_span.pb_set_length(input_paths.len() as u64);
    inputs_span.pb_set_message("Processing font inputs");

    let inputs_span_enter = inputs_span.enter();

    for in_path in &input_paths {
        inputs_span.pb_inc(1);
        inputs_span.pb_set_message(&format!("Processing {in_path:?}"));

        let file_name = in_path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid file name")?
            .to_string();

        let out_path = cli.out.join(file_name);

        process_file(&cli, &ruby, words.as_deref(), in_path, &out_path)?;
    }

    drop(inputs_span_enter);
    drop(inputs_span);

    info!("Done processing inputs.");

    Ok(())
}

fn process_file(
    cli: &Cli,
    ruby: &Ruby,
    words: Option<&[SelectedWord]>,
    in_path: &PathBuf,
    out_path: &PathBuf,
) -> Result<()> {
    let base_font_data =
        fs::read(in_path).with_context(|| anyhow!("Failed to read input file: {in_path:?}"))?;
    let base_file = FileRef::new(&base_font_data)
        .map_err(|e| anyhow!("Failed to parse base font file: {e:?}"))?;

    info!("Processing {in_path:?} -> {out_path:?}");

    let ruby_font_data = if let Some(path) = &cli.font {
        fs::read(path).with_context(|| anyhow!("Failed to read ruby font file: {path:?}"))?
    } else {
        base_font_data.clone()
    };

    let ruby_font_data = Box::<[u8]>::from(ruby_font_data);
    let ruby_file =
        FileRef::new(ruby_font_data.as_ref()).context("Failed to parse ruby font file")?;
    let ruby_fonts: Vec<_> = ruby_file.fonts().collect();

    if ruby_fonts.is_empty() {
        return Err(anyhow!("No fonts found in ruby font file"));
    }

    let ruby_font = ruby_fonts[0]
        .clone()
        .context("Failed to load font from ruby font file")?;

    let renderer: Box<dyn RubyRenderer + '_> = match ruby {
        #[cfg(feature = "pinyin")]
        Ruby::Pinyin => {
            let renderer = renderer::pinyin::PinyinRenderer::new(
                ruby_font,
                cli.scale,
                cli.gutter,
                position_from_str(&cli.position)?,
                cli.offset,
                cli.fit,
                cli.fit_floor,
                cli.tight,
            )?;

            Box::new(renderer)
        }
        #[cfg(feature = "romaji")]
        Ruby::Romaji => {
            let renderer = renderer::romaji::RomajiRenderer::new(
                ruby_font,
                cli.scale,
                cli.gutter,
                position_from_str(&cli.position)?,
                cli.offset,
                cli.fit,
                cli.fit_floor,
                cli.tight,
            )?;

            Box::new(renderer)
        }
        #[cfg(feature = "hiragana")]
        Ruby::Hiragana => {
            let renderer = renderer::hiragana::HiraganaRenderer::new(
                ruby_font,
                cli.scale,
                cli.gutter,
                position_from_str(&cli.position)?,
                cli.offset,
                cli.fit,
                cli.fit_floor,
                cli.tight,
            )?;

            Box::new(renderer)
        }
        // Without renderer features the binary cannot run, but it must build.
        #[allow(unreachable_patterns)]
        _ => unreachable!("no renderer features are enabled"),
    };

    let options = FontProcessing {
        renderer: &*renderer,
        words,
        adjust_metrics: cli.adjust_metrics,
        subset: cli.subset,
        split: cli.split,
    };

    let fonts = rubify::process_font_file(base_file, &options)?;

    let outputs_span = info_span!("write_fonts_to_outputs");
    outputs_span.pb_set_style(
        &ProgressStyle::with_template("{msg} [{wide_bar:.green/cyan}] {pos}/{len}").unwrap(),
    );

    outputs_span.pb_set_length(fonts.len() as u64);
    outputs_span.pb_set_message("Processing font outputs");

    let outputs_span_enter = outputs_span.enter();

    fonts
        .into_par_iter()
        .map(|font| {
            let mut data = font.data;
            let base_path = match font.file_name {
                Some(name) => cli.out.join(name),
                None => out_path.clone(),
            };
            let mut path = base_path;

            outputs_span.pb_inc(1);
            outputs_span.pb_set_message(&format!("Writing {path:?}"));

            #[cfg(feature = "woff2")]
            if cli.woff2 {
                info!("Compressing {path:?}");
                data = rubify::convert_to_woff2(&data)?;
                path = path.with_extension("woff2");
            }

            fs::write(&path, data)
                .with_context(|| anyhow!("Failed to write output file: {path:?}"))?;

            info!("Wrote {path:?}");

            Ok(())
        })
        .collect::<Result<()>>()?;

    drop(outputs_span_enter);
    drop(outputs_span);

    Ok(())
}
