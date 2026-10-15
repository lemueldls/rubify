use std::ops::RangeInclusive;

use anyhow::Result;
use fontcull_read_fonts::FontRef;
use kurbo::BezPath;

use super::{
    BaselineTargets, CJK_EXT_A_RANGE, CJK_RANGE, HIRAGANA_RANGE, KATAKANA_RANGE, RubyPosition,
    RubyRenderer, hiragana::to_hiragana, utils::RubyPainter,
};

/// Single kana to romaji, in modified Hepburn. Yōon, sokuon and long vowels
/// are handled by [`to_hepburn`], which needs the rest of the word.
fn kana_to_romaji(c: char) -> Option<&'static str> {
    Some(match c {
        'あ' => "a",
        'い' => "i",
        'う' => "u",
        'え' => "e",
        'お' => "o",
        'か' => "ka",
        'き' => "ki",
        'く' => "ku",
        'け' => "ke",
        'こ' => "ko",
        'が' => "ga",
        'ぎ' => "gi",
        'ぐ' => "gu",
        'げ' => "ge",
        'ご' => "go",
        'さ' => "sa",
        'し' => "shi",
        'す' => "su",
        'せ' => "se",
        'そ' => "so",
        'ざ' => "za",
        'じ' => "ji",
        'ず' => "zu",
        'ぜ' => "ze",
        'ぞ' => "zo",
        'た' => "ta",
        'ち' => "chi",
        'つ' => "tsu",
        'て' => "te",
        'と' => "to",
        'だ' => "da",
        'ぢ' => "ji",
        'づ' => "zu",
        'で' => "de",
        'ど' => "do",
        'な' => "na",
        'に' => "ni",
        'ぬ' => "nu",
        'ね' => "ne",
        'の' => "no",
        'は' => "ha",
        'ひ' => "hi",
        'ふ' => "fu",
        'へ' => "he",
        'ほ' => "ho",
        'ば' => "ba",
        'び' => "bi",
        'ぶ' => "bu",
        'べ' => "be",
        'ぼ' => "bo",
        'ぱ' => "pa",
        'ぴ' => "pi",
        'ぷ' => "pu",
        'ぺ' => "pe",
        'ぽ' => "po",
        'ま' => "ma",
        'み' => "mi",
        'む' => "mu",
        'め' => "me",
        'も' => "mo",
        'や' => "ya",
        'ゆ' => "yu",
        'よ' => "yo",
        'ら' => "ra",
        'り' => "ri",
        'る' => "ru",
        'れ' => "re",
        'ろ' => "ro",
        'わ' => "wa",
        'ゐ' => "i",
        'ゑ' => "e",
        'を' => "o",
        'ゔ' => "vu",
        'ぁ' => "a",
        'ぃ' => "i",
        'ぅ' => "u",
        'ぇ' => "e",
        'ぉ' => "o",
        'ゃ' => "ya",
        'ゅ' => "yu",
        'ょ' => "yo",
        'ゎ' => "wa",
        _ => return None,
    })
}

fn yoon(c: char) -> Option<&'static str> {
    Some(match c {
        'ゃ' => "a",
        'ゅ' => "u",
        'ょ' => "o",
        _ => return None,
    })
}

struct Syllable {
    kana: String,
    romaji: String,
}

fn syllables(kana: &str) -> Vec<Syllable> {
    let chars: Vec<char> = kana.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        let small = chars.get(i + 1).copied().and_then(yoon);
        let base = kana_to_romaji(c);

        if let (Some(small), Some(base)) = (small, base)
            && base.len() > 1
            && base.ends_with('i')
        {
            // きゃ -> kya, しゃ -> sha, ちゃ -> cha, じゃ -> ja
            let stem = &base[..base.len() - 1];
            let romaji = if stem.ends_with("sh") || stem.ends_with("ch") || stem.ends_with('j') {
                format!("{stem}{small}")
            } else {
                format!("{stem}y{small}")
            };
            out.push(Syllable {
                kana: format!("{c}{}", chars[i + 1]),
                romaji,
            });
            i += 2;
            continue;
        }

        out.push(Syllable {
            kana: c.to_string(),
            romaji: base.map(str::to_string).unwrap_or_else(|| c.to_string()),
        });
        i += 1;
    }

    out
}

fn doubled(next: &str) -> &str {
    if next.starts_with("ch") {
        return "t";
    }

    let mut chars = next.chars();
    match chars.next() {
        Some(c) if !"aeiou".contains(c) && c.is_alphanumeric() => &next[..c.len_utf8()],
        _ => "",
    }
}

/// Folds a long vowel into a macron: おう and おお give ō, うう gives ū.
fn lengthen(out: &mut String, kana: &str) -> bool {
    if (kana == "う" && (out.ends_with('o') || out.ends_with('u')))
        || (kana == "お" && out.ends_with('o'))
    {
        let last = out.pop().unwrap();
        out.push(if last == 'u' { 'ū' } else { 'ō' });
        return true;
    }

    false
}

