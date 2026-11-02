//! Text normalization and encoding for Supertonic.
//!
//! Supertonic reads characters directly, with no phonemizer, so the only
//! preparation is normalizing punctuation and mapping characters to ids.

use std::path::Path;

use unicode_normalization::UnicodeNormalization;

/// Maps Unicode code points to the model's character ids.
pub(crate) struct CharIndex(Vec<i64>);

impl CharIndex {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        serde_json::from_reader(std::io::BufReader::new(file))
            .map(Self)
            .map_err(|e| e.to_string())
    }

    /// Encodes `text` wrapped in its language tag. Characters the model does
    /// not know are dropped rather than fed in as garbage.
    pub(crate) fn encode(&self, text: &str, language: &str) -> Vec<i64> {
        format!("<{language}>{text}</{language}>")
            .chars()
            .filter_map(|c| self.0.get(c as usize).copied())
            .filter(|&id| id >= 0)
            .collect()
    }
}

/// Normalizes text the way the model was trained: decomposed Unicode, plain
/// punctuation, single spaces, and a final punctuation mark.
pub(crate) fn normalize(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    for c in text.nfkd() {
        match c {
            '\u{2013}' | '\u{2011}' | '\u{2014}' => plain.push('-'),
            '\u{201C}' | '\u{201D}' => plain.push('"'),
            '\u{2018}' | '\u{2019}' | '\u{00B4}' | '`' => plain.push('\''),
            '_' | '[' | ']' | '|' | '/' | '#' | '\u{2192}' | '\u{2190}' => plain.push(' '),
            '\u{2665}' | '\u{2606}' | '\u{2661}' | '\u{00A9}' | '\\' => {}
            '@' => plain.push_str(" at "),
            c if is_emoji(c) => {}
            c => plain.push(c),
        }
    }
    let plain = plain
        .replace("e.g.,", "for example,")
        .replace("i.e.,", "that is,");

    let mut out = String::with_capacity(plain.len() + 1);
    for word in plain.split_whitespace() {
        if !out.is_empty() && !word.starts_with([',', '.', '!', '?', ';', ':', '\'']) {
            out.push(' ');
        }
        out.push_str(word);
    }
    while out.contains("\"\"") {
        out = out.replace("\"\"", "\"");
    }
    while out.contains("''") {
        out = out.replace("''", "'");
    }
    if let Some(last) = out.chars().last()
        && !".!?;:,'\")]}\u{2026}".contains(last)
    {
        out.push('.');
    }
    out
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F1E6..=0x1F1FF | 0x1F300..=0x1F6FF | 0x1F700..=0x1FAFF | 0x2600..=0x27BF)
}

/// Splits text longer than `max_chars` at sentence ends, then commas, then
/// spaces. The pipeline already sends clauses; this only guards the model's
/// input limit.
pub(crate) fn split(text: &str, max_chars: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > max_chars {
        let limit = rest
            .char_indices()
            .nth(max_chars)
            .map_or(rest.len(), |(i, _)| i);
        let head = &rest[..limit];
        let cut = [". ", "! ", "? ", "; ", ", ", " "]
            .iter()
            .find_map(|sep| head.rfind(sep).map(|i| i + sep.len()))
            .filter(|&i| i > 0)
            .unwrap_or(limit);
        pieces.push(rest[..cut].trim());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_punctuation_and_spacing() {
        assert_eq!(normalize("Hello , world"), "Hello, world.");
        assert_eq!(
            normalize("It\u{2019}s  done \u{2014} really?"),
            "It's done - really?"
        );
        assert_eq!(normalize("ping me @ noon 🎉"), "ping me at noon.");
        assert_eq!(normalize("a/b_c"), "a b c.");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn splits_long_text_at_natural_boundaries() {
        let text = "First sentence here. Second one, with a comma, is longer than the rest.";
        let pieces = split(text, 30);
        assert!(pieces.iter().all(|p| p.chars().count() <= 30), "{pieces:?}");
        assert_eq!(pieces[0], "First sentence here.");
        assert_eq!(pieces.join(" "), text);
    }

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(split("  Hi there.  ", 300), ["Hi there."]);
        assert_eq!(split("   ", 300), Vec::<&str>::new());
    }
}
