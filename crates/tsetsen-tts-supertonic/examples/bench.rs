//! Load and render times per acceleration setting and step count.
//!
//! ```sh
//! cargo run --release -p tsetsen-tts-supertonic --example bench
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tsetsen_core::ports::{SpeechRequest, TextToSpeech};
use tsetsen_core::{Ctx, RequestId};
use tsetsen_tts_supertonic::{Acceleration, SupertonicConfig, SupertonicTts};

#[tokio::main]
async fn main() {
    let dir = PathBuf::from(std::env::var("HOME").unwrap()).join(".tsetsen/models/supertonic-3");
    let cx = Ctx::new(RequestId::new(1));
    let clause = SpeechRequest {
        text: "Try setting specific work hours".into(),
        ..SpeechRequest::default()
    };
    let sentence = SpeechRequest {
        text: "Creating a dedicated workspace can also help you separate work from personal life."
            .into(),
        ..SpeechRequest::default()
    };
    for acceleration in [
        Acceleration::None,
        Acceleration::Denoiser,
        Acceleration::All,
    ] {
        for steps in [4, 6, 8] {
            let started = Instant::now();
            let config = SupertonicConfig::new("bench", &dir)
                .with_acceleration(acceleration)
                .with_steps(steps);
            let tts = SupertonicTts::load(&config).unwrap();
            let load = started.elapsed();
            tts.synthesize(&clause, &cx).await.unwrap(); // warm up
            let mut best = [Duration::MAX; 2];
            for _ in 0..3 {
                for (i, request) in [&clause, &sentence].into_iter().enumerate() {
                    let started = Instant::now();
                    tts.synthesize(request, &cx).await.unwrap();
                    best[i] = best[i].min(started.elapsed());
                }
            }
            println!(
                "{acceleration:<9?} steps {steps}: load {:>5} ms · short clause {:>4} ms · sentence {:>4} ms",
                load.as_millis(),
                best[0].as_millis(),
                best[1].as_millis()
            );
        }
    }
}
