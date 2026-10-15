//! Repository chores.
//!
//! - `cargo xtask update-dict` downloads the pinned upstream data, compiles
//!   both into the compact blobs embedded by the `rubify` crate, and writes
//!   them under `data/`.
//! - `cargo xtask example` renders the README's `example.png` from a pinned
//!   font, exercising the real annotation pipeline end to end.

mod example;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use rubify::{
    dictionary::{Dictionary, DictionaryEntry, DictionaryReading, encode},
    words::tag_bit,
};

const FURIFONT_TAG: &str = "v0.3.0";
const FURIFONT_BASE: &str =
    "https://raw.githubusercontent.com/antfu/furifont/v0.3.0/packages/dictionary";
const CEDICT_URL: &str =
    "https://www.mdbg.net/chinese/export/cedict/cedict_1_0_ts_utf-8_mdbg.txt.gz";

/// The base font for `example.png` (SIL Open Font License 1.1).
pub(crate) const EXAMPLE_FONT: &str = "LXGWWenKai-Regular.ttf";
pub(crate) const EXAMPLE_FONT_URL: &str =
    "https://github.com/lxgw/LxgwWenKai/releases/download/v1.522/LXGWWenKai-Regular.ttf";

/// SHA-256 of the pinned downloads.
const HASHES: &[(&str, &str)] = &[
    (
        "dictionary.json",
        "7a345f16dd51895563bd21f40a17fa9f68248b14f852075c0ecb31af4b625a0d",
    ),
    (
        "places.json",
        "072d71181579abdcbce8af01d20953f4162212decaa8a5473effea7411d13f98",
    ),
    (
        "jlpt.json",
        "8356324ab6e1d6685832bae13df33900afe6078ddd2b130b685d0f3362891a50",
    ),
    (
        "tatoeba.json",
        "cac5ba4448c49a8410cd68e48ff47b00d292ab8051764b13128bf4c6cb72f87e",
    ),
    (
        "overrides.ts",
        "b1963e6dd5082ba532c43a75a638d9ff0874c37bddaa80d60fedfc93676d754f",
    ),
    (
        "LXGWWenKai-Regular.ttf",
        "39ad71264b588165b469e35e6afb162a378dacd1f95348160240ba9038ac3009",
    ),
];

fn main() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_default();

    match command.as_str() {
        "update-dict" => update_dict(),
        "example" => example::run(),
        other => bail!("unknown command {other:?}; usage: xtask update-dict | example"),
    }
}

pub(crate) fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);

    base.join("rubify-dict")
}

pub(crate) fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the project")
        .to_path_buf()
}

/// Download `url` into the cache unless it is already there, then verify it
/// against the pinned hash when one is known.
pub(crate) fn fetch(cache: &Path, name: &str, url: &str) -> Result<PathBuf> {
    fs::create_dir_all(cache).context("Failed to create the cache directory")?;
    let path = cache.join(name);

    if !path.exists() || fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true) {
        println!("Downloading {url}");

        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&path)
            .arg(url)
            .status()
            .context("Failed to run curl")?;

        if !status.success() {
            bail!("curl failed for {url}");
        }
    }

    if let Some((_, expected)) = HASHES.iter().find(|(file, _)| *file == name) {
        let output = Command::new("sha256sum")
            .arg(&path)
            .output()
            .context("Failed to run sha256sum")?;
        let hash = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();

        if hash != *expected {
            bail!("{name} has hash {hash}, expected {expected}: the pinned data changed");
        }
    }

    Ok(path)
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let data = fs::read(path).with_context(|| format!("Failed to read {path:?}"))?;
    serde_json::from_slice(&data).with_context(|| format!("Failed to parse {path:?}"))
}

fn update_dict() -> Result<()> {
    let cache = cache_dir();
    let root = project_root();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).context("Failed to create the data directory")?;

    let ja = compile_ja(&cache)?;
    let ja_bytes = encode(&ja)?;
    fs::write(data_dir.join("dict-ja.bin"), &ja_bytes).context("Failed to write dict-ja.bin")?;
    println!(
        "dict-ja.bin: {} entries, {} bytes",
        ja.entries.len(),
        ja_bytes.len()
    );

    let zh = compile_zh(&cache)?;
    let zh_bytes = encode(&zh)?;
    fs::write(data_dir.join("dict-zh.bin"), &zh_bytes).context("Failed to write dict-zh.bin")?;
    println!(
        "dict-zh.bin: {} entries, {} bytes",
        zh.entries.len(),
        zh_bytes.len()
    );

    Ok(())
}

