use std::{collections::HashSet, fs, path::PathBuf, str::FromStr};

use facet::{Facet, bitflags};
use facet_args as args;
use fontcull_read_fonts::FileRef;
use glob::glob;
use miette::{Error, IntoDiagnostic, Result, WrapErr, miette};
use rubify::renderer::{RubyPosition, RubyRenderer};
use tracing::info;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Facet)]
struct Cli {
    /// Input paths or glob patterns (can be repeated), e.g. 'fonts/*.ttf' or 'file.ttf'
    // #[arg(value_name = "INPUT", num_args = 1..)]
    #[facet(args::positional)]
    inputs: Vec<PathBuf>,

    /// Output file for single input (mutually exclusive with --out-dir)
    // #[arg(long, value_name = "FILE")]
    #[facet(args::named)]
    out: Option<PathBuf>,

    /// Output directory for multiple inputs or globs
    // #[arg(long, value_name = "DIR")]
    #[facet(args::named)]
    out_dir: Option<PathBuf>,

    /// Optional font file to use for ruby characters
    #[facet(args::named)]
    font: Option<PathBuf>,

    /// Override the exported font display name (full name and family)
    #[facet(args::named)]
    display_name: Option<String>,

    /// Ruby characters. Can be repeated to enable multiple sets.
    #[facet(args::named, default = default_characters())]
    chars: String,

    /// Where to place ruby characters relative to base glyph.
    #[facet(args::named, default = RubyPosition::Top)]
    position: RubyPosition,

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
    baseline_offset: f64,

    /// Subset the font to include only annotation characters.
    #[facet(args::named, default = false)]
    subset: bool,
}

bitflags! {
    pub struct RubyCharactersFlags: u8 {
        #[cfg(feature = "pinyin")]
        const PINYIN = 1 << 0;
        #[cfg(feature = "romaji")]
        const ROMAJI = 1 << 1;
    }
}

impl FromStr for RubyCharactersFlags {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let chars = s.split(",");
        let mut flags = RubyCharactersFlags::empty();

        for ch in chars {
            match ch.to_lowercase().as_str() {
                #[cfg(feature = "pinyin")]
                "pinyin" => flags.insert(RubyCharactersFlags::PINYIN),
                #[cfg(feature = "romaji")]
                "romaji" => flags.insert(RubyCharactersFlags::ROMAJI),
                other => {
                    return Err(miette!("Unknown ruby characters argument: {}", other));
                }
            }
        }

        Ok(flags)
    }
}

fn default_characters() -> String {
    let mut parts: Vec<String> = Vec::new();

    #[cfg(feature = "pinyin")]
    parts.push("pinyin".to_string());
    #[cfg(feature = "romaji")]
    parts.push("romaji".to_string());

    parts.join(",")
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

    let cli: Cli = args::from_std_args()?;

    let rb_chars = RubyCharactersFlags::from_str(&cli.chars)
        .wrap_err_with(|| format!("Failed to parse --chars argument: {}", &cli.chars))?;

    let mut input_paths: Vec<PathBuf> = Vec::new();

    for p in &cli.inputs {
        let pattern = p
            .to_str()
            .ok_or_else(|| miette!("Failed to convert path"))?;

        for entry in glob(pattern)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to expand glob pattern: {p:?}"))?
        {
            match entry {
                Ok(path) if path.is_file() => input_paths.push(path),
                _ => {}
            }
        }
    }

    // Determine output behavior
    if input_paths.len() == 1 {
        // single input
        let in_path = &input_paths[0];

        let out_path = if let Some(out_file) = &cli.out {
            out_file
        } else if let Some(dir) = &cli.out_dir {
            let file_name = in_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| miette!("Invalid file name"))?;

            &dir.join(file_name)
        } else {
            // default: same dir, append -ruby before extension
            let stem = in_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            let ext = in_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("ttf");

            &in_path.with_file_name(format!("{}-ruby.{}", stem, ext))
        };

        process_font(&cli, rb_chars, in_path, out_path)?;
    } else {
        // multiple inputs: require out_dir
        let out_dir = cli.out_dir.as_ref().ok_or_else(|| {
            miette!("--out-dir must be provided when processing multiple inputs/globs")
        })?;

        if !out_dir.exists() {
            fs::create_dir_all(out_dir)
                .into_diagnostic()
                .wrap_err_with(|| format!("Failed to create out-dir: {:?}", out_dir))?;
        }

        info!(
            "Processing {} inputs -> {:?}...",
            input_paths.len(),
            out_dir
        );

        for in_path in &input_paths {
            let file_name = in_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| miette!("Invalid file name"))?
                .to_string();

            // // Convert to woff2 if requested
            // let out_name = if cli.woff2 {
            //     info!("Converting {:?} to WOFF2...", in_path);
            //     new_font_data = rubify::convert_to_woff2(&new_font_data)?;
            //     let stem = in_path
            //         .file_stem()
            //         .and_then(|s| s.to_str())
            //         .unwrap_or(&file_name);
            //     format!("{}.woff2", stem)
            // } else {
            //     file_name.clone()
            // };

            let out_path = out_dir.join(file_name);

            process_font(&cli, rb_chars, in_path, &out_path)?;
        }

        info!("Done processing inputs.");
    }

    Ok(())
}

