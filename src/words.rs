//! Word annotations: notation, Han runs and dictionary word selection.
//!
//! A word is a sequence of characters with a reading attached to one or more
//! spans of it (`日[に]本[ほん]`), which lets a font annotate readings that no
//! single character can express (今日 きょう, 中国 zhōng guó).

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use rustc_hash::FxHashSet;

use crate::dictionary::{Dictionary, DictionaryReading};

/// Priority tags the dictionary compiler records as a bitmask.
pub const TAG_NAMES: &[&str] = &[
    "ichi1", "ichi2", "news1", "news2", "spec1", "spec2", "gai1", "gai2",
];

pub fn tag_bit(name: &str) -> Option<u16> {
    TAG_NAMES
        .iter()
        .position(|candidate| *candidate == name)
        .map(|index| 1u16 << index)
}

pub fn tag_names(mask: u16) -> impl Iterator<Item = &'static str> {
    TAG_NAMES
        .iter()
        .enumerate()
        .filter(move |(index, _)| mask & (1u16 << index) != 0)
        .map(|(_, name)| *name)
}

/// Whether a character is a Han ideograph.
pub fn is_han(c: char) -> bool {
    matches!(
        c as u32,
        0x2E80..=0x2EFF     // radicals supplement
        | 0x3005            // 々
        | 0x3007           // 〇
        | 0x3400..=0x4DBF   // extension A
        | 0x4E00..=0x9FFF   // unified ideographs
        | 0xF900..=0xFAFF   // compatibility ideographs
        | 0x20000..=0x2A6DF // extension B
        | 0x2A700..=0x2EBEF // extensions C..F
        | 0x2F800..=0x2FA1F // compatibility supplement
        | 0x30000..=0x323AF // extensions G..H
    )
}

/// A ruby annotation over characters `[start, end)` of a word.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RubySpan {
    pub start: usize,
    pub end: usize,
    pub ruby: String,
}

/// A maximal sequence of Han characters with its reading, the unit a shaper
/// sees (browsers itemize text by script before shaping).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HanRun {
    pub text: String,
    pub spans: Vec<RubySpan>,
}

/// Parse `0:に;1:ほん` or `0-1:きょう` into spans.
pub fn parse_words(words: &str) -> Vec<RubySpan> {
    words
        .split(';')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (range, ruby) = part.split_once(':')?;
            let (start, end) = match range.split_once('-') {
                Some((start, end)) => {
                    (start.parse::<usize>().ok()?, end.parse::<usize>().ok()? + 1)
                }
                None => {
                    let value = range.parse::<usize>().ok()?;
                    (value, value + 1)
                }
            };

            Some(RubySpan {
                start,
                end,
                ruby: ruby.to_string(),
            })
        })
        .collect()
}

