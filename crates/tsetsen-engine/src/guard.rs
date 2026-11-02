/// A cleanup result and what it was produced from.
#[derive(Clone, Copy, Debug)]
pub struct RewriteCheck<'a> {
    /// The transcript as heard.
    pub raw: &'a str,
    /// The cleanup result, trimmed.
    pub rewritten: &'a str,
    /// The user's vocabulary. Cleanup may respell a misheard word as one of
    /// these ("tseten" → "Tsetsen"), so they count as source text.
    pub vocabulary: &'a [String],
}

/// Accepts or rejects a cleanup result before it reaches the user's document.
pub trait RewriteGuard: Send + Sync {
    /// Whether `check.rewritten` is an acceptable cleanup of `check.raw`.
    fn accepts(&self, check: &RewriteCheck<'_>) -> bool;
}

/// Accepts a rewrite only if almost all of its text came from the raw transcript.
///
/// Cleanup should remove and reformat words, not add them. This cheaply catches
/// the worst failure: a model that *answers* ("what's the capital of France"
/// becoming "Paris") or writes new content instead of cleaning up.
///
/// Matching is lenient where real cleanup legitimately differs from speech:
/// - **Splitting and joining:** `lets` → `let's`, `4pm` → `4 PM`, `e mail` →
///   `email`. Each rewritten word only has to occur somewhere in the raw text
///   with spaces and punctuation removed.
/// - **Spoken numbers:** `four thirty` → `4:30` (English number words).
/// - **Vocabulary:** the user's own terms, which ASR often mishears.
///
/// Coverage is weighted by characters, so a long invented word counts more than
/// a reused "a". An empty rewrite of non-empty speech is rejected, so the
/// user's words are never silently dropped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlapGuard {
    min_overlap: f32,
}

impl OverlapGuard {
    /// Requires at least `min_overlap` (clamped to `0.0..=1.0`; NaN means 1.0)
    /// of the rewritten text to come from the raw transcript.
    #[must_use]
    pub fn new(min_overlap: f32) -> Self {
        let min_overlap = if min_overlap.is_nan() {
            1.0
        } else {
            min_overlap.clamp(0.0, 1.0)
        };
        Self { min_overlap }
    }
}

impl Default for OverlapGuard {
    fn default() -> Self {
        Self::new(0.8)
    }
}

impl RewriteGuard for OverlapGuard {
    #[allow(clippy::cast_precision_loss)] // Character counts are far below f32's exact-integer range.
    fn accepts(&self, check: &RewriteCheck<'_>) -> bool {
        let source = Source::new(check.raw);
        let (mut total, mut covered) = (0usize, 0usize);
        for word in words(check.rewritten) {
            let length = word.chars().count();
            total += length;
            if source.contains(&word) || in_vocabulary(check.vocabulary, &word) {
                covered += length;
            }
        }
        if total == 0 {
            return source.is_empty();
        }
        covered as f32 / total as f32 >= self.min_overlap
    }
}

/// The raw transcript, lowercased and joined without separators, with the
/// digit forms of spoken numbers appended: each number word's own value, and
/// the value of each run of number words ("twenty five" → 20, 5, 25;
/// "march third" → 3, 3rd; "fifty thousand" → 50, 50000).
struct Source(String);

impl Source {
    fn new(raw: &str) -> Self {
        let mut joined = String::with_capacity(raw.len() * 2);
        let mut run = NumberRun::default();
        for word in words(raw) {
            joined.push_str(&word);
            if let Some(forms) = run.feed(&word) {
                joined.push_str(&forms);
            }
            if let Some((value, ordinal)) = cardinal(&word)
                .map(|v| (v, false))
                .or_else(|| ordinal(&word).map(|v| (v, true)))
            {
                push_number(&mut joined, value, ordinal);
            }
        }
        if let Some(forms) = run.finish() {
            joined.push_str(&forms);
        }
        Self(joined)
    }

