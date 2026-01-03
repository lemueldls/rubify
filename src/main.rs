use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Input font file path
    input: PathBuf,

    /// Output font file path
    output: PathBuf,

    /// Optional font file to use for pinyin characters
    #[arg(long)]
    pinyin_font: Option<PathBuf>,

    /// Subset the font to include only CJK and Pinyin characters
    #[arg(long)]
    subset: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let font_data = fs::read(&cli.input)
        .with_context(|| format!("Failed to read input file: {:?}", cli.input))?;

    let pinyin_font_data = if let Some(path) = &cli.pinyin_font {
        Some(
            fs::read(path)
                .with_context(|| format!("Failed to read pinyin font file: {:?}", path))?,
        )
    } else {
        None
    };

    println!("Processing font...");
    let mut new_font_data = pinyinify::process_font_file(&font_data, pinyin_font_data.as_deref())?;

    if cli.subset {
        println!("Subsetting font...");
        new_font_data = pinyinify::subset_cjk(&new_font_data)?;
    }

    // Infer format from output extension
    let extension = cli
        .output
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase());

    if let Some("woff2") = extension.as_deref() {
        println!("Converting to WOFF2...");
        new_font_data = pinyinify::convert_to_woff2(&new_font_data)?;
    }

    fs::write(&cli.output, new_font_data)
        .with_context(|| format!("Failed to write output file: {:?}", cli.output))?;

    println!("Successfully created pinyin font at {:?}", cli.output);

    Ok(())
}
