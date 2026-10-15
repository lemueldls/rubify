//! Building and merging the GSUB rules that pick word annotations.
//!
//! Words become chained contextual substitutions that call single- or
//! ligature-substitution lookups; the rules run under `rclt` and `liga`, both
//! of which shapers enable by default.

use anyhow::{Context, Result};
use fontcull_read_fonts::{
    FontData, FontRead,
    types::{GlyphId, Tag},
};
use fontcull_write_fonts::{
    dump_table,
    tables::{
        gsub::{
            Gsub, Ligature, LigatureSet, LigatureSubstFormat1, SingleSubst,
            SubstitutionChainContext, SubstitutionLookup,
        },
        layout::{
            ChainedSequenceContext, ChainedSequenceContextFormat1, ChainedSequenceRule,
            ChainedSequenceRuleSet, CoverageTable, Feature, FeatureRecord, LangSys, Lookup,
            LookupFlag, Script, ScriptRecord, SequenceLookupRecord,
        },
    },
    types::GlyphId16,
};
use rustc_hash::FxHashMap;

use crate::plan::{FontPlan, LookupRef, RuleSpec};

/// Single substitutions per subtable. Coverage and substitutes cost two bytes
/// per glyph each, so a subtable stays well under its 16-bit offset limit.
const SINGLES_PER_CHUNK: usize = 3000;

/// Ligatures grouped by first glyph: `(first, [(components, glyph index)])`.
type LigatureGroups = Vec<(u16, Vec<(Vec<GlyphId>, usize)>)>;

/// One chain rule, with its input glyphs inline and per-position lookup calls.
fn chain_rule(
    rule: &RuleSpec,
    single_chunks: &FxHashMap<(u16, u16), u16>,
    ligature_chunks: &FxHashMap<(u16, u16), u16>,
    base_lookup_count: u16,
) -> ChainedSequenceRule {
    let input_sequence: Vec<GlyphId16> = rule.input[1..].iter().map(|gid| as_gid16(*gid)).collect();
    let mut records: Vec<SequenceLookupRecord> = Vec::new();

    for (index, lookup) in rule.lookups.iter().enumerate() {
        let Some(lookup) = lookup else {
            continue;
        };

        let local = match lookup {
            LookupRef::Single { slot, input } => single_chunks[&(*slot, input.to_u32() as u16)],
            LookupRef::Ligature { slot, first } => ligature_chunks[&(*slot, first.to_u32() as u16)],
        };

        records.push(SequenceLookupRecord::new(
            index as u16,
            base_lookup_count + local,
        ));
    }

    ChainedSequenceRule::new(Vec::new(), input_sequence, Vec::new(), records)
}

/// The lookups to append, helpers (singles and ligatures) first, then the
/// chain rules that call them.
pub struct GsubAddition {
    lookups: Vec<SubstitutionLookup>,
    /// Positions of the chain lookups within `lookups`.
    chain_indices: Vec<u16>,
}

fn as_gid16(gid: GlyphId) -> GlyphId16 {
    GlyphId16::new(gid.to_u32() as u16)
}

