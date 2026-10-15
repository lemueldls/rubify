//! Prints glyph id, advance and bounding box for characters of a font:
//! `cargo run --example charinfo -- <font> <chars...>`.

use fontcull_read_fonts::{FontRef, TableProvider};
use fontcull_skrifa::{MetadataProvider, instance::Size};
use kurbo::Shape;
use rubify::pen::PathPen;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("usage: charinfo <font> <chars...>");
        std::process::exit(1);
    }

    let data = std::fs::read(&args[1]).expect("failed to read font");
    let font = FontRef::new(&data).expect("failed to parse font");
    let cmap = font.charmap();
    let hmtx = font.hmtx().expect("no hmtx");
    let outlines = font.outline_glyphs();
    let upem = font.head().map(|head| head.units_per_em()).unwrap_or(1000);

    println!("upem: {upem}");

    for arg in &args[2..] {
        for ch in arg.chars() {
            let Some(gid) = cmap.map(ch) else {
                println!("U+{:04X} ({ch}): no glyph", ch as u32);
                continue;
            };

            let advance = hmtx
                .h_metrics()
                .get(gid.to_u32() as usize)
                .map(|m| m.advance.get())
                .unwrap_or(0);

            let bbox = outlines.get(gid).and_then(|glyph| {
                let mut pen = PathPen::new();
                glyph.draw(Size::unscaled(), &mut pen).ok()?;

                if pen.path.elements().is_empty() {
                    None
                } else {
                    Some(pen.path.bounding_box())
                }
            });

            match bbox {
                Some(bbox) => println!(
                    "U+{:04X} ({ch}): gid {} advance {} bbox {}..{}",
                    ch as u32,
                    gid.to_u32(),
                    advance,
                    bbox.x0 as i32,
                    bbox.x1 as i32
                ),
                None => println!(
                    "U+{:04X} ({ch}): gid {} advance {} no outline",
                    ch as u32,
                    gid.to_u32(),
                    advance
                ),
            }
        }
    }
}
