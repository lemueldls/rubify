//! Shaping test: `cargo run --example shape -- <font> <text>`.
//!
//! Prints the glyph ids a shaper picks, to check that word ligatures and
//! contextual rules fire.

use std::fs;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("usage: shape <font> <text>");
        std::process::exit(1);
    }

    let data = fs::read(&args[1]).expect("failed to read font");
    let face = rustybuzz::Face::from_slice(&data, 0).expect("failed to parse font");
    let parsed = ttf_parser::Face::parse(&data, 0).expect("failed to parse font with ttf-parser");

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(&args[2]);

    let glyphs = rustybuzz::shape(&face, &[], buffer);

    println!("upem: {}", face.units_per_em());

    for (info, pos) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
        let bbox = parsed
            .glyph_bounding_box(ttf_parser::GlyphId(info.glyph_id as u16))
            .map(|bbox| {
                format!(
                    "x {}..{} y {}..{}",
                    bbox.x_min, bbox.x_max, bbox.y_min, bbox.y_max
                )
            })
            .unwrap_or_else(|| "no outline".to_string());

        println!(
            "gid={:5} cluster={:2} advance={:5} {bbox}",
            info.glyph_id, info.cluster, pos.x_advance
        );
    }
}