/// Build the lookups a plan needs, referencing glyphs appended after
/// `first_new_gid`.
pub fn build_addition(plan: &FontPlan, first_new_gid: u16, base_lookup_count: u16) -> GsubAddition {
    let final_gid = |glyph_index: usize| first_new_gid + glyph_index as u16;

    let mut lookups: Vec<SubstitutionLookup> = Vec::new();
    let mut single_chunks: FxHashMap<(u16, u16), u16> = FxHashMap::default();
    let mut ligature_chunks: FxHashMap<(u16, u16), u16> = FxHashMap::default();

    // Single substitutions, one lookup per chunk of a slot.
    for (slot, entries) in plan.singles.iter().enumerate() {
        for chunk in entries.chunks(SINGLES_PER_CHUNK) {
            let mut pairs: Vec<(GlyphId, GlyphId16)> = chunk
                .iter()
                .map(|(input, glyph)| (*input, as_gid16(GlyphId::new(final_gid(*glyph) as u32))))
                .collect();
            pairs.sort_by_key(|(input, _)| input.to_u32());

            let coverage =
                CoverageTable::format_1(pairs.iter().map(|(input, _)| as_gid16(*input)).collect());
            let substitutes: Vec<GlyphId16> = pairs.iter().map(|(_, output)| *output).collect();
            let lookup = Lookup::new(
                LookupFlag::default(),
                vec![SingleSubst::format_2(coverage, substitutes)],
            );
            let lookup_index = lookups.len() as u16;

            for (input, _) in &pairs {
                single_chunks.insert((slot as u16, input.to_u32() as u16), lookup_index);
            }

            lookups.push(SubstitutionLookup::Single(lookup));
        }
    }

    // Ligature substitutions. A first glyph's set can be huge (大 starts
    // thousands of place names), so its ligatures may span several subtables
    // inside one lookup; a shaper tries subtables in order. Slots stay in
    // separate lookups, so a rule always reaches the right reading.
    for (slot, entries) in plan.ligatures.iter().enumerate() {
        let mut by_first: LigatureGroups = Vec::new();

        for (components, glyph) in entries {
            let first = components[0].to_u32() as u16;

            match by_first.last_mut() {
                Some((candidate, group)) if *candidate == first => {
                    group.push((components.clone(), *glyph))
                }
                _ => by_first.push((first, vec![(components.clone(), *glyph)])),
            }
        }

        // Chunks of (first, ligatures); a first glyph repeats across chunks.
        let mut chunks: LigatureGroups = Vec::new();
        let mut chunk_bytes = 0usize;
        let mut chunk_coverages: Vec<u16> = Vec::new();
        let mut subtables: Vec<LigatureSubstFormat1> = Vec::new();

        let flush_chunk = |chunks: &mut LigatureGroups,
                           subtables: &mut Vec<LigatureSubstFormat1>| {
            if chunks.is_empty() {
                return;
            }

            // Coverage must be sorted, and the sets stay parallel to it.
            chunks.sort_by_key(|(first, _)| *first);

            let mut firsts: Vec<u16> = Vec::with_capacity(chunks.len());
            let mut sets: Vec<LigatureSet> = Vec::with_capacity(chunks.len());

            for (first, group) in chunks.iter() {
                firsts.push(*first);

                let mut group = group.clone();
                // Longest first: a shaper takes the first ligature that matches.
                group.sort_by(|a, b| {
                    b.0.len()
                        .cmp(&a.0.len())
                        .then_with(|| a.0.cmp(&b.0))
                        .then_with(|| a.1.cmp(&b.1))
                });

                let ligatures: Vec<Ligature> = group
                    .iter()
                    .map(|(components, glyph)| {
                        Ligature::new(
                            as_gid16(GlyphId::new(final_gid(*glyph) as u32)),
                            components[1..].iter().map(|g| as_gid16(*g)).collect(),
                        )
                    })
                    .collect();

                sets.push(LigatureSet::new(ligatures));
            }

            let coverage =
                CoverageTable::format_1(firsts.iter().map(|f| GlyphId16::new(*f)).collect());
            subtables.push(LigatureSubstFormat1::new(coverage, sets));
            chunks.clear();
        };

        for (first, mut group) in by_first {
            // Split one first glyph's ligatures across chunks when they do not
            // fit a subtable.
            while !group.is_empty() {
                if chunk_bytes + group.len() * 12 > 24_000 || chunk_coverages.len() >= 512 {
                    flush_chunk(&mut chunks, &mut subtables);
                    chunk_bytes = 0;
                }

                let take = group
                    .len()
                    .min((24_000usize.saturating_sub(chunk_bytes) / 12).max(1))
                    .min(512usize.saturating_sub(chunk_coverages.len()).max(1));

                let rest = group.split_off(take);
                chunk_bytes += group.len() * 12;
                chunk_coverages.push(first);
                chunks.push((first, group));

                // A first glyph may repeat only across chunks, never within one.
                if !rest.is_empty() {
                    flush_chunk(&mut chunks, &mut subtables);
                    chunk_bytes = 0;
                }

                group = rest;
            }
        }

        flush_chunk(&mut chunks, &mut subtables);

        if !subtables.is_empty() {
            let lookup_index = lookups.len() as u16;

            let mut firsts: Vec<u16> = plan.ligatures[slot]
                .iter()
                .map(|(components, _)| components[0].to_u32() as u16)
                .collect();
            firsts.sort_unstable();
            firsts.dedup();

            for first in firsts {
                ligature_chunks.insert((slot as u16, first), lookup_index);
            }

            lookups.push(SubstitutionLookup::Ligature(Lookup::new(
                LookupFlag::default(),
                subtables,
            )));
        }
    }

    // Chain rules, longest first per first glyph (the plan is sorted).
    //
    // Rules are grouped by first glyph into format 1 chained contexts: one
    // coverage entry per first glyph, one rule set per entry. A single lookup
    // holds them all; the writer promotes it to an extension lookup, whose
    // 32-bit offsets reach the subtables wherever they are packed.
    let mut subtables: Vec<SubstitutionChainContext> = Vec::new();
    let mut groups: Vec<(u16, Vec<&RuleSpec>)> = Vec::new();
    let mut chunk_bytes = 0usize;

    let build_subtable = |groups: &[(u16, Vec<&RuleSpec>)]| -> SubstitutionChainContext {
        let mut coverages: Vec<GlyphId16> = Vec::with_capacity(groups.len());
        let mut rule_sets: Vec<Option<ChainedSequenceRuleSet>> = Vec::with_capacity(groups.len());

        for (first, rules) in groups {
            coverages.push(GlyphId16::new(*first));

            let rules: Vec<ChainedSequenceRule> = rules
                .iter()
                .map(|rule| chain_rule(rule, &single_chunks, &ligature_chunks, base_lookup_count))
                .collect();

            rule_sets.push(Some(ChainedSequenceRuleSet::new(rules)));
        }

        SubstitutionChainContext::from(ChainedSequenceContext::Format1(
            ChainedSequenceContextFormat1::new(CoverageTable::format_1(coverages), rule_sets),
        ))
    };

    let flush_subtable = |subtables: &mut Vec<SubstitutionChainContext>,
                          groups: &mut Vec<(u16, Vec<&RuleSpec>)>,
                          chunk_bytes: &mut usize| {
        if !groups.is_empty() {
            subtables.push(build_subtable(groups));
            groups.clear();
            *chunk_bytes = 0;
        }
    };

    for rule in &plan.rules {
        let first = rule.input[0].to_u32() as u16;
        // Rough serialized size of one rule: counts, input glyphs and records.
        let bytes = 8 + rule.input.len() * 2 + rule.lookups.len() * 4;

        match groups.last_mut() {
            // The same first glyph may repeat only across subtables.
            Some((current, rules)) if *current == first && chunk_bytes + bytes <= 24_000 => {
                rules.push(rule);
                chunk_bytes += bytes;
            }
            _ => {
                if groups.iter().any(|(current, _)| *current == first)
                    || chunk_bytes + bytes > 24_000
                {
                    flush_subtable(&mut subtables, &mut groups, &mut chunk_bytes);
                }

                groups.push((first, vec![rule]));
                chunk_bytes += bytes;
            }
        }
    }

    flush_subtable(&mut subtables, &mut groups, &mut chunk_bytes);

    let mut chain_indices = Vec::new();

    if !subtables.is_empty() {
        let lookup = Lookup::new(LookupFlag::default(), subtables);
        chain_indices.push(lookups.len() as u16);
        lookups.push(SubstitutionLookup::ChainContextual(lookup));
    }

    GsubAddition {
        lookups,
        chain_indices,
    }
}

