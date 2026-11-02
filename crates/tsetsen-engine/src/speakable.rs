//! Turns a streamed reply into clauses the talker can speak.
//!
//! The first clause is cut as early as it reads naturally, at a comma once
//! there is enough of it, so audio starts while the thinker is still writing.
//! Later clauses are whole sentences, which a TTS model phrases better.

/// Cuts streamed text into speakable clauses.
#[derive(Debug)]
pub struct ClauseChunker {
    buffer: String,
    emitted: bool,
    first_min_chars: usize,
    max_chars: usize,
}

impl Default for ClauseChunker {
    fn default() -> Self {
        Self::new(24, 220)
    }
}

impl ClauseChunker {
    /// The first clause may end at a comma once it has `first_min_chars`
    /// characters; no clause exceeds `max_chars` if the text has spaces.
    #[must_use]
    pub fn new(first_min_chars: usize, max_chars: usize) -> Self {
        Self {
            buffer: String::new(),
            emitted: false,
            first_min_chars,
            max_chars: max_chars.max(first_min_chars + 1),
        }
    }

    /// Adds a fragment; returns the clauses it completed, ready to speak.
    pub fn push(&mut self, fragment: &str) -> Vec<String> {
        self.buffer.push_str(fragment);
        let mut clauses = Vec::new();
        while let Some(end) = self.boundary() {
            let clause: String = self.buffer.drain(..end).collect();
            if let Some(clause) = speakable(&clause) {
                self.emitted = true;
                clauses.push(clause);
            }
        }
        clauses
    }

    /// Flushes whatever is left at the end of the reply.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        speakable(&rest)
    }

    /// The byte offset to cut at, if a clause is complete.
    fn boundary(&self) -> Option<usize> {
        let text = &self.buffer;
        let mut chars = text.char_indices().peekable();
        let mut count = 0;
        let mut soft = None;
        while let Some((i, c)) = chars.next() {
            count += 1;
            let next = chars.peek().map(|&(_, n)| n);
            let end = i + c.len_utf8();
            if c == '\n' && text[..i].trim().chars().any(char::is_alphanumeric) {
                return Some(end);
            }
            // Only cut once the following character shows the clause ended.
            let followed_by_space = next.is_some_and(char::is_whitespace);
            if matches!(c, '.' | '!' | '?') && followed_by_space && !is_abbreviation(&text[..i]) {
                return Some(end);
            }
            if matches!(c, ',' | ';' | ':') && followed_by_space {
                if !self.emitted && count >= self.first_min_chars {
                    return Some(end);
                }
                soft = Some(end);
            }
            // The first clause may also end before a word people pause at,
            // so audio starts without waiting for a long sentence to finish.
            if !self.emitted
                && c == ' '
                && count >= self.first_min_chars + 8
                && starts_with_pause_word(&text[end..])
            {
                return Some(end);
            }
            if count >= self.max_chars {
                let at_space = text[..end].rfind(' ').map(|s| s + 1);
                return soft.or(at_space).or(Some(end));
            }
        }
        None
    }
}

/// Whether `rest` starts with a word that commonly follows a spoken pause,
/// as a whole word (so "andrew" does not count).
fn starts_with_pause_word(rest: &str) -> bool {
    ["and ", "but ", "so ", "or ", "because ", "which ", "then "]
        .iter()
        .any(|word| rest.starts_with(word))
}

/// Whether the word ending at a `.` is an abbreviation or an initial, not the
/// end of a sentence.
fn is_abbreviation(before: &str) -> bool {
    let word = before
        .rsplit(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let word = word.trim_start_matches(['"', '\'']);
    word.chars().count() == 1 && word.chars().all(char::is_alphabetic)
        || word.contains('.') // "e.g", "u.s"
        || matches!(
            word,
            "mr" | "mrs" | "ms" | "dr" | "prof" | "st" | "sr" | "jr" | "vs" | "etc" | "approx"
                | "no" | "jan" | "feb" | "mar" | "apr" | "jun" | "jul" | "aug" | "sep" | "sept"
                | "oct" | "nov" | "dec" | "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun"
        )
}

/// Strips markup that should not be read aloud. Returns `None` if nothing
/// speakable is left.
#[must_use]
pub fn speakable(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.trim_start_matches('#').trim_start();
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("• "))
            .unwrap_or(line);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&strip_links(line));
    }
    let out: String = out
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_' | '#'))
        .collect();
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().any(char::is_alphanumeric).then_some(out)
}

