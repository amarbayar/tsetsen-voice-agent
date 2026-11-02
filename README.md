# Tsetsen

**A voice assistant for your computer that runs on your computer.** Say what you want in your
own words. Tsetsen finds the people, pictures and files you mean, operates your apps, and asks
before anything that can't be undone.

Tsetsen (цэцэн, "wise, eloquent") is built for people who shouldn't have to know which app does
what. "I need to email my son a picture of our trip from last Saturday" works as it is said.

> **Status: pre-alpha.** Runs today on macOS (Apple silicon), fully on-device. The core is
> cross-platform, and Windows and Linux adapters come next. Expect breaking changes.

## What it does

- **Talks like a person.** It streams its replies, and you can interrupt at any time; it
  remembers only what you actually heard. Echo cancellation is on, so speakers work.
- **Does errands, not commands.** Find someone ("my son"), find pictures ("end of July"), find
  files ("the letter from the bank"), show them large on screen, and email them. Dates and
  names are resolved in code, not guessed by a model.
- **Operates any app** through the accessibility tree: open, read, type, press, click.
- **Keeps working while you talk.** Each request becomes a background task, so you can ask for
  two things at once or interrupt without stopping the work.
- **Never acts behind your back.** Sending, deleting, buying and submitting wait for a clear
  spoken yes, and the exact action is read back word for word first. A model can't approve
  anything; your own words decide.

## Private by design

Speech recognition, the voice and the language model all run locally by default. Each layer can
point to any model, local or remote. An egress policy decides what may leave the device, and
screen content and secrets never do. Logs never contain audio, transcripts or screen text.
Credentials live in the OS keychain. Gmail access is send-only plus contacts; it never reads
your mail.

## Quick start

You need macOS on Apple silicon, [Rust](https://rustup.rs), [Ollama](https://ollama.com), and
about 12 GB of disk for the models.

```sh
ollama pull qwen3.5:9b-mlx           # the language model, ~9 GB
scripts/fetch-models.sh              # speech-to-text, voice, voice activity: ~3 GB
scripts/install-agent-desktop.sh     # optional: lets it operate apps
cargo run --release -p tsetsen-cli -- talk
```

The binary is then at `target/release/tsetsen` (called `tsetsen` below). macOS asks for
microphone access the first time. To operate apps, also allow your terminal
under **System Settings → Privacy & Security → Accessibility**.

Try saying:

- "Open TextEdit and write a haiku about tea."
- "Show me my pictures from last weekend."
- "Find the letter from the bank."

To email without sending anything for real, messages can go to a folder as `.eml` files:

```sh
tsetsen talk --mail outbox --contacts contacts.json   # [{"name": "...", "email": "..."}]
```

To send through Gmail, create an OAuth client ("Desktop app") with the Gmail and People APIs
enabled, then run `tsetsen google login --client-file client_secret.json`.

Useful options: `--voice F1`–`F5` / `M1`–`M5`, `--speed 0.95`, `--pause-ms 1000` for a more
patient pace, `--model` and `--think-url` for any OpenAI-compatible server, and `--input` /
`--output` to run from audio files. See `tsetsen talk --help`.

## How it's built

Rust, using ports and adapters. A core with no OS or vendor dependencies defines one trait per
capability: speech-to-text, thinker, voice, desktop, contacts, photos, files and mail. Each OS
and model plugs in as an adapter. Every layer routes through an ordered list of providers, with
language and privacy filtering.

| Layer | Default (on-device) |
|---|---|
| Speech-to-text | NVIDIA Parakeet TDT 0.6B v3 |
| Thinker | Qwen 3.5 9B via Ollama (any OpenAI-compatible endpoint) |
| Voice | Supertonic 3 |
| Voice activity | Silero VAD |
| Apps | [agent-desktop](https://github.com/vercel-labs/agent-desktop) |

More detail:
- [docs/architecture.md](docs/architecture.md): the crates, the front desk and workers, the
  errand verbs, the safety model and latency.
- [docs/testing.md](docs/testing.md): tests, live-model tests and evals.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml`. Tests that need real models are `#[ignore]`d
and run locally ([docs/testing.md](docs/testing.md)).

## Roadmap

- Hotkey dictation into any app, with real cleanup ("3, no wait, 4pm" becomes "4pm").
- A fast path for simple commands ("stop", "louder", "go back"), and tasks that survive a
  restart.
- Voice-driven web browsing; reading mail, messages and reminders aloud.
- Names heard right (vocabulary-biased recognition), and models for Mongolian.
- Scam guards and a trusted family contact.
- Windows, then Linux.

## License

[Apache-2.0](LICENSE). Model weights are downloaded separately and carry their own licenses:
Parakeet (CC-BY-4.0), Supertonic 3 (OpenRAIL-M) and Silero VAD (MIT).
