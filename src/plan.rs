//! Per-font word planning: shared glyphs, substitution slots and rules.
//!
//! One word reading becomes either a set of single substitutions (one per
//! character, when every reading covers exactly one character) or one ligature
//! substitution (when a reading spans several characters). Glyphs are shared
//! by what they draw, so the rules of thousands of words cost remark
//! remarkably few outlines.

use fontcull_read_fonts::types::GlyphId;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rustc_hash::FxHashMap;

use crate::words::{RubySpan, SelectedWord};

/// A new glyph to append: base characters with their ruby spans.
#[derive(Clone, Debug)]
pub struct GlyphSpec {
    pub bases: Vec<char>,
    pub gids: Vec<GlyphId>,
    pub spans: Vec<RubySpan>,
    /// Readings after each span in the word, for context-dependent conversion
    /// (a trailing っ doubles the next span's consonant in romaji).
    pub followings: Vec<String>,
}

/// One word rule: glyphs to match and, per position, the lookup to apply.
#[derive(Clone, Debug)]
pub struct RuleSpec {
    pub input: Vec<GlyphId>,
    pub lookups: Vec<Option<LookupRef>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupRef {
    Single { slot: u16, input: GlyphId },
    Ligature { slot: u16, first: GlyphId },
}

#[derive(Clone, Debug, Default)]
pub struct FontPlan {
    /// New glyphs, in deterministic order.
    pub glyphs: Vec<GlyphSpec>,
    /// Word rules, sorted longest first per first glyph.
    pub rules: Vec<RuleSpec>,
    /// Single-substitution slots: `slot -> (input glyph, glyph index)`.
    pub singles: Vec<Vec<(GlyphId, usize)>>,
    /// Ligature slots: `slot -> (component glyphs, glyph index)`.
    pub ligatures: Vec<Vec<(Vec<GlyphId>, usize)>>,
    /// Words skipped because the font cannot draw them.
    pub skipped: usize,
}

#[derive(Default)]
struct Builder {
    glyph_keys: FxHashMap<String, usize>,
    glyphs: Vec<GlyphSpec>,
    /// input glyph -> glyph indexes, in first-seen order (the slot numbering).
    single_outputs: FxHashMap<GlyphId, Vec<usize>>,
    singles: Vec<Vec<(GlyphId, usize)>>,
    /// component sequence -> glyph indexes, in first-seen order.
    ligature_outputs: FxHashMap<Vec<GlyphId>, Vec<usize>>,
    ligatures: Vec<Vec<(Vec<GlyphId>, usize)>>,
    rules: FxHashMap<Vec<GlyphId>, RuleSpec>,
}

fn glyph_key(bases: &[char], rendered: &[String]) -> String {
    let bases: String = bases.iter().collect();
    let rendered = rendered.join("\u{1}");
    format!("{bases}|{rendered}")
}

impl Builder {
    fn register_glyph(
        &mut self,
        bases: Vec<char>,
        gids: Vec<GlyphId>,
        spans: Vec<RubySpan>,
        followings: Vec<String>,
        rendered: &[String],
    ) -> usize {
        let key = glyph_key(&bases, rendered);

        if let Some(index) = self.glyph_keys.get(&key) {
            return *index;
        }

        let index = self.glyphs.len();
        self.glyph_keys.insert(key, index);
        self.glyphs.push(GlyphSpec {
            bases,
            gids,
            spans,
            followings,
        });

        index
    }

    fn single_lookup(&mut self, input: GlyphId, glyph: usize) -> LookupRef {
        let outputs = self.single_outputs.entry(input).or_default();

        let slot = match outputs.iter().position(|candidate| *candidate == glyph) {
            Some(position) => position,
            None => {
                outputs.push(glyph);
                outputs.len() - 1
            }
        };

        while self.singles.len() <= slot {
            self.singles.push(Vec::new());
        }

        let entry = &mut self.singles[slot];

        if !entry.iter().any(|(i, g)| *i == input && *g == glyph) {
            entry.push((input, glyph));
        }

        LookupRef::Single {
            slot: slot as u16,
            input,
        }
    }