/// Format spans as `0:に;1:ほん` / `0-1:きょう`.
pub fn format_words(spans: &[RubySpan]) -> String {
    spans
        .iter()
        .map(|span| {
            if span.end == span.start + 1 {
                format!("{}:{}", span.start, span.ruby)
            } else {
                format!("{}-{}:{}", span.start, span.end - 1, span.ruby)
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Split a form into its Han runs.
///
/// Runs with an unannotated kanji, or with an annotation leaking outside the
/// run (a reading that crosses kana), are dropped: a font never sees context
/// across scripts.
pub fn han_runs(form: &str, spans: &[RubySpan]) -> Vec<HanRun> {
    let chars: Vec<char> = form.chars().collect();
    let mut runs = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if !is_han(chars[i]) {
            i += 1;
            continue;
        }

        let mut end = i;

        while end < chars.len() && is_han(chars[end]) {
            end += 1;
        }

        let inside: Vec<&RubySpan> = spans
            .iter()
            .filter(|span| span.start < end && span.end > i)
            .collect();
        let covered: usize = inside.iter().map(|span| span.end - span.start).sum();

        let all_inside = inside
            .iter()
            .all(|span| span.start >= i && span.end <= end && !span.ruby.is_empty());

        if covered == end - i && all_inside {
            runs.push(HanRun {
                text: chars[i..end].iter().collect(),
                spans: inside
                    .into_iter()
                    .map(|span| RubySpan {
                        start: span.start - i,
                        end: span.end - i,
                        ruby: span.ruby.clone(),
                    })
                    .collect(),
            });
        }

        i = end;
    }

    runs
}

/// Parse the inline notation: `日[に]本[ほん]`, `今日[きょう]`.
///
/// Each bracketed reading attaches to the run of Han characters right before
/// it; kana is written bare, as in `食[た]べる`.
pub fn parse_notation(notation: &str) -> Result<(String, Vec<RubySpan>)> {
    let mut form = String::new();
    let mut spans: Vec<RubySpan> = Vec::new();
    let mut chars = notation.chars().peekable();
    // Where the unbracketed segment before the next '[' starts (in chars).
    let mut segment_start = 0usize;

    while let Some(c) = chars.next() {
        if c == ']' {
            bail!("unexpected ']' in \"{notation}\"");
        }

        if c != '[' {
            form.push(c);
            continue;
        }

        // Read the reading up to the closing bracket.
        let mut ruby = String::new();
        let mut closed = false;

        for c in chars.by_ref() {
            if c == ']' {
                closed = true;
                break;
            }
            ruby.push(c);
        }

        if !closed {
            bail!("unclosed '[' in \"{notation}\"");
        }

        // The reading attaches to the trailing Han run of the segment before
        // the bracket.
        let base: Vec<char> = form.chars().collect();
        let mut kanji_from = base.len();

        while kanji_from > segment_start && is_han(base[kanji_from - 1]) {
            kanji_from -= 1;
        }

        if kanji_from == base.len() {
            bail!("\"[{ruby}]\" in \"{notation}\" does not follow a kanji");
        }

        spans.push(RubySpan {
            start: kanji_from,
            end: base.len(),
            ruby,
        });

        segment_start = base.len();
    }

    if spans.is_empty() {
        bail!("\"{notation}\" has no reading, write it like 日[に]本[ほん]");
    }

    Ok((form, spans))
}

/// Format a word and spans back to inline notation.
pub fn format_notation(form: &str, spans: &[RubySpan]) -> String {
    let chars: Vec<char> = form.chars().collect();
    let mut out = String::new();
    let mut at = 0;

    for span in spans {
        out.extend(chars[at..span.end].iter());
        out.push('[');
        out.push_str(&span.ruby);
        out.push(']');
        at = span.end;
    }

    out.extend(chars[at..].iter());
    out
}

/// A word selected for annotation, before font coverage is known.
#[derive(Clone, Debug)]
pub struct SelectedWord {
    pub form: String,
    pub spans: Vec<RubySpan>,
}

/// Options controlling which words a dictionary contributes.
#[derive(Clone, Debug)]
pub struct SelectionOptions {
    /// Newspaper frequency bucket window (nf01..nf48); 0 means no bound.
    pub nf_min: u16,
    pub nf_max: u16,
    /// JMdict priority tags that select a word whatever its bucket.
    pub select_tags: Vec<String>,
    /// Tatoeba uses that select a word whatever its bucket.
    pub min_tatoeba: u16,
    /// Skip words the reader is taken to know: levels >= this are dropped.
    pub skip_jlpt: Option<u8>,
    /// Include place names for runs no common word claims.
    pub places: bool,
    /// Include unranked words for runs nothing else claims.
    pub untagged: bool,
    /// Words never to annotate.
    pub removed: FxHashSet<String>,
    /// User words in inline notation; they override dictionary readings.
    pub custom: Vec<String>,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            nf_min: 1,
            nf_max: 48,
            select_tags: ["ichi1", "news1", "spec1", "gai1"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            min_tatoeba: 3,
            skip_jlpt: None,
            places: true,
            untagged: true,
            removed: FxHashSet::default(),
            custom: Vec::new(),
        }
    }
}

/// Rough corpus frequency of an entry.
fn reading_weight(reading: &DictionaryReading) -> f64 {
    if reading.tags == 0 {
        return 0.0;
    }

    let nf = reading.nf;

    if nf > 0 {
        return 1.0 / f64::from(nf);
    }

    1.0 / 48.0
}

/// Pinned first, then tagged, then JMdict order, then corpus weight.
fn rank(a: &DictionaryReading, b: &DictionaryReading) -> std::cmp::Ordering {
    b.pinned
        .cmp(&a.pinned)
        .then_with(|| (b.tags != 0).cmp(&(a.tags != 0)))
        .then_with(|| a.order.cmp(&b.order))
        .then_with(|| {
            reading_weight(b)
                .partial_cmp(&reading_weight(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn reading_selected(reading: &DictionaryReading, options: &SelectionOptions) -> bool {
    let in_bucket = reading.nf >= options.nf_min && reading.nf <= options.nf_max && reading.nf > 0;
    let tagged = options
        .select_tags
        .iter()
        .any(|tag| tag_bit(tag).is_some_and(|bit| reading.tags & bit != 0));
    let tatoeba = options.min_tatoeba > 0 && reading.tatoeba >= options.min_tatoeba;

    reading.common || in_bucket || tagged || tatoeba
}

fn reading_skipped(reading: &DictionaryReading, options: &SelectionOptions) -> bool {
    let known = options.skip_jlpt;

    (reading.nf > 0 && reading.nf < options.nf_min)
        || known.is_some_and(|level| reading.jlpt >= level)
}

/// Apply curated defaults to a pattern map: pin the listed reading where the
/// word is selected.
fn apply_defaults(patterns: &mut BTreeMap<String, SelectedWord>, defaults: &[String]) {
    for notation in defaults {
        let Ok((form, spans)) = parse_notation(notation) else {
            continue;
        };

        if let Some(word) = patterns.get_mut(&form) {
            word.spans = spans;
        }
    }
}

/// Choose the words a dictionary annotates.
pub fn select_words(dict: &Dictionary, options: &SelectionOptions) -> Vec<SelectedWord> {
    let mut patterns: BTreeMap<String, SelectedWord> = BTreeMap::new();
    let mut skipped: FxHashSet<String> = FxHashSet::default();

    // Aggregate readings per form: the compiled data holds one entry per
    // (form, reading) pair, and the best reading must win across them.
    let mut common: BTreeMap<&str, Vec<&DictionaryReading>> = BTreeMap::new();
    let mut places: BTreeMap<&str, Vec<&DictionaryReading>> = BTreeMap::new();

    for entry in &dict.entries {
        if options.removed.contains(&entry.form) {
            continue;
        }

        let readings = if entry.place {
            places.entry(&entry.form).or_default()
        } else {
            common.entry(&entry.form).or_default()
        };

        readings.extend(entry.readings.iter().filter(|r| !r.words.is_empty()));
    }

    for (form, readings) in &common {
        if readings.iter().any(|r| reading_skipped(r, options)) {
            // A word is skipped whole: were only its known reading dropped,
            // 今日 would show こんにち instead of きょう.
            skipped.insert((*form).to_string());
            continue;
        }

        let Some(best) = readings
            .iter()
            .copied()
            .filter(|r| reading_selected(r, options))
            .min_by(|a, b| rank(a, b))
        else {
            continue;
        };

        patterns.insert(
            (*form).to_string(),
            SelectedWord {
                form: (*form).to_string(),
                spans: parse_words(&best.words),
            },
        );
    }

    if options.places {
        for (form, readings) in &places {
            if patterns.contains_key(*form)
                || skipped.contains(*form)
                || options.removed.contains(*form)
            {
                continue;
            }

            let Some(best) = readings.iter().copied().min_by(|a, b| rank(a, b)) else {
                continue;
            };

            patterns.insert(
                (*form).to_string(),
                SelectedWord {
                    form: (*form).to_string(),
                    spans: parse_words(&best.words),
                },
            );
        }
    }

    if options.untagged {
        for (form, readings) in &common {
            if patterns.contains_key(*form)
                || skipped.contains(*form)
                || options.removed.contains(*form)
            {
                continue;
            }

            let Some(best) = readings.iter().copied().min_by_key(|r| r.order) else {
                continue;
            };

            patterns.insert(
                (*form).to_string(),
                SelectedWord {
                    form: (*form).to_string(),
                    spans: parse_words(&best.words),
                },
            );
        }
    }

    apply_defaults(&mut patterns, &dict.defaults);

    for notation in &options.custom {
        let Ok((form, spans)) = parse_notation(notation) else {
            continue;
        };

        patterns.insert(
            form.clone(),
            SelectedWord {
                form: form.clone(),
                spans: spans.clone(),
            },
        );
    }

    patterns
        .into_values()
        .filter(|word| {
            // Only one Han run that is the whole form: a font never sees kana
            // around a run, and single kanji have no reliable reading.
            if word.spans.is_empty() || word.form.chars().count() < 2 {
                return false;
            }

            let runs = han_runs(&word.form, &word.spans);

            runs.len() == 1 && runs[0].text == word.form
        })
        .collect()
}

/// Parse a user word list: one entry per line, `#` comments, either inline
/// notation (`日[に]本[ほん]`) or `word<TAB>reading`.
pub fn parse_word_list(content: &str) -> Result<Vec<String>> {
    let mut entries = Vec::new();

    for (index, line) in content.lines().enumerate() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.contains('\t') {
            let (form, reading) = line
                .split_once('\t')
                .ok_or_else(|| anyhow!("invalid word list line {}", index + 1))?;
            let form = form.trim();
            let reading = reading.trim();

            if form.is_empty() || reading.is_empty() {
                bail!("empty form or reading on line {}", index + 1);
            }

            entries.push(format!("{form}[{reading}]"));
        } else {
            // Validate the notation now, so errors point at the line.
            parse_notation(line).map_err(|error| anyhow!("line {}: {error}", index + 1))?;
            entries.push(line.to_string());
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::DictionaryEntry;

    #[test]
    fn words_round_trip() {
        let spans = parse_words("0:に;1:ほん");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].ruby, "に");
        assert_eq!(spans[1].start, 1);
        assert_eq!(format_words(&spans), "0:に;1:ほん");

        let grouped = parse_words("0-1:きょう");
        assert_eq!(grouped[0].start, 0);
        assert_eq!(grouped[0].end, 2);
        assert_eq!(format_words(&grouped), "0-1:きょう");
    }

    #[test]
    fn notation_parsing() {
        let (form, spans) = parse_notation("日[に]本[ほん]").unwrap();
        assert_eq!(form, "日本");
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0],
            RubySpan {
                start: 0,
                end: 1,
                ruby: "に".into()
            }
        );
        assert_eq!(
            spans[1],
            RubySpan {
                start: 1,
                end: 2,
                ruby: "ほん".into()
            }
        );

        let (form, spans) = parse_notation("今日[きょう]").unwrap();
        assert_eq!(form, "今日");
        assert_eq!(
            spans[0],
            RubySpan {
                start: 0,
                end: 2,
                ruby: "きょう".into()
            }
        );

        let (form, spans) = parse_notation("食[た]べる").unwrap();
        assert_eq!(form, "食べる");
        assert_eq!(
            spans[0],
            RubySpan {
                start: 0,
                end: 1,
                ruby: "た".into()
            }
        );

        assert!(parse_notation("きょう[kyou]").is_err());
    }

    #[test]
    fn han_runs_drop_crossing_spans() {
        let runs = han_runs("日本", &parse_words("0:に;1:ほん"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "日本");
        assert_eq!(runs[0].spans.len(), 2);

        // 行く's reading belongs to the kana after it: still inside the run,
        // but length 1 and not a word.
        let runs = han_runs("行く", &parse_words("0:い"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "行");
    }

    #[test]
    fn word_list_lines() {
        let entries = parse_word_list("# comment\n今日[きょう]\n中国\tzhōng guó\n\n").unwrap();
        assert_eq!(entries, vec!["今日[きょう]", "中国[zhōng guó]"]);
    }

    fn reading(words: &str, nf: u16, tags: u16, order: u16, jlpt: u8) -> DictionaryReading {
        DictionaryReading {
            words: words.to_string(),
            nf,
            tags,
            order,
            jlpt,
            ..DictionaryReading::default()
        }
    }

    fn entry(form: &str, readings: Vec<DictionaryReading>, place: bool) -> DictionaryEntry {
        DictionaryEntry {
            form: form.to_string(),
            readings,
            place,
        }
    }

    fn selected<'a>(words: &'a [SelectedWord], form: &str) -> Option<&'a SelectedWord> {
        words.iter().find(|word| word.form == form)
    }

    #[test]
    fn selection_ranks_readings_of_a_form() {
        // The compiled data holds one entry per (form, reading); the best
        // reading must win across them.
        let dict = Dictionary {
            entries: vec![
                entry(
                    "今日",
                    vec![reading("0-1:きょう", 2, tag_bit("ichi1").unwrap(), 0, 0)],
                    false,
                ),
                entry(
                    "今日",
                    vec![reading("0:こん;1:にち", 2, tag_bit("news1").unwrap(), 1, 0)],
                    false,
                ),
            ],
            ..Dictionary::default()
        };

        let words = select_words(&dict, &SelectionOptions::default());
        let today = selected(&words, "今日").unwrap();
        assert_eq!(format_words(&today.spans), "0-1:きょう");
    }

    #[test]
    fn a_known_word_is_skipped_whole() {
        let dict = Dictionary {
            entries: vec![entry("箱", vec![reading("0:はこ", 0, 0, 0, 5)], false)],
            ..Dictionary::default()
        };

        let options = SelectionOptions {
            skip_jlpt: Some(4),
            ..SelectionOptions::default()
        };

        let words = select_words(&dict, &options);
        assert!(selected(&words, "箱").is_none());
    }

    #[test]
    fn places_fill_only_unclaimed_runs() {
        let dict = Dictionary {
            entries: vec![
                entry("東京", vec![reading("0:とう;1:きょう", 10, 0, 0, 0)], false),
                entry("東京", vec![reading("0-1:とうきょう", 0, 0, 0, 0)], true),
                entry("新宿", vec![reading("0:しん;1:じゅく", 0, 0, 0, 0)], true),
            ],
            ..Dictionary::default()
        };

        let words = select_words(&dict, &SelectionOptions::default());

        let tokyo = selected(&words, "東京").unwrap();
        assert_eq!(format_words(&tokyo.spans), "0:とう;1:きょう");

        let shinjuku = selected(&words, "新宿").unwrap();
        assert_eq!(format_words(&shinjuku.spans), "0:しん;1:じゅく");
    }

    #[test]
    fn unranked_words_are_configurable() {
        let dict = Dictionary {
            entries: vec![entry(
                "海月",
                vec![reading("0-1:くらげ", 0, 0, 0, 0)],
                false,
            )],
            ..Dictionary::default()
        };

        let words = select_words(&dict, &SelectionOptions::default());
        assert!(selected(&words, "海月").is_some());

        let options = SelectionOptions {
            untagged: false,
            places: false,
            ..SelectionOptions::default()
        };
        let words = select_words(&dict, &options);
        assert!(selected(&words, "海月").is_none());
    }

    #[test]
    fn defaults_and_custom_words_win() {
        let dict = Dictionary {
            entries: vec![entry(
                "出来",
                vec![reading(
                    "0:しゅっ;1:たい",
                    9,
                    tag_bit("news1").unwrap(),
                    0,
                    0,
                )],
                false,
            )],
            defaults: vec!["出[で]来[き]".to_string()],
            ..Dictionary::default()
        };

        let words = select_words(&dict, &SelectionOptions::default());
        assert_eq!(
            format_words(&selected(&words, "出来").unwrap().spans),
            "0:で;1:き"
        );

        let options = SelectionOptions {
            custom: vec!["中[zhōng]国[guó]".to_string()],
            ..SelectionOptions::default()
        };
        let words = select_words(&dict, &options);
        assert_eq!(
            format_words(&selected(&words, "中国").unwrap().spans),
            "0:zhōng;1:guó"
        );
    }
}