/// Append the addition to a font's GSUB (or build one), registering the chain
/// lookups under `liga` and `rclt`.
pub fn merge(existing: Option<&[u8]>, addition: GsubAddition) -> Result<Vec<u8>> {
    let mut gsub = match existing {
        Some(data) => {
            Gsub::read(FontData::new(data)).context("Failed to parse existing GSUB table")?
        }
        None => Gsub::default(),
    };

    let base_lookup_count = gsub.lookup_list.lookups.len() as u16;

    for lookup in addition.lookups {
        gsub.lookup_list.lookups.push(lookup.into());
    }

    let chain_indices: Vec<u16> = addition
        .chain_indices
        .iter()
        .map(|index| base_lookup_count + index)
        .collect();

    for tag in [b"liga", b"rclt"] {
        let feature_index = ensure_feature(&mut gsub, tag, &chain_indices);
        register_feature(&mut gsub, feature_index);
    }

    dump_table(&gsub).context("Failed to write GSUB table")
}

/// Find the feature with `tag`, extending it, or insert a new record keeping
/// the list sorted and shifting the indices that follow.
fn ensure_feature(gsub: &mut Gsub, tag: &[u8; 4], lookup_indices: &[u16]) -> u16 {
    let tag = Tag::new(tag);

    for (index, record) in gsub.feature_list.feature_records.iter_mut().enumerate() {
        if record.feature_tag == tag {
            record
                .feature
                .lookup_list_indices
                .extend_from_slice(lookup_indices);

            return index as u16;
        }
    }

    let position = gsub
        .feature_list
        .feature_records
        .iter()
        .position(|record| record.feature_tag > tag)
        .unwrap_or(gsub.feature_list.feature_records.len());

    shift_feature_indices(gsub, position as u16);

    gsub.feature_list.feature_records.insert(
        position,
        FeatureRecord::new(tag, Feature::new(None, lookup_indices.to_vec())),
    );

    position as u16
}

/// Shift LangSys feature references at or after `position` by one.
fn shift_feature_indices(gsub: &mut Gsub, position: u16) {
    fn shift_langsys(langsys: &mut LangSys, position: u16) {
        for index in langsys.feature_indices.iter_mut() {
            if *index >= position {
                *index += 1;
            }
        }

        if langsys.required_feature_index != 0xFFFF && langsys.required_feature_index >= position {
            langsys.required_feature_index += 1;
        }
    }

    for record in gsub.script_list.script_records.iter_mut() {
        if let Some(langsys) = record.script.default_lang_sys.as_mut() {
            shift_langsys(langsys, position);
        }

        for langsys_record in record.script.lang_sys_records.iter_mut() {
            shift_langsys(&mut langsys_record.lang_sys, position);
        }
    }
}