fn compile_ja(cache: &Path) -> Result<Dictionary> {
    let dictionary_path = fetch(
        cache,
        "dictionary.json",
        &format!("{FURIFONT_BASE}/data/dictionary.json"),
    )?;
    let places_path = fetch(
        cache,
        "places.json",
        &format!("{FURIFONT_BASE}/data/places.json"),
    )?;
    let jlpt_path = fetch(
        cache,
        "jlpt.json",
        &format!("{FURIFONT_BASE}/data/jlpt.json"),
    )?;
    let tatoeba_path = fetch(
        cache,
        "tatoeba.json",
        &format!("{FURIFONT_BASE}/data/tatoeba.json"),
    )?;
    let overrides_path = fetch(
        cache,
        "overrides.ts",
        &format!("{FURIFONT_BASE}/src/overrides.ts"),
    )?;

    let dictionary = read_json(&dictionary_path)?;
    let jlpt = read_json(&jlpt_path)?;
    let tatoeba = read_json(&tatoeba_path)?;
    let places = read_json(&places_path)?;

    let mut levels: HashMap<(String, String), u8> = HashMap::new();

    for row in jlpt.as_array().context("jlpt.json is not an array")? {
        let row = row.as_array().context("jlpt row is not an array")?;
        let form = row[0].as_str().unwrap_or_default().to_string();
        let words = row[1].as_str().unwrap_or_default().to_string();
        let level = row[2].as_u64().unwrap_or_default() as u8;
        levels.insert((form, words), level);
    }

    let mut uses: HashMap<(String, String), u16> = HashMap::new();

    for row in tatoeba.as_array().context("tatoeba.json is not an array")? {
        let row = row.as_array().context("tatoeba row is not an array")?;
        let form = row[0].as_str().unwrap_or_default().to_string();
        let words = row[1].as_str().unwrap_or_default().to_string();
        let n = row[2].as_u64().unwrap_or_default() as u16;
        uses.insert((form, words), n);
    }

    // One reading per (form, furigana); entries that share the pair merge.
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut entries: Vec<DictionaryEntry> = Vec::new();

    for row in dictionary["entries"]
        .as_array()
        .context("dictionary entries are not an array")?
    {
        let row = row.as_array().context("dictionary entry is not an array")?;
        let form = row[0].as_str().unwrap_or_default().to_string();
        let words = row[1].as_str().unwrap_or_default().to_string();
        let mut nf = row[2].as_u64().unwrap_or_default() as u16;
        let priority = row[3].as_str().unwrap_or_default();
        let order = row[4].as_u64().unwrap_or_default() as u16;
        let conjugates = row.len() > 5;

        let mut tags = 0u16;

        for token in priority.split_whitespace() {
            if let Some(value) = token.strip_prefix("nf")
                && let Ok(value) = value.parse::<u16>()
            {
                if nf == 0 {
                    nf = value;
                }

                continue;
            }

            if let Some(bit) = tag_bit(token) {
                tags |= bit;
            }
        }

        let level = levels.get(&(form.clone(), words.clone())).copied();
        let n = uses.get(&(form.clone(), words.clone())).copied();

        let reading = DictionaryReading {
            words: words.clone(),
            nf,
            jlpt: level.unwrap_or_default(),
            tags,
            order,
            tatoeba: n.unwrap_or_default(),
            common: false,
            pinned: 0,
            conjugates: false,
        };

        match index.get(&(form.clone(), words.clone())) {
            Some(position) => {
                let existing = &mut entries[*position].readings[0];

                // Keep the best evidence: the lowest bucket, the earliest
                // order, the highest counts.
                if reading.nf > 0 && (existing.nf == 0 || reading.nf < existing.nf) {
                    existing.nf = reading.nf;
                }

                existing.order = existing.order.min(reading.order);
                existing.jlpt = existing.jlpt.max(reading.jlpt);
                existing.tatoeba = existing.tatoeba.max(reading.tatoeba);
                existing.tags |= reading.tags;

                if conjugates {
                    existing.conjugates = true;
                }
            }
            None => {
                index.insert((form.clone(), words), entries.len());
                entries.push(DictionaryEntry {
                    form,
                    readings: vec![reading],
                    place: false,
                });
            }
        }
    }

    // Places fill runs no common word claims, so they stay separate entries.
    let mut seen_places: HashMap<String, ()> = HashMap::new();

    for row in places.as_array().context("places.json is not an array")? {
        let row = row.as_array().context("place row is not an array")?;
        let form = row[0].as_str().unwrap_or_default().to_string();
        let words = row[1].as_str().unwrap_or_default().to_string();

        if seen_places.insert(form.clone(), ()).is_some() {
            continue;
        }

        entries.push(DictionaryEntry {
            form,
            readings: vec![DictionaryReading {
                words,
                common: false,
                ..DictionaryReading::default()
            }],
            place: true,
        });
    }

    let overrides = fs::read_to_string(&overrides_path)
        .with_context(|| format!("Failed to read {overrides_path:?}"))?;
    let defaults = extract_string_array(&overrides, "OVERRIDE_READINGS");
    let bare = extract_string_array(&overrides, "BARE_FORMS");

    let mut sources: Vec<(String, String)> = dictionary["sources"]
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();

    sources.push(("furifont".to_string(), FURIFONT_TAG.to_string()));

    Ok(Dictionary {
        sources,
        entries,
        defaults,
        bare,
    })
}