    fn ligature_lookup(&mut self, components: &[GlyphId], glyph: usize) -> LookupRef {
        let outputs = self
            .ligature_outputs
            .entry(components.to_vec())
            .or_default();

        let slot = match outputs.iter().position(|candidate| *candidate == glyph) {
            Some(position) => position,
            None => {
                outputs.push(glyph);
                outputs.len() - 1
            }
        };

        while self.ligatures.len() <= slot {
            self.ligatures.push(Vec::new());
        }

        let entry = &mut self.ligatures[slot];

        if !entry
            .iter()
            .any(|(c, g)| c.as_slice() == components && *g == glyph)
        {
            entry.push((components.to_vec(), glyph));
        }

        LookupRef::Ligature {
            slot: slot as u16,
            first: components[0],
        }
    }

    fn push_rule(&mut self, rule: RuleSpec) {
        self.rules.insert(rule.input.clone(), rule);
    }
}

/// A word whose inputs and rendered texts are ready, before it touches the
/// shared glyph registry.
struct PreparedWord {
    bases: Vec<char>,
    input: Vec<GlyphId>,
    spans: Vec<RubySpan>,
    followings: Vec<String>,
    rendered: Vec<String>,
    grouped: bool,
}

/// Plan the words a font can draw.
///
/// `char_to_gid` maps every base character of the font, and `render` returns
/// the annotation text a reading renders to (with its following readings for
/// context-dependent conversion); glyphs are shared by the text they draw.
pub fn build_plan(
    words: &[SelectedWord],
    char_to_gid: &FxHashMap<char, GlyphId>,
    render: impl Fn(&str, &str) -> Option<String> + Sync,
) -> FontPlan {
    // Later entries win: this lets custom words override dictionary ones.
    let mut by_sequence: FxHashMap<Vec<char>, &SelectedWord> = FxHashMap::default();

    for word in words {
        by_sequence.insert(word.form.chars().collect(), word);
    }

    let mut sequences: Vec<Vec<char>> = by_sequence.keys().cloned().collect();
    sequences.sort();

    // Everything that does not need the registry is independent per word and
    // runs in parallel; measuring the ruby glyphs is the expensive part.
    let prepared: Vec<Option<PreparedWord>> = sequences
        .par_iter()
        .map(|sequence| {
            let word = by_sequence[sequence];

            let input: Option<Vec<GlyphId>> = sequence
                .iter()
                .map(|c| char_to_gid.get(c).copied())
                .collect();

            let input = input?;

            if word.spans.is_empty() {
                return None;
            }

            // Every character must be covered and every span must be inside
            // the word: otherwise the ligature would drop an annotation.
            let mut covered = vec![false; sequence.len()];

            for span in &word.spans {
                if span.start >= span.end || span.end > sequence.len() || span.ruby.is_empty() {
                    return None;
                }

                covered[span.start..span.end].fill(true);
            }

            if covered.iter().any(|c| !*c) {
                return None;
            }

            let followings: Vec<String> = word
                .spans
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    word.spans[index + 1..]
                        .iter()
                        .map(|span| span.ruby.as_str())
                        .collect()
                })
                .collect();

            // The rendered text decides what the glyph draws; a word with any
            // reading the ruby font cannot draw is skipped whole.
            let rendered: Option<Vec<String>> = word
                .spans
                .iter()
                .enumerate()
                .map(|(index, span)| render(&span.ruby, &followings[index]))
                .collect();

            let rendered = rendered?;

            Some(PreparedWord {
                bases: sequence.clone(),
                input,
                spans: word.spans.clone(),
                followings,
                rendered,
                grouped: word.spans.iter().any(|span| span.end > span.start + 1),
            })
        })
        .collect();

    let skipped = prepared.iter().filter(|word| word.is_none()).count();
    let mut builder = Builder::default();

    for prepared in prepared.into_iter().flatten() {
        let PreparedWord {
            bases,
            input,
            spans,
            followings,
            rendered,
            grouped,
        } = prepared;

        if grouped {
            let glyph = builder.register_glyph(bases, input.clone(), spans, followings, &rendered);
            let lookup = builder.ligature_lookup(&input, glyph);
            let mut lookups = vec![Some(lookup); input.len()];

            for (index, lookup) in lookups.iter_mut().enumerate() {
                if index > 0 {
                    *lookup = None;
                }
            }

            builder.push_rule(RuleSpec { input, lookups });
        } else {
            let mut lookups = Vec::with_capacity(input.len());

            for (index, (ch, gid)) in bases.iter().zip(&input).enumerate() {
                let span = spans
                    .iter()
                    .find(|span| span.start == index)
                    .expect("single spans cover every character");

                let glyph = builder.register_glyph(
                    vec![*ch],
                    vec![*gid],
                    vec![RubySpan {
                        start: 0,
                        end: 1,
                        ruby: span.ruby.clone(),
                    }],
                    vec![followings[index].clone()],
                    &rendered[index..index + 1],
                );

                lookups.push(Some(builder.single_lookup(*gid, glyph)));
            }

            builder.push_rule(RuleSpec { input, lookups });
        }
    }

    let mut rules: Vec<RuleSpec> = builder.rules.into_values().collect();
    rules.sort_by(|a, b| {
        a.input[0]
            .to_u32()
            .cmp(&b.input[0].to_u32())
            .then_with(|| b.input.len().cmp(&a.input.len()))
            .then_with(|| a.input[1..].cmp(&b.input[1..]))
    });

    FontPlan {
        glyphs: builder.glyphs,
        rules,
        singles: builder.singles,
        ligatures: builder.ligatures,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::words::parse_words;

    fn map(pairs: &[(char, u32)]) -> FxHashMap<char, GlyphId> {
        pairs
            .iter()
            .map(|(ch, gid)| (*ch, GlyphId::new(*gid)))
            .collect()
    }

    fn word(form: &str, words: &str) -> SelectedWord {
        SelectedWord {
            form: form.to_string(),
            spans: parse_words(words),
        }
    }

    fn render(reading: &str, _following: &str) -> Option<String> {
        Some(reading.to_string())
    }

    #[test]
    fn glyphs_are_shared_by_rendered_text() {
        let words = vec![
            word("中国", "0:ちゅう;1:ごく"),
            word("中学", "0:ちゅう;1:がく"),
        ];
        let gids = map(&[('中', 10), ('国', 11), ('学', 12)]);

        let plan = build_plan(&words, &gids, render);

        // 中(ちゅう) is shared between the two words.
        assert_eq!(plan.glyphs.len(), 3);
        assert_eq!(plan.rules.len(), 2);
        assert_eq!(plan.singles.len(), 1, "one slot for every first output");
        assert_eq!(plan.ligatures.len(), 0);
    }

    #[test]
    fn grouped_readings_become_ligatures() {
        let words = vec![word("日本", "0-1:にほん")];
        let gids = map(&[('日', 1), ('本', 2)]);

        let plan = build_plan(&words, &gids, render);

        assert_eq!(plan.glyphs.len(), 1, "one ligature glyph");
        assert_eq!(plan.ligatures.len(), 1);
        assert_eq!(plan.singles.len(), 0);

        let rule = &plan.rules[0];
        assert_eq!(rule.input, vec![GlyphId::new(1), GlyphId::new(2)]);
        assert!(matches!(
            rule.lookups[0],
            Some(LookupRef::Ligature { slot: 0, .. })
        ));
        assert!(rule.lookups[1].is_none());
    }

    #[test]
    fn rules_sort_longest_first_per_first_glyph() {
        let words = vec![
            word("日本", "0-1:にほん"),
            word("日本語", "0:に;1-2:ほんご"),
        ];
        let gids = map(&[('日', 1), ('本', 2), ('語', 3)]);

        let plan = build_plan(&words, &gids, render);

        assert_eq!(plan.rules.len(), 2);
        assert_eq!(plan.rules[0].input.len(), 3, "日本語 comes first");
        assert_eq!(plan.rules[1].input.len(), 2);
    }

    #[test]
    fn undrawable_words_are_skipped() {
        let words = vec![word("日本", "0-1:にほん")];
        let gids = map(&[('日', 1)]); // 本 is missing

        let plan = build_plan(&words, &gids, render);

        assert!(plan.glyphs.is_empty());
        assert!(plan.rules.is_empty());
        assert_eq!(plan.skipped, 1);
    }

    #[test]
    fn followings_reach_single_character_glyphs() {
        let words = vec![word("学校", "0:がっ;1:こう")];
        let gids = map(&[('学', 1), ('校', 2)]);

        let plan = build_plan(&words, &gids, render);

        assert_eq!(plan.glyphs.len(), 2);
        assert_eq!(plan.glyphs[0].followings, vec!["こう".to_string()]);
        assert!(plan.glyphs[1].followings[0].is_empty());
    }
}
