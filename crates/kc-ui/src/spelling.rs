//! A small, self-contained spell checker for the message composer.
//!
//! Misspelled words are underlined in red as you type. The dictionary is a
//! bundled English word list (compressed, decoded once on a background thread),
//! plus a per-user list of added words kept in the config directory so names and
//! jargon can be taught. Checking is case-insensitive.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, RwLock};

/// Whether checking is on (a setting). When off, nothing is underlined.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// The bundled dictionary, decompressed on first use.
static BUILTIN: LazyLock<HashSet<String>> = LazyLock::new(|| {
    let compressed: &[u8] = include_bytes!("../assets/words.txt.gz");
    let mut text = String::new();
    if flate2::read::GzDecoder::new(compressed)
        .read_to_string(&mut text)
        .is_err()
    {
        return HashSet::new();
    }
    text.lines()
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
});

/// How common words are, as SCOWL size levels (10 = the most common; see
/// assets/SCOWL-COPYRIGHT.txt), for ranking suggestions.
static TIERS: LazyLock<std::collections::HashMap<String, u8>> = LazyLock::new(|| {
    let compressed: &[u8] = include_bytes!("../assets/word_tiers.txt.gz");
    let mut text = String::new();
    if flate2::read::GzDecoder::new(compressed)
        .read_to_string(&mut text)
        .is_err()
    {
        return std::collections::HashMap::new();
    }
    let mut tier = u8::MAX;
    let mut tiers = std::collections::HashMap::new();
    for line in text.lines() {
        match line.strip_prefix('#') {
            Some(level) => tier = level.parse().unwrap_or(u8::MAX),
            None => {
                tiers.insert(line.to_owned(), tier);
            }
        }
    }
    tiers
});

/// Words the user has added, mirrored to disk.
static USER: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(load_user_words()));

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Forces the bundled dictionary to decompress now (call off the main thread at
/// startup so the first keystroke does not pay for it).
pub(crate) fn warm() {
    LazyLock::force(&BUILTIN);
    LazyLock::force(&USER);
    LazyLock::force(&TIERS);
}

/// Whether `word` is spelled correctly (case-insensitive).
pub(crate) fn is_known(word: &str) -> bool {
    let word = word.to_lowercase();
    if in_dictionaries(&word) {
        return true;
    }
    // The bundled list is alphabetic-only, so contractions and possessives are
    // stored without the apostrophe (don't -> dont, players' -> players).
    if word.contains('\'') {
        let stripped: String = word.chars().filter(|&c| c != '\'').collect();
        return !stripped.is_empty() && in_dictionaries(&stripped);
    }
    false
}

fn in_dictionaries(word: &str) -> bool {
    BUILTIN.contains(word)
        || USER
            .read()
            .map(|user| user.contains(word))
            .unwrap_or(false)
}

/// Teaches the checker `word`, persisting it. A no-op for blank input or a word
/// already known.
pub(crate) fn add_word(word: &str) {
    let word = word.trim().to_lowercase();
    if word.is_empty() {
        return;
    }
    let inserted = USER
        .write()
        .map(|mut user| user.insert(word.clone()))
        .unwrap_or(false);
    if inserted {
        save_user_words();
    }
}

/// Forgets a previously added word.
pub(crate) fn remove_word(word: &str) {
    let word = word.to_lowercase();
    let removed = USER
        .write()
        .map(|mut user| user.remove(&word))
        .unwrap_or(false);
    if removed {
        save_user_words();
    }
}

/// The user-added words, sorted, for the settings list.
pub(crate) fn custom_words() -> Vec<String> {
    let mut words: Vec<String> = USER
        .read()
        .map(|user| user.iter().cloned().collect())
        .unwrap_or_default();
    words.sort();
    words
}

/// Character ranges (start, end) of misspelled words in `text`, as char offsets
/// suitable for a `TextBuffer`. Skips URLs, mentions, `:shortcodes:`, numbers
/// and acronyms so the common cases are not flagged.
pub(crate) fn misspelled_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // One whitespace-delimited token.
        let token_start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let token = &chars[token_start..i];
        // Skip anything that looks like a URL, mention, channel, shortcode,
        // path, code span or number -- not prose to spell-check.
        let structural = token.iter().any(|&c| {
            matches!(c, '/' | ':' | '@' | '#' | '.' | '\\' | '`' | '~' | '_') || c.is_ascii_digit()
        });
        if structural {
            continue;
        }
        // Within the token, check each alphabetic run (keeping apostrophes, for
        // contractions like "don't").
        let mut k = token_start;
        while k < i {
            if !chars[k].is_alphabetic() {
                k += 1;
                continue;
            }
            let word_start = k;
            while k < i && (chars[k].is_alphabetic() || chars[k] == '\'') {
                k += 1;
            }
            // Trim trailing apostrophes ("players'" -> "players").
            let mut word_end = k;
            while word_end > word_start && chars[word_end - 1] == '\'' {
                word_end -= 1;
            }
            let word: String = chars[word_start..word_end].iter().collect();
            let count = word.chars().count();
            let acronym = word.chars().all(char::is_uppercase);
            if count >= 2 && !acronym && !is_known(&word) {
                spans.push((word_start, word_end));
            }
        }
    }
    spans
}

