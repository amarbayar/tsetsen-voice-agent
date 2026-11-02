//! Shared machinery for evaluating Tsetsen pipelines against real models.
//!
//! Binaries:
//! - `rewrite`: cleanup models only; each case's text stands in for the transcript.
//! - `dictation`: end to end from synthesized speech through real STT and cleanup.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tsetsen_core::ports::{
    Anchor, AudioClip, RewriteRequest, Rewriter, SpeechToText, SttHints, Transcript,
};
use tsetsen_core::{Ctx, Describe, Layer, ProviderDescriptor, ProviderError, RequestId};
use tsetsen_engine::{
    DictationConfig, DictationEngine, DictationOutcome, DictationRequest, OverlapGuard,
    RewriteBudget, RewriteStatus,
};
use tsetsen_llm_openai::{OpenAiConfig, OpenAiRewriter};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::FakeDesktop;

/// Boxed error for eval binaries.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The cleanup model the evals use when none is named. Qwen3.5-9B passed every
/// case where 2B and 4B missed self-corrections in long or numeric speech.
pub const DEFAULT_CLEANUP_MODEL: &str = "qwen3.5:9b-mlx";

/// The repository root.
#[must_use]
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One evaluation case from `evals/rewrite.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct Case {
    /// Stable identifier.
    pub id: String,
    /// What the user says.
    pub said: String,
    /// Language hint.
    #[serde(default)]
    pub language: Option<String>,
    /// Vocabulary hint.
    #[serde(default)]
    pub vocabulary: Vec<String>,
    /// Case-insensitive phrases the output must contain.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Groups of alternatives; the output must contain one of each group.
    #[serde(default)]
    pub contains_any: Vec<Vec<String>>,
    /// Case-sensitive substrings the output must contain.
    #[serde(default)]
    pub contains_exact: Vec<String>,
    /// Whole words that must not appear in the output.
    #[serde(default)]
    pub absent_words: Vec<String>,
}

/// Loads `evals/rewrite.json`.
pub fn load_cases() -> Result<Vec<Case>, BoxError> {
    let path = repo_root().join("evals/rewrite.json");
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Lowercase words joined by single spaces, punctuation dropped, so
/// "Good morning, everyone." matches "good morning everyone". A trailing `?`
/// is kept as its own token so questions can be checked.
#[must_use]
pub fn normalize(text: &str) -> String {
    let mut words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    if text.trim_end().ends_with('?') {
        words.push("?".into());
    }
    words.join(" ")
}

impl Case {
    /// Every check `output` violates.
    #[must_use]
    pub fn failures(&self, output: &str) -> Vec<String> {
        let normalized = normalize(output);
        let words: Vec<&str> = normalized.split(' ').collect();
        let mut failures = Vec::new();
        for needle in &self.contains {
            if !normalized.contains(&normalize(needle)) {
                failures.push(format!("missing {needle:?}"));
            }
        }
        for group in &self.contains_any {
            if !group.iter().any(|n| normalized.contains(&normalize(n))) {
                failures.push(format!("missing any of {group:?}"));
            }
        }
        for needle in &self.contains_exact {
            if !output.contains(needle.as_str()) {
                failures.push(format!("missing exact {needle:?}"));
            }
        }
        for word in &self.absent_words {
            if words.contains(&word.to_lowercase().as_str()) {
                failures.push(format!("kept {word:?}"));
            }
        }
        failures
    }

    /// Recognition and cleanup hints for this case.
    #[must_use]
    pub fn hints(&self) -> SttHints {
        SttHints {
            language: self.language.as_deref().map(Into::into),
            vocabulary: self.vocabulary.clone(),
        }
    }
}

/// How a case went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Cleanup applied and every check holds.
    Pass,
    /// Cleanup was not applied (guard, timeout, error), but the raw text that
    /// was inserted still satisfies every check. Correct, just unpolished.
    Safe,
    /// The inserted text violates a check, or nothing was inserted.
    Fail,
    /// The case cannot run in this configuration (e.g. no STT for its language).
    Skip,
}

impl Verdict {
    /// Fixed-width label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Safe => "SAFE",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// A judged outcome.
#[derive(Clone, Debug)]
pub struct Judgement {
    /// The verdict.
    pub verdict: Verdict,
    /// The text that reached the document.
    pub text: String,
    /// Why, for anything but a clean pass.
    pub notes: Vec<String>,
}

/// Judges a dictation outcome against `case`.
#[must_use]
pub fn judge(case: &Case, outcome: &DictationOutcome) -> Judgement {
    let (text, status) = match outcome {
        DictationOutcome::Inserted {
            text, provenance, ..
        }
        | DictationOutcome::Held {
            text, provenance, ..
        } => (text.clone(), Some(provenance.rewrite.clone())),
        DictationOutcome::NothingHeard { .. } => (String::new(), None),
    };
    let mut notes = Vec::new();
    match &status {
        Some(RewriteStatus::Applied(_)) => {}
        Some(RewriteStatus::Rejected { candidate, .. }) => {
            notes.push(format!("guard rejected: {candidate:?}"));
        }
        Some(other) => notes.push(format!("cleanup not applied: {other:?}")),
        None => notes.push("nothing heard".into()),
    }
    let failures = case.failures(&text);
    let applied = matches!(status, Some(RewriteStatus::Applied(_)));
    let verdict = match (failures.is_empty(), applied) {
        (true, true) => Verdict::Pass,
        (true, false) => Verdict::Safe,
        (false, _) => Verdict::Fail,
    };
    notes.extend(failures);
    Judgement {
        verdict,
        text,
        notes,
    }
}

/// Builds a cleanup provider for an OpenAI-compatible endpoint, with
/// reasoning disabled (required for hybrid thinking models).
pub fn openai_rewriter(base_url: &str, model: &str) -> Result<Arc<dyn Rewriter>, BoxError> {
    let config = OpenAiConfig::new(model, base_url, model)?.with_reasoning_effort("none");
    Ok(Arc::new(OpenAiRewriter::new(&config)?))
}

/// Loads a model into the server's memory so the first case is not penalized.
pub async fn warm_up(rewriter: &Arc<dyn Rewriter>) {
    let request = RewriteRequest {
        transcript: "hello".into(),
        ..RewriteRequest::default()
    };
    let cx = Ctx::new(RequestId::new(0)).with_timeout(Duration::from_secs(120));
    let _ = rewriter.rewrite(&request, &cx).await;
}

/// A dictation engine over one STT and one rewriter, local-only egress, and a
/// desktop that verifies every insert.
pub fn engine(
    stt: Arc<dyn SpeechToText>,
    rewriter: &Arc<dyn Rewriter>,
    rewrite_budget: RewriteBudget,
) -> Result<DictationEngine, BoxError> {
    let egress = Arc::new(EgressPolicy::new());
    let stt_id = stt.descriptor().id.clone();
    let rewriter_id = rewriter.descriptor().id.clone();
    Ok(DictationEngine::new(
        Router::new(Layer::Stt, &Route::new(stt_id), [stt], Arc::clone(&egress))?,
        Arc::new(FakeDesktop::verified()),
    )
    .with_rewriter(Router::new(
        Layer::Rewrite,
        &Route::new(rewriter_id),
        [Arc::clone(rewriter)],
        egress,
    )?)
    .with_config(DictationConfig {
        rewrite_budget,
        guard: Arc::new(OverlapGuard::default()),
    }))
}

/// A dictation request into a `TextEdit` field.
#[must_use]
pub fn request(audio: AudioClip, hints: SttHints) -> DictationRequest {
    DictationRequest {
        anchor: Anchor {
            app: "com.apple.TextEdit".into(),
            window: "w-1".into(),
            element: Some("e-1".into()),
        },
        audio,
        hints,
    }
}

/// Renders `text` with a macOS voice as 16 kHz mono float audio, cached
/// under `target/eval-audio`.
pub fn speak(voice: &str, text: &str) -> Result<AudioClip, BoxError> {
    let dir = repo_root().join("target/eval-audio");
    std::fs::create_dir_all(&dir)?;
    let key = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let path = dir.join(format!("{voice}-{key:016x}.wav"));
    if !path.exists() {
        let status = Command::new("say")
            .args(["-v", voice, "-o"])
            .arg(&path)
            .args(["--file-format=WAVE", "--data-format=LEF32@16000", text])
            .status()?;
        if !status.success() {
            return Err(format!("`say -v {voice}` failed").into());
        }
    }
    let mut reader = hound::WavReader::open(&path)?;
    let rate = reader.spec().sample_rate;
    let samples = reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?;
    Ok(AudioClip::new(rate, samples)?)
}

/// Wraps an STT provider and remembers its last transcript and latency, so an
/// eval can report what was heard without changing the engine.
pub struct RecordingStt {
    inner: Arc<dyn SpeechToText>,
    last: Mutex<Option<(Transcript, Duration)>>,
}

impl RecordingStt {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn SpeechToText>) -> Self {
        Self {
            inner,
            last: Mutex::new(None),
        }
    }

    /// The last transcript and how long it took, if any.
    #[must_use]
    pub fn take_last(&self) -> Option<(Transcript, Duration)> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl Describe for RecordingStt {
    fn descriptor(&self) -> &ProviderDescriptor {
        self.inner.descriptor()
    }
}

#[async_trait]
impl SpeechToText for RecordingStt {
    async fn transcribe(
        &self,
        audio: &AudioClip,
        hints: &SttHints,
        cx: &Ctx,
    ) -> Result<Transcript, ProviderError> {
        let started = Instant::now();
        let result = self.inner.transcribe(audio, hints, cx).await;
        if let Ok(transcript) = &result {
            *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((transcript.clone(), started.elapsed()));
        }
        result
    }
}

/// Latency percentiles in milliseconds: (p50, p90, max).
#[must_use]
pub fn percentiles(mut millis: Vec<u128>) -> (u128, u128, u128) {
    if millis.is_empty() {
        return (0, 0, 0);
    }
    millis.sort_unstable();
    let at = |p: usize| millis[(millis.len() * p / 100).min(millis.len() - 1)];
    (at(50), at(90), millis[millis.len() - 1])
}

/// Prints a one-line summary of verdicts and latencies.
pub fn summarize(label: &str, verdicts: &[Verdict], millis: Vec<u128>) {
    let count = |v: Verdict| verdicts.iter().filter(|&&x| x == v).count();
    let (p50, p90, max) = percentiles(millis);
    println!(
        "--- {label}: {} pass, {} safe, {} fail, {} skip (of {})   p50 {p50} ms, p90 {p90} ms, max {max} ms",
        count(Verdict::Pass),
        count(Verdict::Safe),
        count(Verdict::Fail),
        count(Verdict::Skip),
        verdicts.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_ignores_punctuation_but_keeps_questions() {
        assert_eq!(
            normalize("Good morning, everyone."),
            "good morning everyone"
        );
        assert_eq!(normalize("Ready?"), "ready ?");
    }

    #[test]
    fn checks_are_meaning_based() {
        let case = Case {
            id: "t".into(),
            said: String::new(),
            language: None,
            vocabulary: vec![],
            contains: vec!["good morning everyone".into()],
            contains_any: vec![vec!["3 weeks".into(), "three weeks".into()]],
            contains_exact: vec![],
            absent_words: vec!["two".into()],
        };
        assert_eq!(
            case.failures("Good morning, everyone, in 3 weeks."),
            Vec::<String>::new()
        );
        assert_eq!(case.failures("good morning everyone two weeks").len(), 2);
    }

    #[test]
    fn percentiles_of_nothing_are_zero() {
        assert_eq!(percentiles(vec![]), (0, 0, 0));
        assert_eq!(percentiles(vec![5, 1, 9]), (5, 9, 9));
    }
}