/// Extract `NAME: string[] = ['...', ...]` from a TypeScript source.
fn extract_string_array(source: &str, name: &str) -> Vec<String> {
    let Some(start) = source.find(name) else {
        return Vec::new();
    };
    // The `[` of `string[]` is not the array; skip to the `= [` after it.
    let Some(open) = source[start..].find("= [").map(|offset| start + offset + 2) else {
        return Vec::new();
    };

    let mut out = Vec::new();

    for line in source[open..].lines() {
        let trimmed = line.trim();

        // The entries may contain brackets (readings do); the closing line
        // starts with one.
        if trimmed.starts_with(']') {
            break;
        }

        // Drop comments: an apostrophe in prose is not a string.
        let line = line.split("//").next().unwrap_or(line);
        let mut chars = line.chars();

        while let Some(c) = chars.next() {
            if c != '\'' {
                continue;
            }

            let mut value = String::new();

            while let Some(c) = chars.next() {
                if c == '\'' {
                    break;
                }

                if c == '\\' {
                    if let Some(next) = chars.next() {
                        value.push(next);
                    }
                } else {
                    value.push(c);
                }
            }

            out.push(value);
        }
    }

    out
}

fn compile_zh(cache: &Path) -> Result<Dictionary> {
    let path = fetch(cache, "cedict.txt.gz", CEDICT_URL)?;
    let file = fs::File::open(&path).with_context(|| format!("Failed to open {path:?}"))?;
    let mut decoder = flate2::read::GzDecoder::new(file);
    let mut text = String::new();
    decoder
        .read_to_string(&mut text)
        .context("Failed to decompress CC-CEDICT")?;

    let mut entries: BTreeMap<String, Vec<DictionaryReading>> = BTreeMap::new();
    let mut converted = 0usize;
    let mut skipped = 0usize;

    for line in text.lines() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((traditional, rest)) = line.split_once(' ') else {
            skipped += 1;
            continue;
        };
        let Some((simplified, rest)) = rest.split_once(' ') else {
            skipped += 1;
            continue;
        };
        let Some(pinyin_start) = rest.find('[') else {
            skipped += 1;
            continue;
        };
        let Some(pinyin_end) = rest.find(']') else {
            skipped += 1;
            continue;
        };

        let pinyin = &rest[pinyin_start + 1..pinyin_end];
        let syllables: Option<Vec<String>> = pinyin.split_whitespace().map(to_tone_marks).collect();

        let Some(syllables) = syllables else {
            skipped += 1;
            continue;
        };

        let mut forms: Vec<&str> = vec![traditional];

        if simplified != traditional {
            forms.push(simplified);
        }

        for form in forms {
            if form.chars().count() != syllables.len() {
                skipped += 1;
                continue;
            }

            let words = syllables
                .iter()
                .enumerate()
                .map(|(index, syllable)| format!("{index}:{syllable}"))
                .collect::<Vec<_>>()
                .join(";");

            let readings = entries.entry(form.to_string()).or_default();

            // Merge duplicate readings of the same form.
            if readings.iter().any(|reading| reading.words == words) {
                continue;
            }

            readings.push(DictionaryReading {
                words,
                order: readings.len() as u16,
                common: true,
                ..DictionaryReading::default()
            });
            converted += 1;
        }
    }

    println!("CC-CEDICT: {converted} readings, {skipped} lines skipped");

    let entries = entries
        .into_iter()
        .map(|(form, readings)| DictionaryEntry {
            form,
            readings,
            place: false,
        })
        .collect();

    Ok(Dictionary {
        sources: vec![(
            "CC-CEDICT".to_string(),
            "cedict_1_0_ts_utf-8_mdbg".to_string(),
        )],
        entries,
        defaults: Vec::new(),
        bare: Vec::new(),
    })
}