fn process_font(
    cli: &Cli,
    rb_chars: RubyCharactersFlags,
    in_path: &PathBuf,
    out_path: &PathBuf,
) -> Result<(), miette::Error> {
    let base_font_data = fs::read(in_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to read input file: {:?}", in_path))?;
    let base_file = FileRef::new(&base_font_data)
        .map_err(|e| miette!("Failed to parse base font file: {:?}", e))?;

    info!("Processing {:?} -> {:?}...", in_path, out_path);

    let mut renderers: Vec<Box<dyn RubyRenderer>> = Vec::new();

    let ruby_font_data = if let Some(path) = &cli.font {
        fs::read(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read ruby font file: {:?}", path))?
    } else {
        base_font_data.clone()
    };
    let ruby_font_data = Box::leak(ruby_font_data.into_boxed_slice());
    let ruby_file = FileRef::new(ruby_font_data)
        .map_err(|e| miette!("Failed to parse ruby font file: {:?}", e))?;
    let ruby_fonts: Vec<_> = ruby_file.fonts().collect();

    if ruby_fonts.is_empty() {
        return Err(miette!("No fonts found in ruby font file"));
    }

    let ruby_font = ruby_fonts[0]
        .as_ref()
        .map_err(|e| miette!("Failed to load font from ruby font file: {:?}", e))?;

    #[cfg(feature = "pinyin")]
    if rb_chars.contains(RubyCharactersFlags::PINYIN) {
        let renderer = rubify::renderer::pinyin::PinyinRenderer::new(
            ruby_font.clone(),
            cli.scale,
            cli.gutter,
            cli.position,
            cli.baseline_offset,
            cli.tight,
        )?;

        renderers.push(Box::new(renderer));
    }

    #[cfg(feature = "romaji")]
    if rb_chars.contains(RubyCharactersFlags::ROMAJI) {
        let renderer = rubify::renderer::romaji::RomajiRenderer::new(
            ruby_font.clone(),
            cli.scale,
            cli.gutter,
            cli.position,
            cli.baseline_offset,
            cli.tight,
        )?;

        renderers.push(Box::new(renderer));
    }

    let char_ranges = renderers
        .iter()
        .flat_map(|r| r.ranges().iter().cloned())
        .collect::<Vec<_>>();

    let mut new_font_data = rubify::process_font_file(
        base_file,
        &char_ranges,
        &renderers,
        cli.subset,
        cli.display_name.as_deref(),
    )?;

    if cli.subset {
        info!("Subsetting font...");
        new_font_data = rubify::subset_by_renderers(&new_font_data, &renderers)?;
    }

    // let extension = out_path
    //     .extension()
    //     .and_then(|ext| ext.to_str())
    //     .map(|ext| ext.to_lowercase());

    // if let Some("woff2") = extension.as_deref() {
    //     info!("Converting to WOFF2...");
    //     new_font_data = rubify::convert_to_woff2(&new_font_data)?;
    // }

    fs::write(&out_path, new_font_data)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to write output file: {:?}", out_path))?;

    info!("Wrote {:?}", out_path);

    Ok(())
}