/// The prolonged sound mark ー lengthens the vowel before it (コーヒー -> kōhī).
fn prolong(out: &mut String) -> bool {
    let Some(last) = out.chars().last() else {
        return false;
    };

    let marked = match last {
        'a' => 'ā',
        'i' => 'ī',
        'u' => 'ū',
        'e' => 'ē',
        'o' => 'ō',
        _ => return false,
    };

    out.pop();
    out.push(marked);
    true
}

/// Modified Hepburn with macrons (きょう -> kyō, おおきい -> ōkii).
///
/// `following` is the kana written after this reading in the same word, so a
/// trailing っ can double the next syllable's consonant (がっ before こう ->
/// gak).
pub fn to_hepburn(reading: &str, following: &str) -> String {
    let kana = to_hiragana(reading);
    let following = to_hiragana(following);
    let parts = syllables(&kana);
    let after: Vec<Syllable> = parts
        .iter()
        .map(|s| Syllable {
            kana: s.kana.clone(),
            romaji: s.romaji.clone(),
        })
        .chain(syllables(&following))
        .collect();

    let mut out = String::new();

    for (i, part) in parts.iter().enumerate() {
        let next = after[i + 1..]
            .iter()
            .find(|s| s.kana != "っ")
            .map(|s| s.romaji.as_str())
            .unwrap_or("");

        if part.kana == "っ" {
            out.push_str(doubled(next));
        } else if part.kana == "ー" {
            prolong(&mut out);
        } else if part.kana == "ん" {
            // n' before a vowel or y, so んい stays one syllable.
            let vowel_next = next.starts_with(['a', 'e', 'i', 'o', 'u', 'y']);
            let more = i + 1 < parts.len();
            out.push('n');
            if more && vowel_next {
                out.push('\'');
            }
        } else if !lengthen(&mut out, &part.kana) {
            out.push_str(&part.romaji);
        }
    }

    out
}

pub struct RomajiRenderer<'a> {
    painter: RubyPainter<'a>,
}

impl<'a> RomajiRenderer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        font: FontRef<'a>,
        scale_ratio: f64,
        gutter_em: f64,
        position: RubyPosition,
        baseline_offset_em: f64,
        fit: bool,
        fit_floor: f64,
        tight: bool,
    ) -> Result<Self> {
        Ok(Self {
            painter: RubyPainter::new(
                font,
                scale_ratio,
                gutter_em,
                position,
                baseline_offset_em,
                fit,
                fit_floor,
                tight,
            )?,
        })
    }
}

/// Whether a character can be part of a kana reading.
fn is_kana(c: char) -> bool {
    let cp = c as u32;
    (0x3040..=0x30ff).contains(&cp)
}

impl RubyRenderer for RomajiRenderer<'_> {
    fn reading_for(&self, ch: char) -> Option<String> {
        if is_kana(ch) {
            Some(ch.to_string())
        } else {
            None
        }
    }

    fn prepare_text(&self, reading: &str, following: &str) -> Option<String> {
        if reading.is_empty() {
            return None;
        }

        let romaji = to_hepburn(reading, following);

        if romaji.is_empty() || romaji == reading {
            None
        } else {
            Some(romaji)
        }
    }

    fn required_targets(
        &self,
        reading: &str,
        following: &str,
        base_top: f64,
        base_bottom: f64,
        main_upem: f64,
    ) -> Result<BaselineTargets> {
        let Some(text) = self.prepare_text(reading, following) else {
            return Ok(BaselineTargets::default());
        };

        Ok(self
            .painter
            .required_targets(&text, base_top, base_bottom, main_upem)
            .unwrap_or_default())
    }

    fn draw_reading(
        &self,
        reading: &str,
        following: &str,
        final_path: &mut BezPath,
        span_x: f64,
        span_advance: f64,
        main_upem: f64,
        targets: &BaselineTargets,
    ) -> Result<()> {
        if let Some(text) = self.prepare_text(reading, following) {
            self.painter
                .draw(&text, final_path, span_x, span_advance, main_upem, targets);
        }

        Ok(())
    }

    fn can_draw_text(&self, text: &str) -> bool {
        self.painter.collect(text).is_some()
    }

    fn ranges(&self) -> &[RangeInclusive<u32>] {
        &[CJK_RANGE, CJK_EXT_A_RANGE, HIRAGANA_RANGE, KATAKANA_RANGE]
    }
}

#[cfg(test)]
mod tests {
    use super::to_hepburn;

    #[test]
    fn word_readings() {
        assert_eq!(to_hepburn("きょう", ""), "kyō");
        assert_eq!(to_hepburn("がっ", "こう"), "gak");
        assert_eq!(to_hepburn("こう", ""), "kō");
        assert_eq!(to_hepburn("おとな", ""), "otona");
        assert_eq!(to_hepburn("にほん", ""), "nihon");
        assert_eq!(to_hepburn("しゅっ", "たい"), "shut");
        assert_eq!(to_hepburn("あした", ""), "ashita");
        assert_eq!(to_hepburn("ローマじ", ""), "rōmaji");
        assert_eq!(to_hepburn("しんぶん", ""), "shinbun");
        assert_eq!(to_hepburn("まっ", "ちゃ"), "mat");
    }
}
