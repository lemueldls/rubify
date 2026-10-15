//! Prints what the bundled dictionaries contain: `cargo run --example dictinfo`.

use rubify::dictionary::{Lang, load_bundled};

fn main() {
    for lang in [Lang::Ja, Lang::Zh] {
        let Some(dict) = load_bundled(lang) else {
            println!("{}: not bundled", lang.name());
            continue;
        };

        println!(
            "{}: {} entries, {} defaults, {} bare",
            lang.name(),
            dict.entries.len(),
            dict.defaults.len(),
            dict.bare.len()
        );
        println!("  sources: {:?}", dict.sources);

        for default in dict.defaults.iter().take(10) {
            println!("  default: {default}");
        }

        for bare in dict.bare.iter().take(10) {
            println!("  bare: {bare}");
        }
    }
}