/// Convert CC-CEDICT numbered pinyin (`zhong1`, `nu:3`, `r5`) to tone marks.
fn to_tone_marks(syllable: &str) -> Option<String> {
    let mut chars = syllable.chars().peekable();
    let mut body = String::new();
    let mut tone = None;

    for c in chars.by_ref() {
        if let Some(digit) = c.to_digit(10) {
            if !(1..=5).contains(&digit) {
                return None;
            }

            tone = Some(digit);

            if chars.next().is_some() {
                return None;
            }

            break;
        }

        body.push(c);
    }

    let mut body = body.replace("u:", "ü").replace('v', "ü");
    let tone = tone?;

    if tone == 5 {
        return Some(body);
    }

    let vowel = body
        .char_indices()
        .find(|(_, c)| *c == 'a')
        .or_else(|| body.char_indices().find(|(_, c)| *c == 'o'))
        .or_else(|| body.char_indices().find(|(_, c)| *c == 'e'))
        .or_else(|| {
            if body.contains("iu") {
                body.char_indices().find(|(_, c)| *c == 'u')
            } else if body.contains("ui") {
                body.char_indices().find(|(_, c)| *c == 'i')
            } else {
                body.char_indices()
                    .find(|(_, c)| matches!(c, 'i' | 'u' | 'ü'))
            }
        })?;

    let marks = match vowel.1 {
        'a' => ['ā', 'á', 'ǎ', 'à'],
        'o' => ['ō', 'ó', 'ǒ', 'ò'],
        'e' => ['ē', 'é', 'ě', 'è'],
        'i' => ['ī', 'í', 'ǐ', 'ì'],
        'u' => ['ū', 'ú', 'ǔ', 'ù'],
        'ü' => ['ǖ', 'ǘ', 'ǚ', 'ǜ'],
        _ => return None,
    };

    let marked = marks[tone as usize - 1];
    body.replace_range(vowel.0..vowel.0 + vowel.1.len_utf8(), &marked.to_string());

    Some(body)
}

#[cfg(test)]
mod tests {
    use super::to_tone_marks;

    #[test]
    fn pinyin_tone_marks() {
        assert_eq!(to_tone_marks("zhong1").as_deref(), Some("zhōng"));
        assert_eq!(to_tone_marks("guo2").as_deref(), Some("guó"));
        assert_eq!(to_tone_marks("shui3").as_deref(), Some("shuǐ"));
        assert_eq!(to_tone_marks("liu2").as_deref(), Some("liú"));
        assert_eq!(to_tone_marks("nu:3").as_deref(), Some("nǚ"));
        assert_eq!(to_tone_marks("lv4").as_deref(), Some("lǜ"));
        assert_eq!(to_tone_marks("r5").as_deref(), Some("r"));
        assert_eq!(to_tone_marks("xie4").as_deref(), Some("xiè"));
    }
}
