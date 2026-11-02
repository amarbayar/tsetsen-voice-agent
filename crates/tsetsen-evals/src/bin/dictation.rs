//! End to end from speech: each case is spoken by several macOS voices, then
//! runs through real Parakeet STT, real cleanup, the guard and the engine.
//!
//! ```sh
//! cargo run --release -p tsetsen-evals --bin dictation [-- <cleanup-model>]
//! ```
//!
//! Environment: `TSETSEN_EVAL_BASE_URL` (default Ollama), `TSETSEN_EVAL_BUDGET_MS`
//! (a fixed cleanup budget; default: the product's length-scaled one), `TSETSEN_EVAL_VOICES`
//! (comma-separated, default `Samantha,Daniel,Karen`), `TSETSEN_PARAKEET_DIR`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tsetsen_core::ports::SpeechToText;
use tsetsen_core::{Ctx, RequestId};
use tsetsen_engine::{DictationError, RewriteBudget};
use tsetsen_evals::{
    BoxError, DEFAULT_CLEANUP_MODEL, RecordingStt, Verdict, engine, judge, load_cases,
    openai_rewriter, request, speak, summarize, warm_up,
};
use tsetsen_stt_parakeet::{ParakeetConfig, ParakeetStt};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let model = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CLEANUP_MODEL.to_owned());
    let base_url = std::env::var("TSETSEN_EVAL_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let budget = match std::env::var("TSETSEN_EVAL_BUDGET_MS") {
        Ok(ms) => RewriteBudget::fixed(Duration::from_millis(ms.parse()?)),
        Err(_) => RewriteBudget::default(),
    };
    let voices: Vec<String> = std::env::var("TSETSEN_EVAL_VOICES")
        .unwrap_or_else(|_| "Samantha,Daniel,Karen".into())
        .split(',')
        .map(str::to_owned)
        .collect();
    let parakeet_dir = std::env::var_os("TSETSEN_PARAKEET_DIR").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".tsetsen/models/parakeet-tdt-0.6b-v3")
        },
        PathBuf::from,
    );

    let loaded = Instant::now();
    let parakeet: Arc<dyn SpeechToText> = Arc::new(ParakeetStt::load(&ParakeetConfig::new(
        "parakeet",
        parakeet_dir,
    ))?);
    println!("parakeet loaded in {:?}", loaded.elapsed());
    let rewriter = openai_rewriter(&base_url, &model)?;
    warm_up(&rewriter).await;

    let cases = load_cases()?;
    println!(
        "\n=== parakeet → {model}  ({} cases × {} voices, cleanup budget {budget:?})",
        cases.len(),
        voices.len()
    );
    let (mut verdicts, mut total_ms, mut stt_ms) = (Vec::new(), Vec::new(), Vec::new());
    for case in &cases {
        for voice in &voices {
            let audio = speak(voice, &case.said)?;
            let stt = Arc::new(RecordingStt::new(Arc::clone(&parakeet)));
            let engine = engine(Arc::clone(&stt) as _, &rewriter, budget)?;

            let started = Instant::now();
            let result = engine
                .run(&request(audio, case.hints()), &Ctx::new(RequestId::new(1)))
                .await;
            let elapsed = started.elapsed().as_millis();
            let heard = stt.take_last();

            let (verdict, text, notes) = match result {
                Ok(outcome) => {
                    let judged = judge(case, &outcome);
                    (judged.verdict, judged.text, judged.notes)
                }
                Err(DictationError::NoSttProvider { .. }) => (
                    Verdict::Skip,
                    String::new(),
                    vec![format!("no STT provider for {:?}", case.language)],
                ),
                Err(error) => (Verdict::Fail, String::new(), vec![error.to_string()]),
            };
            let stt_note = heard.map(|(transcript, took)| {
                stt_ms.push(took.as_millis());
                format!("heard ({} ms): {:?}", took.as_millis(), transcript.text)
            });
            if verdict != Verdict::Skip {
                total_ms.push(elapsed);
            }
            println!(
                "{} {elapsed:>5} ms  {:<28} {voice:<9} {text:?}",
                verdict.label(),
                case.id
            );
            if verdict != Verdict::Pass {
                for note in stt_note.iter().chain(&notes) {
                    println!("                {note}");
                }
            }
            verdicts.push(verdict);
        }
    }
    summarize(&format!("parakeet → {model}"), &verdicts, total_ms);
    let (p50, p90, max) = tsetsen_evals::percentiles(stt_ms);
    println!("    speech-to-text alone: p50 {p50} ms, p90 {p90} ms, max {max} ms");
    Ok(())
}
