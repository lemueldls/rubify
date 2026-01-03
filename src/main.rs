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
    /// Input paths or glob patterns (can be repeated), e.g. 'fonts/*.ttf' or 'file.ttf'
    #[arg(value_name = "INPUT", num_args = 1..)]
    inputs: Vec<String>,

    /// Output file for single input (mutually exclusive with --out-dir)
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Output directory for multiple inputs or globs
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Optional font file to use for ruby characters
    #[arg(long)]
    font: Option<PathBuf>,

    /// Force converting all outputs to WOFF2 when using glob/directory mode
    #[arg(long)]
    woff2: bool,

    /// Override the exported font display name (full name and family)
    #[arg(long)]
    display_name: Option<String>,

    /// Choose character set(s): `none` or `pinyin`. Can be repeated to enable multiple sets.
    #[arg(long, value_enum, default_values_t = [CharactersArg::Pinyin])]
    characters: Vec<CharactersArg>,

    /// Where to place ruby text relative to base glyph.
    #[arg(long, value_enum, default_value_t = RubyPositionArg::Top)]
    position: RubyPositionArg,

    /// Scale ratio for ruby text (fraction of main font size).
    #[arg(long, default_value_t = 0.3)]
    scale: f64,

    /// Gutter (in em) between base glyph and ruby text.
    #[arg(long, default_value_t = 0.1)]
    gutter: f64,

    /// Delimiter string to split ruby text into parts (must be single character)
    #[arg(long)]
    delimiter: Option<String>,

    /// Spacing (in em) to insert between parts when delimiter is used.
    #[arg(long, default_value_t = 0.0)]
    spacing: f64,

    /// Subset the font to include only CJK and Pinyin characters
    #[arg(long)]
    subset: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

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

    // We'll keep the ruby font data and renderer options so we can construct
    // renderers per-input when processing files (this supports batch directory mode).
    let pinyin_font_bytes = ruby_font_data.clone();

    let position = match cli.position {
        RubyPositionArg::Top => rubify::renderer::RubyPosition::Top,
        RubyPositionArg::Bottom => rubify::renderer::RubyPosition::Bottom,
        RubyPositionArg::LeftDown => rubify::renderer::RubyPosition::LeftDown,
        RubyPositionArg::LeftUp => rubify::renderer::RubyPosition::LeftUp,
        RubyPositionArg::RightDown => rubify::renderer::RubyPosition::RightDown,
        RubyPositionArg::RightUp => rubify::renderer::RubyPosition::RightUp,
    };

    // Helper to build renderers for each file when needed
    let build_renderers = |chars: &HashSet<CharactersArg>| -> Result<Vec<rubify::renderer::pinyin::PinyinRenderer<'static>>> {
        let mut result: Vec<rubify::renderer::pinyin::PinyinRenderer<'static>> = Vec::new();

        if chars.contains(&CharactersArg::Pinyin) {
            let data = pinyin_font_bytes
                .as_ref()
                .ok_or_else(|| anyhow!("Pinyin font data is required for Pinyin renderer"))?;

            // Leaked static slice so we can create a FontRef with 'static lifetime for the renderer
            let leaked: &'static [u8] = Box::leak(data.clone().into_boxed_slice());
            let pfile2 = FileRef::new(leaked).map_err(|e| anyhow!("Failed to parse ruby font file: {:?}", e))?;
            let pfonts2: Vec<_> = pfile2.fonts().collect();

            if pfonts2.is_empty() {
                return Err(anyhow!("No fonts found in ruby font file"));
            }

            let pfont2 = pfonts2[0].clone().map_err(|e| anyhow!("Failed to load font from ruby font file: {:?}", e))?;

            let renderer = rubify::renderer::pinyin::PinyinRenderer::new(
                pfont2,
                cli.scale,
                cli.gutter,
                delimiter_char,
                cli.spacing,
                position,
            )?;

            result.push(renderer);
        }

        Ok(result)
    };

    // Expand input patterns (globs, directories, single files)
    use glob::glob;

    fn expand_inputs(patterns: &[String]) -> Result<Vec<PathBuf>> {
        let mut out: Vec<PathBuf> = Vec::new();

        for p in patterns {
            if p.contains('*') || p.contains('?') || p.contains('[') {
                for entry in
                    glob(p).with_context(|| format!("Failed to expand glob pattern: {}", p))?
                {
                    match entry {
                        Ok(path) if path.is_file() => out.push(path),
                        _ => {}
                    }
                }
            } else {
                let pb = PathBuf::from(p);
                if pb.is_dir() {
                    for entry in fs::read_dir(&pb)
                        .with_context(|| format!("Failed to read dir: {:?}", pb))?
                    {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_file() {
                            out.push(path);
                        }
                    }
                } else if pb.is_file() {
                    out.push(pb);
                } else {
                    return Err(anyhow!(
                        "Input path does not exist or is not a file/dir: {}",
                        p
                    ));
                }
            }
        }

        if out.is_empty() {
            return Err(anyhow!("No input files matched"));
        }

        Ok(out)
    }

    let input_paths = expand_inputs(&cli.inputs)?;

    // Determine output behavior
    if input_paths.len() == 1 {
        // single input
        let in_path = &input_paths[0];

        let out_path: PathBuf = if let Some(ref out_file) = cli.out {
            out_file.clone()
        } else if let Some(ref dir) = cli.out_dir {
            let file_name = in_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("Invalid file name"))?;
            dir.join(file_name)
        } else {
            // default: same dir, append -pinyin before extension
            let stem = in_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            let ext = in_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("ttf");
            in_path.with_file_name(format!("{}-pinyin.{}", stem, ext))
        };

        let font_data = fs::read(in_path)
            .with_context(|| format!("Failed to read input file: {:?}", in_path))?;

        println!("Processing {:?} -> {:?}...", in_path, out_path);

        let mut new_font_data = rubify::process_font_file(
            &font_data,
            build_renderers(&characters)?,
            cli.display_name.as_deref(),
        )?;

        if cli.subset {
            println!("Subsetting font...");
            new_font_data = rubify::subset_cjk(&new_font_data)?;
        }

        // Infer format from output extension
        let extension = out_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_lowercase());

        if let Some("woff2") = extension.as_deref() {
            println!("Converting to WOFF2...");
            new_font_data = rubify::convert_to_woff2(&new_font_data)?;
        }

        fs::write(&out_path, new_font_data)
            .with_context(|| format!("Failed to write output file: {:?}", out_path))?;

        println!("Wrote {:?}", out_path);
    } else {
        // multiple inputs: require out_dir
        let out_dir = cli
            .out_dir
            .as_ref()
            .with_context(|| "--out-dir must be provided when processing multiple inputs/globs")?;

        if !out_dir.exists() {
            fs::create_dir_all(out_dir)
                .with_context(|| format!("Failed to create out-dir: {:?}", out_dir))?;
        }

        println!(
            "Processing {} inputs -> {:?}...",
            input_paths.len(),
            out_dir
        );

        for path in input_paths {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("Invalid file name"))?
                .to_string();

            let font_data = fs::read(&path)
                .with_context(|| format!("Failed to read input file: {:?}", path))?;

            let mut new_font_data = rubify::process_font_file(
                &font_data,
                build_renderers(&characters)?,
                cli.display_name.as_deref(),
            )?;

            if cli.subset {
                println!("Subsetting font {:?}...", path);
                new_font_data = rubify::subset_cjk(&new_font_data)?;
            }

            // Convert to woff2 if requested
            let out_name = if cli.woff2 {
                println!("Converting {:?} to WOFF2...", path);
                new_font_data = rubify::convert_to_woff2(&new_font_data)?;
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&file_name);
                format!("{}.woff2", stem)
            } else {
                file_name.clone()
            };

            let out_path = out_dir.join(out_name);

            fs::write(&out_path, new_font_data)
                .with_context(|| format!("Failed to write output file: {:?}", out_path))?;

            println!("Wrote {:?}", out_path);
        }

        println!("Done processing inputs.");
    }
    Ok(())
}
