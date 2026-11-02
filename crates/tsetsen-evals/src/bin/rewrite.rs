//! Cleanup models through the real `DictationEngine` and guard. Each case's
//! `said` text stands in for the transcript, isolating cleanup quality.
//!
//! ```sh
//! cargo run --release -p tsetsen-evals --bin rewrite [-- <model>...]
//! ```
//!
//! Environment: `TSETSEN_EVAL_BASE_URL` (default Ollama), `TSETSEN_EVAL_BUDGET_MS`
//! (fixed cleanup budget, default 2000, to measure quality apart from speed).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tsetsen_core::ports::SpeechToText;
use tsetsen_core::{Ctx, Layer, RequestId};
use tsetsen_engine::RewriteBudget;
use tsetsen_evals::{
    BoxError, DEFAULT_CLEANUP_MODEL, engine, judge, load_cases, openai_rewriter, request,
    summarize, warm_up,
};
use tsetsen_testkit::{FakeStt, local, one_second_of_audio};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut models: Vec<String> = std::env::args().skip(1).collect();
    if models.is_empty() {
        models.push(DEFAULT_CLEANUP_MODEL.to_owned());
    }
    let base_url = std::env::var("TSETSEN_EVAL_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let budget = RewriteBudget::fixed(Duration::from_millis(
        std::env::var("TSETSEN_EVAL_BUDGET_MS").map_or(Ok(2000), |v| v.parse())?,
    ));
    let cases = load_cases()?;

    for model in &models {
        println!("\n=== {model}  ({} cases, budget {budget:?})", cases.len());
        let rewriter = openai_rewriter(&base_url, model)?;
        warm_up(&rewriter).await;

        let (mut verdicts, mut millis) = (Vec::new(), Vec::new());
        for case in &cases {
            let stt: Arc<dyn SpeechToText> =
                Arc::new(FakeStt::hearing(local("text", Layer::Stt), &case.said));
            let engine = engine(stt, &rewriter, budget)?;
            let started = Instant::now();
            let outcome = engine
                .run(
                    &request(one_second_of_audio(), case.hints()),
                    &Ctx::new(RequestId::new(1)),
                )
                .await?;
            let elapsed = started.elapsed().as_millis();
            let judged = judge(case, &outcome);
            println!(
                "{} {elapsed:>5} ms  {:<28} {:?}",
                judged.verdict.label(),
                case.id,
                judged.text
            );
            for note in &judged.notes {
                println!("                {note}");
            }
            verdicts.push(judged.verdict);
            millis.push(elapsed);
        }
        summarize(model, &verdicts, millis);
    }
    Ok(())
}
