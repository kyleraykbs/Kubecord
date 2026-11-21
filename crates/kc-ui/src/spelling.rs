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
    fn known_words_and_contractions_pass() {
        assert!(is_known("hello"));
        assert!(is_known("Hello"));
        assert!(misspelled_spans("don't worry").is_empty());
    }
}
