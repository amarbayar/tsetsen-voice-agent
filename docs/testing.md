# Testing

```sh
cargo test --workspace                   # unit, property and protocol tests; no models needed
cargo clippy --workspace --all-targets   # pedantic; warnings are errors in CI
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml`. CI runs all three on macOS, Linux and
Windows.

## Against real models

Tests that need a model, a device or an OS service are `#[ignore]`d, so CI stays hermetic.
Run them locally after `scripts/fetch-models.sh` and `ollama pull qwen3.5:9b-mlx`:

```sh
cargo test -p tsetsen-llm-openai --test live -- --ignored --test-threads=1
for c in stt-parakeet tts-supertonic vad-silero; do
  cargo test --release -p tsetsen-$c --test live -- --ignored --test-threads=1
done
# Desktop control: drives a real TextEdit; needs an unlocked screen. Skips if TextEdit is open.
cargo test -p tsetsen-agent-desktop --test live -- --ignored --test-threads=1
# Photos and Spotlight: read-only; needs Photos access and a picture from the past year.
cargo test -p tsetsen-macos --test live -- --ignored
```

Live tests never touch your documents. Spotlight is tested against `/System/Applications`, and
pictures are only read.

## End to end, without a microphone

`talk` reads speech from a file and writes the reply to one. With the outbox mailer and a
contacts file, a whole errand runs on real models without sending anything:

```sh
tsetsen talk --input conversation.wav --output reply.wav --turns 5 \
  --mail outbox --outbox ./outbox --contacts contacts.json
```

## Evals

```sh
cargo run --release -p tsetsen-evals --bin rewrite     # [model ...], default qwen3.5:9b-mlx
cargo run --release -p tsetsen-evals --bin dictation   # [model]
```

`evals/rewrite.json` holds the cleanup cases: self-corrections, injections, numbers, vocabulary
and Mongolian. `rewrite` feeds text straight to the real `DictationEngine` and guard, measuring
cleanup quality alone. `dictation` speaks each case with several macOS voices and runs real
Parakeet speech-to-text under the product's latency budget.

Each result is one of:
- **PASS:** cleaned correctly.
- **SAFE:** cleanup was rejected, failed or timed out, but the raw text inserted instead is
  still correct.
- **FAIL**
- **SKIP:** no provider for that language.

Each result also reports latency. Keep the prompt examples disjoint from these cases.

> Hybrid thinking models (Qwen3.5) need `reasoning_effort = "none"`. Otherwise they spend the
> whole token budget reasoning and return no text.