/// `[text](url)` becomes `text`.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(fragments: &[&str]) -> Vec<String> {
        let mut chunker = ClauseChunker::default();
        let mut clauses: Vec<String> = fragments.iter().flat_map(|f| chunker.push(f)).collect();
        clauses.extend(chunker.finish());
        clauses
    }

    /// Every way of splitting `text` into fragments gives the same clauses.
    fn chunk_every_way(text: &str) -> Vec<String> {
        let whole = chunk(&[text]);
        for size in 1..8 {
            let chars: Vec<char> = text.chars().collect();
            let pieces: Vec<String> = chars.chunks(size).map(|c| c.iter().collect()).collect();
            let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
            assert_eq!(chunk(&refs), whole, "fragment size {size}");
        }
        whole
    }

    #[test]
    fn first_clause_starts_early_then_whole_sentences() {
        assert_eq!(
            chunk_every_way(
                "Sure, I can help with that, and it won't take long. First, open Settings, then Privacy. Done!"
            ),
            [
                "Sure, I can help with that,",
                "and it won't take long.",
                "First, open Settings, then Privacy.",
                "Done!"
            ]
        );
    }

    #[test]
    fn a_long_first_sentence_starts_at_a_pause_word() {
        assert_eq!(
            chunk_every_way(
                "Try setting specific work hours and taking regular breaks to keep your energy up. You can also do more and more."
            ),
            [
                "Try setting specific work hours",
                "and taking regular breaks to keep your energy up.",
                "You can also do more and more."
            ]
        );
        // Not when it would leave a fragment too short to phrase well.
        assert_eq!(
            chunk_every_way("Cats and dogs are fine."),
            ["Cats and dogs are fine."]
        );
    }

    #[test]
    fn short_lead_ins_are_not_cut_off_alone() {
        assert_eq!(chunk_every_way("Yes, it is."), ["Yes, it is."]);
    }

    #[test]
    fn does_not_cut_at_abbreviations_decimals_or_initials() {
        assert_eq!(
            chunk_every_way(
                "Dr. Smith said version 3.5 ships on Jan. 4, e.g. next week. J. R. agreed."
            ),
            [
                "Dr. Smith said version 3.5 ships on Jan. 4,",
                "e.g. next week.",
                "J. R. agreed."
            ]
        );
    }

    #[test]
    fn strips_markdown_and_skips_empty_clauses() {
        assert_eq!(
            chunk_every_way(
                "## Steps\n- **Open** the [docs](https://x.y/z).\n- Run `make`.\n---\n"
            ),
            ["Steps", "Open the docs.", "Run make."]
        );
    }

    #[test]
    fn caps_long_unpunctuated_text() {
        let text = "word ".repeat(100);
        let clauses = chunk(&[&text]);
        assert!(
            clauses.iter().all(|c| c.chars().count() <= 220),
            "{clauses:?}"
        );
        assert_eq!(clauses.join(" ").split_whitespace().count(), 100);
    }

    #[test]
    fn waits_for_the_character_after_a_period() {
        let mut chunker = ClauseChunker::default();
        assert!(chunker.push("It costs 3.").is_empty());
        assert!(chunker.push("5 dollars").is_empty());
        assert_eq!(chunker.push(". Okay").as_slice(), ["It costs 3.5 dollars."]);
        assert_eq!(chunker.finish().as_deref(), Some("Okay"));
    }
}