/// Up to `limit` dictionary words close to `word` (at most two edits away,
/// a swap of neighbours counting as one), best first, cased like `word`.
pub(crate) fn suggestions(word: &str, limit: usize) -> Vec<String> {
    let lower = word.to_lowercase();
    let target: Vec<char> = lower.chars().collect();
    if target.is_empty() {
        return Vec::new();
    }
    let mut letters = target.clone();
    letters.sort_unstable();
    let user = USER.read().map(|user| user.clone()).unwrap_or_default();
    let mut scored: Vec<(usize, u8, bool, bool, usize, &str)> = Vec::new();
    for candidate in BUILTIN.iter().chain(user.iter()) {
        let length = candidate.chars().count();
        if length.abs_diff(target.len()) > 2 {
            continue;
        }
        let chars: Vec<char> = candidate.chars().collect();
        let Some(distance) = edit_distance(&target, &chars, 2).filter(|distance| *distance > 0)
        else {
            continue;
        };
        // Ties: the more common word, then the same letters reordered (a
        // slip), then the same first letter, then the closest length. Words
        // the user added count as common.
        let mut sorted = chars.clone();
        sorted.sort_unstable();
        let tier = if user.contains(candidate) {
            0
        } else {
            TIERS.get(candidate).copied().unwrap_or(u8::MAX)
        };
        scored.push((
            distance,
            tier,
            sorted != letters,
            chars.first() != target.first(),
            length.abs_diff(target.len()),
            candidate.as_str(),
        ));
    }
    scored.sort();
    let capitalised = word.chars().next().is_some_and(char::is_uppercase);
    let mut out: Vec<String> = Vec::new();
    for (.., candidate) in scored {
        let cased = if capitalised {
            let mut chars = candidate.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        } else {
            candidate.to_owned()
        };
        if !out.contains(&cased) {
            out.push(cased);
        }
        if out.len() == limit {
            break;
        }
    }
    out
}

/// Edits between `a` and `b` (insert, delete, replace, or swap two
/// neighbours), or `None` when more than `most`.
fn edit_distance(a: &[char], b: &[char], most: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > most {
        return None;
    }
    let mut before = vec![0usize; m + 1];
    let mut previous: Vec<usize> = (0..=m).collect();
    let mut current = vec![0usize; m + 1];
    for i in 1..=n {
        current[0] = i;
        let mut row_best = current[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut value = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                value = value.min(before[j - 2] + 1);
            }
            current[j] = value;
            row_best = row_best.min(value);
        }
        if row_best > most {
            return None;
        }
        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    (previous[m] <= most).then_some(previous[m])
}

fn user_dict_path() -> Option<PathBuf> {
    kc_core::paths::config_dir()
        .ok()
        .map(|dir| dir.join("dictionary.txt"))
}

fn load_user_words() -> HashSet<String> {
    let Some(path) = user_dict_path() else {
        return HashSet::new();
    };
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

fn save_user_words() {
    let Some(path) = user_dict_path() else {
        return;
    };
    let mut words: Vec<String> = USER
        .read()
        .map(|user| user.iter().cloned().collect())
        .unwrap_or_default();
    words.sort();
    let body = words.join("\n");
    if let Err(error) = std::fs::write(&path, body) {
        tracing::warn!(%error, "could not save the user dictionary");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_misspelled_words_and_skips_structure() {
        // "teh" is misspelled; "the" and "cat" are fine.
        let spans = misspelled_spans("teh cat");
        assert_eq!(spans, vec![(0, 3)]);

        // URLs, mentions, shortcodes, numbers and acronyms are left alone.
        assert!(misspelled_spans("see https://example.com/teh/path").is_empty());
        assert!(misspelled_spans("hi @someone in #general").is_empty());
        assert!(misspelled_spans("nice :smile: 1234 LOL").is_empty());
    }

    #[test]
    fn offsets_are_char_based() {
        // A leading multi-byte char must not shift the reported offsets.
        let spans = misspelled_spans("é teh");
        assert_eq!(spans, vec![(2, 5)]);
    }

    #[test]
    fn suggestions_put_the_likely_word_first() {
        assert_eq!(suggestions("teh", 3).first().map(String::as_str), Some("the"));
        assert_eq!(suggestions("recieve", 3).first().map(String::as_str), Some("receive"));
        assert_eq!(suggestions("Wrold", 3).first().map(String::as_str), Some("World"));
        assert_eq!(suggestions("mesage", 3).first().map(String::as_str), Some("message"));
        assert_eq!(suggestions("seperate", 3).first().map(String::as_str), Some("separate"));
        assert!(suggestions("teh", 5).len() <= 5);
    }

    #[test]
    fn edit_distance_counts_a_swap_as_one() {
        let chars = |text: &str| text.chars().collect::<Vec<_>>();
        assert_eq!(edit_distance(&chars("teh"), &chars("the"), 2), Some(1));
        assert_eq!(edit_distance(&chars("cat"), &chars("cart"), 2), Some(1));
        assert_eq!(edit_distance(&chars("abc"), &chars("xyz"), 2), None);
    }

    #[test]
    fn known_words_and_contractions_pass() {
        assert!(is_known("hello"));
        assert!(is_known("Hello"));
        assert!(misspelled_spans("don't worry").is_empty());
    }
}