/// Add a feature index to every language system, so any script sees the rules.
fn register_feature(gsub: &mut Gsub, feature_index: u16) {
    let scripts = &mut gsub.script_list.script_records;

    if scripts.is_empty() {
        scripts.push(ScriptRecord::new(
            Tag::new(b"DFLT"),
            Script::new(Some(LangSys::new(vec![feature_index])), Vec::new()),
        ));

        return;
    }

    let mut has_default = false;

    for record in scripts.iter_mut() {
        if record.script_tag == Tag::new(b"DFLT") {
            has_default = true;
        }

        match record.script.default_lang_sys.as_mut() {
            Some(langsys) => {
                if !langsys.feature_indices.contains(&feature_index) {
                    langsys.feature_indices.push(feature_index);
                }
            }
            None => {
                record
                    .script
                    .default_lang_sys
                    .set(LangSys::new(vec![feature_index]));
            }
        }

        for langsys_record in record.script.lang_sys_records.iter_mut() {
            if !langsys_record
                .lang_sys
                .feature_indices
                .contains(&feature_index)
            {
                langsys_record.lang_sys.feature_indices.push(feature_index);
            }
        }
    }

    if !has_default {
        // Keep the script list sorted: DFLT sorts before most tags.
        let position = scripts
            .iter()
            .position(|record| record.script_tag > Tag::new(b"DFLT"))
            .unwrap_or(scripts.len());

        scripts.insert(
            position,
            ScriptRecord::new(
                Tag::new(b"DFLT"),
                Script::new(Some(LangSys::new(vec![feature_index])), Vec::new()),
            ),
        );
    }
}

/// Keep the empty GSUB out of the API; `merge` builds one when needed.
impl Default for GsubAddition {
    fn default() -> Self {
        Self {
            lookups: Vec::new(),
            chain_indices: Vec::new(),
        }
    }
}

/// Whether an addition has anything to write.
impl GsubAddition {
    pub fn is_empty(&self) -> bool {
        self.lookups.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{FontPlan, GlyphSpec, LookupRef, RuleSpec};
    use crate::words::parse_furigana;

    #[test]
    fn merge_registers_features_and_scripts() {
        let plan = FontPlan {
            glyphs: vec![GlyphSpec {
                bases: vec!['日', '本'],
                gids: vec![GlyphId::new(1), GlyphId::new(2)],
                spans: parse_furigana("0-1:にほん"),
                followings: vec![String::new()],
            }],
            rules: vec![RuleSpec {
                input: vec![GlyphId::new(1), GlyphId::new(2)],
                lookups: vec![
                    Some(LookupRef::Ligature {
                        slot: 0,
                        first: GlyphId::new(1),
                    }),
                    None,
                ],
            }],
            ligatures: vec![vec![(vec![GlyphId::new(1), GlyphId::new(2)], 0)]],
            singles: Vec::new(),
            skipped: 0,
        };

        let addition = build_addition(&plan, 100, 0);
        assert!(!addition.is_empty());

        let bytes = merge(None, addition).unwrap();

        let gsub = fontcull_read_fonts::tables::gsub::Gsub::read(FontData::new(&bytes)).unwrap();

        // Both feature tags exist and point at the chain lookup.
        let features = gsub.feature_list().unwrap();
        let mut tags = Vec::new();

        for record in features.feature_records() {
            let tag = record.feature_tag();
            let feature = record.feature(features.offset_data()).unwrap();
            let indices: Vec<u16> = feature
                .lookup_list_indices()
                .iter()
                .map(|index| index.get())
                .collect();

            tags.push(tag);
            assert_eq!(indices, vec![1], "the chain lookup is the only one");
        }

        assert!(tags.contains(&Tag::new(b"liga")));
        assert!(tags.contains(&Tag::new(b"rclt")));

        // A DFLT script exposes both features.
        let scripts = gsub.script_list().unwrap();
        assert_eq!(scripts.script_records().len(), 1);

        let record = scripts.script_records()[0];
        assert_eq!(record.script_tag(), Tag::new(b"DFLT"));

        let script = record.script(scripts.offset_data()).unwrap();
        let langsys = script.default_lang_sys().unwrap().unwrap();
        let indices: Vec<u16> = langsys
            .feature_indices()
            .iter()
            .map(|index| index.get())
            .collect();
        assert_eq!(indices, vec![0, 1]);

        // Two lookups: the ligature helper, then the chain rules.
        let lookups = gsub.lookup_list().unwrap();
        assert_eq!(lookups.lookups().len(), 2);
    }
}
