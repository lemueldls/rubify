use std::{collections::HashSet, fs, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use read_fonts::FileRef;

#[derive(Clone, ValueEnum, PartialEq, Eq, Hash)]
#[repr(u8)]
enum CharactersArg {
    None,
    Pinyin,
}

#[derive(Clone, ValueEnum, Debug)]
enum RubyPositionArg {
    Top,
    Bottom,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Input font file path
    input: PathBuf,

    /// Output font file path
    output: PathBuf,

    /// Optional font file to use for ruby characters
    #[arg(long)]
    font: Option<PathBuf>,

    /// Choose character set(s): `none` or `pinyin`. Can be repeated to enable multiple sets. Default: `pinyin`
    #[arg(long, value_enum, default_values_t = [CharactersArg::Pinyin])]
    characters: Vec<CharactersArg>,

    /// Where to place ruby text relative to base glyph. Default: `Top`
    #[arg(long, value_enum, default_value_t = RubyPositionArg::Top)]
    position: RubyPositionArg,

    /// Scale ratio for ruby text (fraction of main font size). Default: 0.3
    #[arg(long, default_value_t = 0.3)]
    scale: f64,

    /// Gutter (in em) between base glyph and ruby text. Default: 0.075
    #[arg(long, default_value_t = 0.075)]
    gutter: f64,

    /// Delimiter string to split ruby text into parts (must be single character)
    #[arg(long)]
    delimiter: Option<String>,

    /// Spacing (in em) to insert between parts when delimiter is used. Default: 0.0
    #[arg(long, default_value_t = 0.0)]
    spacing: f64,

    /// Subset the font to include only CJK and Pinyin characters
    #[arg(long)]
    subset: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let font_data = fs::read(&cli.input)
        .with_context(|| format!("Failed to read input file: {:?}", cli.input))?;

    let ruby_font_data = if let Some(path) = &cli.font {
        Some(fs::read(path).with_context(|| format!("Failed to read ruby font file: {:?}", path))?)
    } else {
        None
    };

    // convert delimiter string to Option<char> (must be single char)
    let delimiter_char: Option<char> = match cli.delimiter {
        Some(ref s) if s.chars().count() == 1 => s.chars().next(),
        Some(ref s) => {
            eprintln!(
                "Warning: --delimiter must be a single character; ignoring `{}`",
                s
            );
            None
        }
        None => None,
    };

    let characters = HashSet::<CharactersArg>::from_iter(cli.characters);
    let mut renderers = Vec::new();

    if characters.contains(&CharactersArg::Pinyin) {
        let font_ref =
            if let Some(data) = &ruby_font_data {
                let pfile = FileRef::new(data)
                    .map_err(|e| anyhow!("Failed to parse ruby font file: {:?}", e))?;
                let pfonts: Vec<_> = pfile.fonts().collect();

                if pfonts.is_empty() {
                    return Err(anyhow!("No fonts found in ruby font file"));
                }

                Some(pfonts[0].clone().map_err(|e| {
                    anyhow!("Failed to load first font from ruby font file: {:?}", e)
                })?)
            } else {
                None
            };

        let position = match cli.position {
            RubyPositionArg::Top => rubify::renderer::RubyPosition::Top,
            RubyPositionArg::Bottom => rubify::renderer::RubyPosition::Bottom,
            RubyPositionArg::LeftDown => rubify::renderer::RubyPosition::LeftDown,
            RubyPositionArg::LeftUp => rubify::renderer::RubyPosition::LeftUp,
            RubyPositionArg::RightDown => rubify::renderer::RubyPosition::RightDown,
            RubyPositionArg::RightUp => rubify::renderer::RubyPosition::RightUp,
        };

        let renderer = rubify::renderer::pinyin::PinyinRenderer::new(
            font_ref.expect("Pinyin font data is required for Pinyin renderer"),
            cli.scale,
            cli.gutter,
            delimiter_char,
            cli.spacing,
            position,
        )
        .expect("Failed to create Pinyin renderer");

        renderers.push(renderer);
    }

    println!("Processing font...");
    let mut new_font_data = rubify::process_font_file(&font_data, renderers)?;

    if cli.subset {
        println!("Subsetting font...");
        new_font_data = rubify::subset_cjk(&new_font_data)?;
    }

    // Infer format from output extension
    let extension = cli
        .output
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase());

    if let Some("woff2") = extension.as_deref() {
        println!("Converting to WOFF2...");
        new_font_data = rubify::convert_to_woff2(&new_font_data)?;
    }

    fs::write(&cli.output, new_font_data)
        .with_context(|| format!("Failed to write output file: {:?}", cli.output))?;

    println!("Successfully created pinyin font at {:?}", cli.output);

    Ok(())
}
