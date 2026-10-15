//! Benchmarks for the GSUB built for word annotations.
//!
//! `build_addition` assembles the lookups, and `merge` packs them with the
//! table writer (the expensive part); keeping both measurable makes the
//! packer's cost visible when it changes.
//!
//! Runs as a plain binary (`cargo bench --bench gsub`) rather than through a
//! benchmark framework, so the dependency tree stays small.

use std::time::{Duration, Instant};

use fontcull_read_fonts::types::GlyphId;
use rubify::{
    gsub,
    plan::{FontPlan, GlyphSpec, LookupRef, RuleSpec},
    words::RubySpan,
};

/// How many distinct helper lookups the generator spreads readings over.
///
/// Rules that reference the same helper lookups serialize identically when
/// their inputs do (the first glyph lives in the coverage, not the rule), and
/// the table writer deduplicates them; spreading readings over a set of slots
/// that does not align with the input cycle keeps a generated plan's rules
/// unique, as a dictionary's are.
const SINGLE_SLOTS: usize = 61;
const LIGATURE_SLOTS: usize = 7;

/// A plan shaped like a real dictionary build: many 2-3 glyph words, some
/// readings spanning several characters (ligatures), the rest per character.
fn synthetic_plan(rules: usize) -> FontPlan {
    // A synthetic cmap: 4096 base "characters" mapped to glyphs 3..4099.
    let base = |index: usize| GlyphId::new(3 + (index % 4096) as u32);

    let mut plan = FontPlan::default();
    let mut singles: Vec<Vec<(GlyphId, usize)>> = vec![Vec::new(); SINGLE_SLOTS];
    let mut ligatures: Vec<Vec<(Vec<GlyphId>, usize)>> = vec![Vec::new(); LIGATURE_SLOTS];
    let mut registered: rustc_hash::FxHashMap<GlyphId, usize> = rustc_hash::FxHashMap::default();

    for index in 0..rules {
        let first = base(index * 7 + 1);
        let second = base(index * 13 + 2);
        let third = base(index * 17 + 3);

        let mut input = vec![first, second];

        if index % 2 == 1 {
            input.push(third);
        }

        // Every fourth word has a grouped reading: one ligature for the run.
        if index % 4 == 0 {
            let glyph = plan.glyphs.len();
            plan.glyphs.push(GlyphSpec {
                bases: input.iter().map(|_| 'x').collect(),
                gids: input.clone(),
                spans: vec![RubySpan {
                    start: 0,
                    end: input.len(),
                    ruby: "よみ".to_string(),
                }],
                followings: vec![String::new()],
            });

            let slot = index % LIGATURE_SLOTS;
            ligatures[slot].push((input.clone(), glyph));

            let mut lookups = vec![None; input.len()];
            lookups[0] = Some(LookupRef::Ligature {
                slot: slot as u16,
                first,
            });

            plan.rules.push(RuleSpec { input, lookups });

            continue;
        }

        // The rest are per-character readings: one single substitution each.
        let slot = index % SINGLE_SLOTS;
        let mut lookups = Vec::with_capacity(input.len());

        for gid in &input {
            let glyph = *registered.entry(*gid).or_insert_with(|| {
                let index = plan.glyphs.len();
                plan.glyphs.push(GlyphSpec {
                    bases: vec!['x'],
                    gids: vec![*gid],
                    spans: vec![RubySpan {
                        start: 0,
                        end: 1,
                        ruby: "x".to_string(),
                    }],
                    followings: vec![String::new()],
                });
                index
            });

            let entry = &mut singles[slot];

            if !entry.iter().any(|(input, g)| *input == *gid && *g == glyph) {
                entry.push((*gid, glyph));
            }

            lookups.push(Some(LookupRef::Single {
                slot: slot as u16,
                input: *gid,
            }));
        }

        plan.rules.push(RuleSpec { input, lookups });
    }

    plan.rules.sort_by(|a, b| {
        a.input[0]
            .to_u32()
            .cmp(&b.input[0].to_u32())
            .then_with(|| b.input.len().cmp(&a.input.len()))
            .then_with(|| a.input[1..].cmp(&b.input[1..]))
    });

    plan.singles = singles;
    plan.ligatures = ligatures;

    plan
}

/// Run `body` a few times and report the fastest run (least noise).
fn time(label: &str, iterations: usize, mut body: impl FnMut()) {
    let mut best: Option<Duration> = None;

    for _ in 0..iterations {
        let start = Instant::now();
        body();
        let elapsed = start.elapsed();
        best = Some(best.map_or(elapsed, |best| best.min(elapsed)));
    }

    println!("{label:32} {:?}", best.unwrap());
}

fn main() {
    for rules in [10_000usize, 100_000] {
        let plan = synthetic_plan(rules);

        time(&format!("build_addition/{rules}"), 5, || {
            std::hint::black_box(gsub::build_addition(&plan, 60_000, 0));
        });

        time(&format!("merge/{rules}"), 3, || {
            let addition = gsub::build_addition(&plan, 60_000, 0);
            std::hint::black_box(gsub::merge(None, addition).unwrap());
        });
    }
}
