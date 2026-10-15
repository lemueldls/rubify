#![cfg(feature = "hiragana")]

//! End-to-end shaping tests on a tiny generated font.
//!
//! The font maps a handful of characters to box glyphs; after rubify processes
//! it, rustybuzz must pick the word annotations through GSUB.

use fontcull_read_fonts::{FontRef, TopLevelTable};
use fontcull_write_fonts::{
    FontBuilder, dump_table,
    tables::{
        cmap::Cmap,
        glyf::Glyf,
        glyf::SimpleGlyph,
        head::Head,
        hhea::Hhea,
        hmtx::{Hmtx, LongMetric},
        loca::Loca,
        maxp::Maxp,
    },
};
use kurbo::{Rect, Shape};
use rubify::{
    process_font_ref_with,
    renderer::{RubyPosition, hiragana::HiraganaRenderer},
    words::{SelectedWord, parse_furigana},
};

/// Glyphs of the test font, in gid order (gid 0 is `.notdef`).
const CHARS: &[char] = &[
    '日', '本', '今', '語', 'に', 'ほ', 'ん', 'き', 'ょ', 'う', 'ご',
];

fn build_font() -> Vec<u8> {
    let num_glyphs = 1 + CHARS.len();
    let mut glyf_bytes: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = vec![0];

    let box_path = || Rect::new(100.0, 0.0, 900.0, 800.0).to_path(0.1);

    // gid 0: an empty .notdef
    for index in 0..num_glyphs {
        if index > 0 {
            let path = box_path();
            let simple = SimpleGlyph::from_bezpath(&path).unwrap();
            glyf_bytes.extend_from_slice(&dump_table(&simple).unwrap());
        }

        offsets.push(glyf_bytes.len() as u32);
    }

    let mut head = Head {
        units_per_em: 1000,
        magic_number: 0x5F0F_3CF5,
        ..Head::default()
    };

    let loca = Loca::new(offsets);
    head.index_to_loc_format = loca.format() as i16;

    let hhea = Hhea {
        ascender: fontcull_write_fonts::types::FWord::new(900),
        descender: fontcull_write_fonts::types::FWord::new(-100),
        number_of_h_metrics: num_glyphs as u16,
        advance_width_max: fontcull_write_fonts::types::UfWord::new(1000),
        ..Hhea::default()
    };

    let maxp = Maxp::new(num_glyphs as u16);

    let metrics = Hmtx::new(
        (0..num_glyphs)
            .map(|_| LongMetric::new(1000, 100))
            .collect(),
        Vec::new(),
    );

    let cmap = Cmap::from_mappings(CHARS.iter().enumerate().map(|(index, ch)| {
        (
            *ch,
            fontcull_write_fonts::types::GlyphId::new(index as u32 + 1),
        )
    }))
    .unwrap();

    let mut builder = FontBuilder::new();
    builder.add_table(&head).unwrap();
    builder.add_table(&hhea).unwrap();
    builder.add_table(&maxp).unwrap();
    builder.add_table(&metrics).unwrap();
    builder.add_table(&cmap).unwrap();
    builder.add_raw(Glyf::TAG, glyf_bytes);
    builder.add_raw(Loca::TAG, dump_table(&loca).unwrap());
    builder.build()
}

fn shape(data: &[u8], text: &str) -> Vec<(u32, i32)> {
    let face = rustybuzz::Face::from_slice(data, 0).expect("output font parses");

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);

    let glyphs = rustybuzz::shape(&face, &[], buffer);

    glyphs
        .glyph_infos()
        .iter()
        .zip(glyphs.glyph_positions())
        .map(|(info, pos)| (info.glyph_id, pos.x_advance))
        .collect()
}

fn process(words: &[SelectedWord]) -> Vec<u8> {
    let data = build_font();
    let font = FontRef::new(&data).unwrap();
    let ruby_font = font.clone();

    let renderer = HiraganaRenderer::new(
        ruby_font,
        0.4,
        0.0,
        RubyPosition::Top,
        0.0,
        false,
        0.0,
        false,
    )
    .unwrap();

    process_font_ref_with(&font, &renderer, Some(words), false).unwrap()
}

#[test]
fn grouped_reading_becomes_one_ligature() {
    let words = vec![SelectedWord {
        form: "日本".into(),
        spans: parse_furigana("0-1:にほん"),
    }];

    let output = process(&words);

    let shaped = shape(&output, "日本");
    assert_eq!(shaped.len(), 1, "日本 is one ligature glyph");
    assert_eq!(shaped[0].1, 2000, "the ligature keeps both advances");

    // A character no word matches is untouched.
    let shaped = shape(&output, "日");
    assert_eq!(shaped, vec![(1, 1000)]);

    let shaped = shape(&output, "今");
    assert_eq!(shaped, vec![(3, 1000)]);
}

#[test]
fn aligned_readings_substitute_in_context() {
    // 日本 reads に+ほん per character; 日本語 is one ligature. The longest
    // match must win.
    let words = vec![
        SelectedWord {
            form: "日本".into(),
            spans: parse_furigana("0:に;1:ほん"),
        },
        SelectedWord {
            form: "日本語".into(),
            spans: parse_furigana("0:に;1-2:ほんご"),
        },
    ];

    let output = process(&words);

    let shaped = shape(&output, "日本");
    assert_eq!(shaped.len(), 2, "日本 stays two glyphs, both substituted");
    assert!(shaped.iter().all(|(gid, _)| *gid > 2));
    assert!(shaped.iter().all(|(_, advance)| *advance == 1000));

    let shaped = shape(&output, "日本語");
    assert_eq!(shaped.len(), 1, "日本語 matches as a whole");
    assert_eq!(shaped[0].1, 3000);
}

#[test]
fn annotations_draw_above_the_base() {
    let words = vec![SelectedWord {
        form: "今日".into(),
        spans: parse_furigana("0-1:きょう"),
    }];

    let output = process(&words);
    let parsed = ttf_parser::Face::parse(&output, 0).unwrap();

    let shaped = shape(&output, "今日");
    assert_eq!(shaped.len(), 1);

    let bbox = parsed
        .glyph_bounding_box(ttf_parser::GlyphId(shaped[0].0 as u16))
        .unwrap();

    // The base boxes reach y 800; the ruby must be above them.
    assert!(bbox.y_max > 1000, "ruby ink above the em box: {bbox:?}");
}
