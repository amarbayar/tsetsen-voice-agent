# Architecture

## Ports and adapters

`tsetsen-core` defines domain types and one port trait per capability: `SpeechToText`,
`Rewriter`, `Thinker`, `TextToSpeech`, `Playback`, `VoiceActivity`, `Decider`, `Desktop`, and
the errand services (`Contacts`, `PhotoLibrary`, `FileSearch`, `Mailer`, …). It depends on no
OS, runtime or vendor. Everything else plugs in, and concrete adapters are wired in one place
only: the composition root (today `tsetsen-cli`).

Each layer routes through an ordered list of providers, which can be any model running
anywhere. Every request is filtered by language support and by an **egress policy** keyed on
data sensitivity. Local providers may receive anything. Remote providers receive nothing until
granted, and `Secret` data never leaves the device. Pipelines degrade instead of failing: slow
or bad cleanup falls back to the raw transcript. Text whose target window disappeared is held,
never typed into whatever happens to be focused.

| Crate | Role |
|---|---|
| `tsetsen-core` | Domain types, IDs, request context (deadline, cancellation), port traits. No I/O. |
| `tsetsen-policy` | Egress policy (sensitivity × locality), which actions need the user's OK, and reading their yes or no. Pure, property-tested. |
| `tsetsen-router` | Per-layer provider selection: route order, language and egress filtering, explained exclusions. |
| `tsetsen-engine` | Pipelines built only from ports: dictation, the conversation (clause streaming, turn detection, barge-in), the front desk and its workers, and the errand tools. |
| `tsetsen-testkit` | Scriptable fakes for every port. |
| `tsetsen-llm-openai` | `Rewriter` and streaming `Thinker` over any OpenAI-compatible endpoint (Ollama, LM Studio, MLX, vLLM, llama.cpp, OpenRouter). Locality derived from the URL; no redirects; plain HTTP only to loopback. |
| `tsetsen-onnx` | The one ONNX Runtime environment per process, and session loading. |
| `tsetsen-stt-parakeet` | `SpeechToText`: NVIDIA Parakeet TDT 0.6B v3 (25 European languages). |
| `tsetsen-tts-supertonic` | `TextToSpeech`: Supertonic 3 (31 languages, 10 voices, 44.1 kHz). |
| `tsetsen-vad-silero` | `VoiceActivity`: Silero VAD v6, 32 ms frames. |
| `tsetsen-audio` | Microphone and speaker (cpal) with echo cancellation (pure-Rust WebRTC AEC3), resampled to 16 kHz. |
| `tsetsen-agent-desktop` | `Desktop`: reads and operates apps through the accessibility tree via the [agent-desktop](https://github.com/vercel-labs/agent-desktop) CLI. |
| `tsetsen-macos` | `PhotoLibrary` and `Showcase` through a small Swift helper (read-only export, a large numbered picture window); `FileSearch` through Spotlight. |
| `tsetsen-files` | Portable `FileSearch` (a bounded walk of Documents, Desktop and Downloads) and `Launcher` (the OS's own opener). |
| `tsetsen-google` | `Mailer` (Gmail API) and `Contacts` (People API), with browser sign-in (PKCE, loopback) and the refresh token in the OS keychain. `OutboxMailer` writes `.eml` files instead. |
| `tsetsen-cli` | The `tsetsen` binary: `talk`, `say`, `google login`. |
| `tsetsen-evals` | Rewrite and dictation evals against real models. |

## The front desk and its workers

The voice you talk to never does the work itself. It is a **front desk**: it hands each request
to a **worker** as a task, then goes on listening. You can keep talking while something slow
runs, ask for two things at once, and interrupt what is being said without stopping what is
being done.

- **Routing is a forced choice, not free talk.** Each time the user speaks, the front desk must
  call `start_task`, `tell_task`, `stop_task` or `just_talk` (the request carries
  `tool_choice: required`). Only then does it reply in words. Small models otherwise say "I'm
  sending it now" and hand nothing on. As a backstop, a spoken promise of work is held back
  until the step ends. It is dropped if work follows, spoken if work is already under way, and
  otherwise kept by code: the user's own words go to the task waiting for an answer, or to a
  new task.
- **Workers** run the errand and desktop tools side by side, each with its own context and
  cancellation (`DeskConfig::max_tasks`). A worker that insists it did something when nothing
  ran is not believed. The front desk hears "it has not been done yet".
- **Patience.** A task that reports within `DeskConfig::patience` (5 s) is answered in the same
  breath. A slower one is announced when the user is quiet.
- **Approvals stay in code.** One question is open at a time, read back word for word, and
  settled by the very next utterance. A clear yes approves. Anything else withdraws the action
  and tells the task what was said instead, so a "yes" meant for something else never approves
  an old request.
- **Any model at either level.** `ConversationEngine::with_worker_thinker` gives workers their
  own router, for example a small fast model up front and a stronger one behind it.

## Errands: general verbs over kinds of things

The errand tools work through each service's own interface, not by driving apps on screen. That
makes them reliable whatever is open. The model never types an email address: it can only use
people it has looked up.

| Verb | Takes | Needs |
|---|---|---|
| `find_person`, `remember_person` | a name, or "my son" | `Contacts` or `PeopleMemory` |
| `find_pictures` | when, in the user's words | `PhotoLibrary` |
| `find_files` | what it's about, and roughly when | `FileSearch` |
| `show` | pictures and documents | `Showcase` |
| `open` | any file or picture (never a program) | `Launcher` |
| `share` | people, a message, anything found | `Mailer` and a way to find people |

Everything found goes into one working set under a short id (`c1` a person, `p3` a picture,
`f2` a file), and every verb takes any id it understands. A new kind of thing works with every
verb that accepts it, and a new verb works with every kind. Dates ("last Saturday", "Friday,
July 31") are resolved in code, not by the model. A phrase that can mean two days is checked
against the user's data before asking.

The verbs never touch the OS. Each OS fills a `Capabilities` struct with its adapters
(`crates/tsetsen-cli/src/platform.rs`), and a verb is offered only when what it needs is
present.

## Safety

- **Commitments wait for a spoken yes.** Sending, deleting, buying, pressing Return, clicking
  Send or an unnamed button, and typing a newline are all held. The answer is read from the
  user's own words by fixed rules, never by a model, so text on a web page cannot approve
  anything. "Yes, but sign it Grandma" is not a yes.
- **Screen, tool and email text is data, not instructions.** It reaches the model marked as
  such and is labeled `ScreenContent`, so the egress policy keeps it on the device. Password
  fields are never read.
- **Every action names its window.** Element ids are never reused, so a stale id fails instead
  of hitting another button.
- **Narrow permissions.** Google sign-in asks for sending mail and reading contacts only, never
  for reading mail. Photos are read-only, and screenshots are left out. `open` never launches
  programs or scripts.
- **No content in logs.** Logs carry ids, kinds and timings, never audio, transcripts, typed
  text or screen content.
- **Secrets live in the OS keychain**, never in config files, environment variables or
  arguments. Local state (`~/.tsetsen/memory`, the outbox) is readable by the user only.

## Latency

Each turn prints where the time went. On an M5 Max with `--pause-ms 650`, a plain spoken reply
starts 0.55–0.85 s after you stop talking. Of that, 650 ms is the silence that decides you're
done, about 90 ms is transcription and about 125 ms is the thinker's first token. Supertonic
renders a clause in about 0.23 s (`cargo run --release -p tsetsen-tts-supertonic --example
bench`).

With the front desk, the routing step comes first. In end-to-end runs with qwen3.5 9B, the
first word (usually the acknowledgement) arrived 0.7–1.6 s after the turn ended. A task that
needed a worker step took 2–3 s to report.
