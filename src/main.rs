use std::{fs, path::PathBuf, str::FromStr};

use anyhow::{Context, Error, Result, anyhow};
use facet::Facet;
use figue::{self as args, FigueBuiltins};
use fontcull_read_fonts::FileRef;
use glob::glob;
use indicatif::ProgressStyle;
use rubify::renderer::{self, RubyPosition, RubyRenderer};
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

    /// Ruby characters. Can be repeated to enable multiple sets.
    #[facet(args::named)]
    ruby: String,

    /// Separate font file to use for ruby characters
    #[facet(args::named)]
    font: Option<PathBuf>,

    /// Subset the font to include only annotation characters.
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

    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

pub enum Ruby {
    #[cfg(feature = "pinyin")]
    Pinyin,
    #[cfg(feature = "romaji")]
    Romaji,
}

impl FromStr for Ruby {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            #[cfg(feature = "pinyin")]
            "pinyin" => Ok(Ruby::Pinyin),
            #[cfg(feature = "romaji")]
            "romaji" => Ok(Ruby::Romaji),
            other => {
                return Err(anyhow!("Unknown ruby characters argument: {other}"));
            }
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
        .with_context(|| anyhow!("Failed to parse --ruby argument: {}", &cli.ruby))?;

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
        inputs_span.pb_set_message(&format!("Processing {}", in_path.display()));

        let file_name = in_path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid file name")?
            .to_string();

        let out_path = cli.out.join(file_name);

        process_file(&cli, &ruby, &in_path, &out_path)?;
    }

    drop(inputs_span_enter);
    drop(inputs_span);

    info!("Done processing inputs.");

    Ok(())
}

fn process_file(cli: &Cli, ruby: &Ruby, in_path: &PathBuf, out_path: &PathBuf) -> Result<()> {
    let base_font_data =
        fs::read(in_path).with_context(|| anyhow!("Failed to read input file: {in_path:?}"))?;
    let base_file = FileRef::new(&base_font_data)
        .map_err(|e| anyhow!("Failed to parse base font file: {:?}", e))?;

    info!("Processing {:?} -> {:?}", in_path, out_path);

    let ruby_font_data = if let Some(path) = &cli.font {
        fs::read(path).with_context(|| anyhow!("Failed to read ruby font file: {path:?}"))?
    } else {
        base_font_data.clone()
    };

    let ruby_font_data = Box::leak(ruby_font_data.into_boxed_slice());
    let ruby_file = FileRef::new(ruby_font_data).context("Failed to parse ruby font file")?;
    let ruby_fonts: Vec<_> = ruby_file.fonts().collect();

    if ruby_fonts.is_empty() {
        return Err(anyhow!("No fonts found in ruby font file"));
    }

    let ruby_font = ruby_fonts[0]
        .clone()
        .context("Failed to load font from ruby font file")?;

    let renderer: Box<dyn RubyRenderer> = match ruby {
        #[cfg(feature = "pinyin")]
        Ruby::Pinyin => {
            let renderer = renderer::pinyin::PinyinRenderer::new(
                ruby_font,
                cli.scale,
                cli.gutter,
                position_from_str(&cli.position)?,
                cli.offset,
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
                cli.tight,
            )?;

            Box::new(renderer)
        }
    };

    let fonts = rubify::process_font_file(base_file, &renderer, cli.subset, cli.split)?;

    for font in fonts {
        let mut data = font.data;
        let mut path = out_path.to_owned();

        #[cfg(feature = "woff2")]
        if cli.woff2 {
            info!("Converting to WOFF2");
            data = rubify::convert_to_woff2(&data)?;
            path = out_path.with_extension("woff2");
        }

        fs::write(&path, data).with_context(|| anyhow!("Failed to write output file: {path:?}"))?;

        info!("Wrote {path:?}");
    }

    Ok(())
}