    fn contains(&self, word: &str) -> bool {
        self.0.contains(word)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether `word` (lowercase) is one of the words of a vocabulary entry.
fn in_vocabulary(vocabulary: &[String], word: &str) -> bool {
    vocabulary
        .iter()
        .any(|entry| words(entry).any(|term| term == word))
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

/// Accumulates a run of English number words into its value, the standard
/// way: units add, "hundred" multiplies, "thousand"/"million" close a group.
#[derive(Default)]
struct NumberRun {
    total: u64,
    current: u64,
    active: bool,
    ordinal: bool,
}

impl NumberRun {
    /// Feeds one word. Returns the finished run's digit forms when `word` ends it.
    fn feed(&mut self, word: &str) -> Option<String> {
        if let Some(value) = cardinal(word) {
            self.current = self.current.saturating_add(value);
        } else if let Some(value) = ordinal(word) {
            self.current = self.current.saturating_add(value);
            self.ordinal = true;
        } else if self.active && word == "hundred" {
            self.current = self.current.max(1).saturating_mul(100);
        } else if let (true, Some(scale)) = (self.active, scale(word)) {
            self.total = self
                .total
                .saturating_add(self.current.max(1).saturating_mul(scale));
            self.current = 0;
        } else if self.active && word == "and" {
            // "one hundred and five" continues the run.
        } else {
            return self.finish();
        }
        self.active = true;
        if self.ordinal {
            // An ordinal always ends a number: "twenty third", "first".
            return self.finish();
        }
        None
    }

    fn finish(&mut self) -> Option<String> {
        let run = std::mem::take(self);
        run.active.then(|| {
            let mut forms = String::new();
            push_number(
                &mut forms,
                run.total.saturating_add(run.current),
                run.ordinal,
            );
            forms
        })
    }
}

fn push_number(out: &mut String, value: u64, ordinal: bool) {
    let digits = value.to_string();
    out.push_str(&digits);
    if ordinal {
        out.push_str(&digits);
        out.push_str(match (value % 100, value % 10) {
            (11..=13, _) => "th",
            (_, 1) => "st",
            (_, 2) => "nd",
            (_, 3) => "rd",
            _ => "th",
        });
    }
}

fn cardinal(word: &str) -> Option<u64> {
    Some(match word {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

fn ordinal(word: &str) -> Option<u64> {
    Some(match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        "eleventh" => 11,
        "twelfth" => 12,
        "thirteenth" => 13,
        "fourteenth" => 14,
        "fifteenth" => 15,
        "sixteenth" => 16,
        "seventeenth" => 17,
        "eighteenth" => 18,
        "nineteenth" => 19,
        "twentieth" => 20,
        "thirtieth" => 30,
        _ => return None,
    })
}

fn scale(word: &str) -> Option<u64> {
    match word {
        "thousand" => Some(1_000),
        "million" => Some(1_000_000),
        "billion" => Some(1_000_000_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(raw: &str, rewritten: &str) -> bool {
        accepts_with(&[], raw, rewritten)
    }

    fn accepts_with(vocabulary: &[&str], raw: &str, rewritten: &str) -> bool {
        let vocabulary: Vec<String> = vocabulary.iter().map(|&v| v.to_owned()).collect();
        OverlapGuard::default().accepts(&RewriteCheck {
            raw,
            rewritten,
            vocabulary: &vocabulary,
        })
    }

    #[test]
    fn accepts_misheard_words_respelled_as_vocabulary() {
        let (raw, fixed) = (
            "ask amor about the tseten launch",
            "Ask Amar about the Tsetsen launch.",
        );
        assert!(!accepts(raw, fixed));
        assert!(accepts_with(&["Amar", "Tsetsen"], raw, fixed));
    }

    #[test]
    fn vocabulary_does_not_excuse_invented_text() {
        assert!(!accepts_with(
            &["Tsetsen"],
            "what is tsetsen",
            "Tsetsen is a voice agent that runs on your computer."
        ));
    }

    #[test]
    fn accepts_self_corrections_and_filler_removal() {
        assert!(accepts(
            "um so let's meet at 3 no wait 4pm on thursday",
            "Let's meet at 4pm on Thursday."
        ));
    }

    /// Observed from qwen3.5:4b against Ollama: the token-exact guard rejected this.
    #[test]
    fn accepts_resplit_contractions_and_times() {
        assert!(accepts(
            "um so lets meet at 3 no wait 4pm on thursday",
            "So, let's meet at 4 PM on Thursday."
        ));
    }

    #[test]
    fn accepts_spoken_numbers_written_as_digits() {
        assert!(accepts("call me at four thirty", "Call me at 4:30."));
        assert!(accepts("we need twenty five copies", "We need 25 copies."));
        assert!(accepts(
            "the budget is sixty thousand dollars",
            "The budget is $60,000."
        ));
        assert!(accepts("one hundred and five people", "105 people"));
        assert!(accepts("two point five million users", "2.5 million users"));
    }

    /// Observed from qwen3.5:4b against Ollama: rejected before ordinals and
    /// compound numbers were parsed.
    #[test]
    fn accepts_ordinals_and_compound_numbers() {
        assert!(accepts(
            "we need twenty five copies by march third",
            "We need 25 copies by March 3rd."
        ));
        assert!(accepts("on the twenty first of may", "On May 21st."));
        assert!(accepts("the first of november", "November 1st"));
    }

    #[test]
    fn number_runs_end_at_non_number_words() {
        let source = Source::new("one apple and two pears");
        assert!(source.contains("1apple"));
        assert!(!source.contains("3"), "separate numbers must not be summed");
    }

    #[test]
    fn rejects_answers_instead_of_transcripts() {
        assert!(!accepts("what's the capital of france", "Paris."));
        assert!(!accepts(
            "um let's meet at 3 no wait 4pm",
            "Sure! Here is a summary of your schedule."
        ));
    }

    #[test]
    fn rejects_dropping_all_speech() {
        assert!(!accepts("send it to the team", ""));
        assert!(accepts("", ""));
    }

    #[test]
    fn works_with_non_latin_scripts() {
        assert!(accepts(
            "сайн байна уу эээ найзаа",
            "Сайн байна уу, найзаа."
        ));
    }

    #[test]
    fn clamps_out_of_range_thresholds() {
        let check = |raw, rewritten| RewriteCheck {
            raw,
            rewritten,
            vocabulary: &[],
        };
        assert!(OverlapGuard::new(-3.0).accepts(&check("a", "b")));
        assert!(!OverlapGuard::new(f32::NAN).accepts(&check("a b", "a c")));
    }
}
