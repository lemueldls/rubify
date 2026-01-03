use clap::{Parser, ValueEnum};
use std::fs;
use std::path::PathBuf;
use anyhow::{Context, Result};

#[derive(Clone, ValueEnum, Debug)]
enum Format {
    Ttf,
    Woff2,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Input font file path
    #[arg(short, long)]
    input: PathBuf,

    /// Output font file path
    #[arg(short, long)]
    output: PathBuf,

    /// Optional font file to use for pinyin characters
    #[arg(long)]
    pinyin_font: Option<PathBuf>,

    /// Subset the font to include only CJK and Pinyin characters
    #[arg(long)]
    subset: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Ttf)]
    format: Format,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let font_data = fs::read(&cli.input)
        .with_context(|| format!("Failed to read input file: {:?}", cli.input))?;

    let pinyin_font_data = if let Some(path) = &cli.pinyin_font {
        Some(fs::read(path).with_context(|| format!("Failed to read pinyin font file: {:?}", path))?)
    } else {
        None
    };

    println!("Processing font...");
    let mut new_font_data = pinyinify::process_font_file(&font_data, pinyin_font_data.as_deref())?;

    if cli.subset {
        println!("Subsetting font...");
        new_font_data = pinyinify::subset_cjk(&new_font_data)?;
    }

    match cli.format {
        Format::Woff2 => {
            println!("Converting to WOFF2...");
            new_font_data = pinyinify::convert_to_woff2(&new_font_data)?;
        },
        Format::Ttf => {}
    }

    fs::write(&cli.output, new_font_data)
        .with_context(|| format!("Failed to write output file: {:?}", cli.output))?;

    println!("Successfully created pinyin font at {:?}", cli.output);
    Ok(())
}