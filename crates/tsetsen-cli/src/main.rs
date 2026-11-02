//! `tsetsen`: talk to your computer. Everything runs locally by default.
//!
//! This is the composition root: the only place that knows which adapters
//! exist. Everything else depends on ports.

mod file_io;
mod memory;
mod platform;
mod terminal;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tsetsen_agent_desktop::{AgentDesktop, AgentDesktopConfig};
use tsetsen_audio::{Audio, AudioConfig, PIPELINE_RATE_HZ};
use tsetsen_core::ports::{
    AudioClip, Contact, Contacts, Desktop, Mailer, Playback, SpeechRequest, SpeechToText, SttHints,
    TextToSpeech, Thinker,
};
use tsetsen_core::{Cancellation, Ctx, Describe, Layer, RequestId, ServiceError};
use tsetsen_engine::{
    Capabilities, Clock, ConversationConfig, ConversationEngine, DeskConfig, EndpointConfig,
    SessionConfig, ToolSet, catalog, run_session,
};
use tsetsen_google::{
    Auth, CredentialStore, Endpoints, GmailMailer, GoogleContacts, Keychain, OAuthClient,
    OutboxMailer, sign_in,
};
use tsetsen_llm_openai::{OpenAiConfig, OpenAiThinker};
use tsetsen_policy::{ConfirmPolicy, EgressPolicy};
use tsetsen_router::{Route, Router};
use tsetsen_stt_parakeet::{ParakeetConfig, ParakeetStt};
use tsetsen_tts_supertonic::{SupertonicConfig, SupertonicTts};
use tsetsen_vad_silero::SileroVad;

use crate::terminal::Terminal;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(name = "tsetsen", version, about = "Talk to your computer, locally.")]
struct Cli {
    /// Where models live.
    #[arg(long, global = true, env = "TSETSEN_MODELS_DIR", default_value_os_t = default_models_dir())]
    models: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Have a spoken conversation. Interrupt any time by talking.
    Talk(Box<TalkArgs>),
    /// Connect a Google account, for sending email and finding contacts.
    #[command(subcommand)]
    Google(GoogleCommand),
    /// Speak text aloud, to check the voice.
    Say {
        /// What to say.
        text: String,
        /// Voice: F1-F5 or M1-M5.
        #[arg(long, default_value = "F1")]
        voice: String,
    },
}

#[derive(Subcommand)]
enum GoogleCommand {
    /// Sign in through the browser. Needs an OAuth client of the "Desktop
    /// app" type from the Google Cloud console, with the Gmail and People
    /// APIs enabled.
    Login {
        /// The client JSON file the console downloads. Its contents go into
        /// the keychain; delete the file afterwards.
        #[arg(long)]
        client_file: PathBuf,
    },
    /// Forget the sign-in.
    Logout,
    /// Whether an account is connected.
    Status,
}

/// How email leaves the computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MailMode {
    /// Send through Gmail, as the signed-in account.
    Gmail,
    /// Write each message to the outbox folder instead of sending it.
    Outbox,
    /// No email.
    Off,
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line switches
struct TalkArgs {
    /// OpenAI-compatible endpoint for the thinker.
    #[arg(
        long,
        env = "TSETSEN_THINK_URL",
        default_value = "http://localhost:11434/v1"
    )]
    think_url: String,
    /// Thinker model.
    #[arg(long, env = "TSETSEN_THINK_MODEL", default_value = "qwen3.5:9b-mlx")]
    model: String,
    /// Voice: F1-F5 or M1-M5.
    #[arg(long, default_value = "F1")]
    voice: String,
    /// Speaking rate: a little slower than usual is easier to follow.
    #[arg(long, default_value_t = 0.95)]
    speed: f32,
    /// Silence, in milliseconds, before deciding the user has finished.
    /// Longer suits people who pause mid-sentence.
    #[arg(long, default_value_t = 1000)]
    pause_ms: u64,
    /// TTS denoising steps: fewer is faster.
    #[arg(long, default_value_t = 6)]
    steps: usize,
    /// Turn off echo cancellation (only with headphones).
    #[arg(long)]
    no_echo_cancellation: bool,
    /// Read the microphone from a WAV file instead (one question), for testing.
    #[arg(long, requires = "output")]
    input: Option<PathBuf>,
    /// With --input: write what the speaker played to this WAV file.
    #[arg(long, requires = "input")]
    output: Option<PathBuf>,
    /// With --input: stop after this many turns (interrupted ones count).
    #[arg(long, default_value_t = 1, requires = "input")]
    turns: usize,
    /// The agent-desktop executable, for operating apps.
    #[arg(long, env = "TSETSEN_AGENT_DESKTOP", default_value_os_t = default_agent_desktop())]
    agent_desktop: PathBuf,
    /// Talk only: never operate apps.
    #[arg(long)]
    no_desktop: bool,
    /// Ask before every change, not only before sending, deleting and the like.
    #[arg(long)]
    confirm_everything: bool,
    /// How email is sent.
    #[arg(long, value_enum, default_value_t = MailMode::Gmail)]
    mail: MailMode,
    /// With --mail outbox: where messages are written.
    #[arg(long, default_value_os_t = tsetsen_dir().join("outbox"))]
    outbox: PathBuf,
    /// An address book to use instead of Google Contacts: a JSON list of
    /// {"name", "email"}. For trying things out.
    #[arg(long)]
    contacts: Option<PathBuf>,
    /// Never look at the photo library.
    #[arg(long)]
    no_photos: bool,
    /// Never look for the user's files.
    #[arg(long)]
    no_files: bool,
    /// The photos helper, if not the one built with tsetsen (macOS).
    #[arg(long, env = "TSETSEN_PHOTOS_HELPER")]
    photos_helper: Option<PathBuf>,
}

fn tsetsen_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".tsetsen")
}

fn note(text: &str) {
    eprintln!("\x1b[2m  {text}\x1b[0m");
}

fn warn(text: &str) {
    eprintln!("\x1b[33m  {text}\x1b[0m");
}

/// An address book from a JSON file, for trying things out.
struct ListedContacts(Vec<Contact>);

#[async_trait::async_trait]
impl Contacts for ListedContacts {
    async fn search(&self, query: &str, _cx: &Ctx) -> Result<Vec<Contact>, ServiceError> {
        let query = query.to_lowercase();
        Ok(self
            .0
            .iter()
            .filter(|c| {
                query.split_whitespace().all(|w| {
                    c.name.to_lowercase().contains(w) || c.emails.iter().any(|e| e.contains(w))
                })
            })
            .cloned()
            .collect())
    }
}

fn listed_contacts(path: &Path) -> Result<ListedContacts, BoxError> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        email: String,
    }
    let entries: Vec<Entry> = serde_json::from_slice(&std::fs::read(path)?)?;
    Ok(ListedContacts(
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| Contact {
                id: format!("listed/{i}"),
                name: e.name,
                emails: vec![e.email],
            })
            .collect(),
    ))
}

/// Every tool this computer supports: the services each OS offers, the
/// user's accounts, and operating apps as the fallback.
async fn connect_tools(args: &TalkArgs, clock: Clock) -> Result<Option<ToolSet>, BoxError> {
    let local = platform::connect(&platform::Choices {
        photos: !args.no_photos,
        files: !args.no_files,
        photos_helper: args.photos_helper.clone(),
    })
    .await;
    let Accounts { contacts, mail } = connect_accounts(args)?;
    let caps = Capabilities {
        contacts,
        memory: Some(Arc::new(memory::FileMemory::new(
            tsetsen_dir().join("memory/people.json"),
        ))),
        photos: local.photos,
        showcase: local.showcase,
        files: local.files,
        launcher: local.launcher,
        mail,
        desktop: connect_desktop(args).await,
    };
    Ok(catalog(caps, clock)?)
}

/// The services that come from the user's accounts, the same on every OS.
struct Accounts {
    contacts: Option<Arc<dyn Contacts>>,
    mail: Option<Arc<dyn Mailer>>,
}

/// The address book and email.
fn connect_accounts(args: &TalkArgs) -> Result<Accounts, BoxError> {
    let keychain: Arc<dyn CredentialStore> = Arc::new(Keychain::google());
    let google = Arc::new(Auth::google(keychain)?);
    let signed_in = google.signed_in().unwrap_or(false);

    let contacts: Option<Arc<dyn Contacts>> = if let Some(path) = &args.contacts {
        note(&format!("contacts from {}", path.display()));
        Some(Arc::new(listed_contacts(path)?))
    } else if signed_in {
        note("contacts: Google");
        Some(Arc::new(GoogleContacts::new(Arc::clone(&google))))
    } else {
        None
    };

    let mail: Option<Arc<dyn Mailer>> = match args.mail {
        MailMode::Gmail if signed_in => {
            note("email: Gmail");
            Some(Arc::new(GmailMailer::new(Arc::clone(&google))))
        }
        MailMode::Gmail => {
            warn("email is off: no Google account connected (run `tsetsen google login`)");
            None
        }
        MailMode::Outbox => {
            note(&format!(
                "email goes to {} (not sent)",
                args.outbox.display()
            ));
            Some(Arc::new(OutboxMailer::new(&args.outbox)))
        }
        MailMode::Off => None,
    };
    Ok(Accounts { contacts, mail })
}

async fn google(command: GoogleCommand) -> Result<(), BoxError> {
    let keychain = Keychain::google();
    match command {
        GoogleCommand::Login { client_file } => {
            let client = OAuthClient::from_console_json(&std::fs::read_to_string(&client_file)?)?;
            println!("Opening your browser to sign in to Google…");
            sign_in(client, &keychain, &Endpoints::default(), |url| {
                if std::process::Command::new("open").arg(url).spawn().is_err() {
                    println!("Open this address in your browser:\n{url}");
                }
            })
            .await?;
            println!(
                "Signed in. Tsetsen can now send email and find contacts.\n\
                 The client details are in the keychain: you can delete {}.",
                client_file.display()
            );
        }
        GoogleCommand::Logout => {
            keychain.clear()?;
            println!("Signed out.");
        }
        GoogleCommand::Status => println!(
            "{}",
            if keychain.load()?.is_some() {
                "A Google account is connected."
            } else {
                "No Google account is connected."
            }
        ),
    }
    Ok(())
}

fn default_agent_desktop() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".tsetsen/tools/agent-desktop-1.0.2/agent-desktop")
}

/// Desktop control if agent-desktop is installed and allowed; otherwise
/// talk only, with the reason.
async fn connect_desktop(args: &TalkArgs) -> Option<Arc<dyn Desktop>> {
    if args.no_desktop {
        return None;
    }
    let started = Instant::now();
    match AgentDesktop::connect(AgentDesktopConfig::new(&args.agent_desktop)).await {
        Ok(desktop) => {
            eprintln!(
                "\x1b[2m  desktop control (agent-desktop) ready in {} ms\x1b[0m",
                started.elapsed().as_millis()
            );
            Some(Arc::new(desktop))
        }
        Err(error) => {
            eprintln!("\x1b[33m  desktop control is off: {error}\x1b[0m");
            None
        }
    }
}

fn default_models_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".tsetsen/models")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,ort=error".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Talk(args) => talk(&cli.models, *args).await,
        Command::Say { text, voice } => say(&cli.models, &text, &voice).await,
        Command::Google(command) => google(command).await,
    };
    if let Err(error) = result {
        eprintln!("\x1b[31merror:\x1b[0m {error}");
        std::process::exit(1);
    }
}

fn local_router<P: ?Sized + Describe>(
    layer: Layer,
    provider: Arc<P>,
) -> Result<Router<P>, BoxError> {
    let route = Route::new(provider.descriptor().id.clone());
    // Local only: nothing leaves the device unless a remote grant is configured.
    Ok(Router::new(
        layer,
        &route,
        [provider],
        Arc::new(EgressPolicy::new()),
    )?)
}

fn load_talker(
    models: &Path,
    voice: &str,
    steps: usize,
    speed: f32,
) -> Result<SupertonicTts, BoxError> {
    let config = SupertonicConfig::new("supertonic", models.join("supertonic-3"))
        .with_voice(voice)
        .with_steps(steps)
        .with_speed(speed);
    Ok(SupertonicTts::load(&config)?)
}

fn timed<T>(what: &str, load: impl FnOnce() -> Result<T, BoxError>) -> Result<T, BoxError> {
    let started = Instant::now();
    let value = load().map_err(|e| format!("loading {what}: {e}"))?;
    eprintln!(
        "\x1b[2m  {what} ready in {} ms\x1b[0m",
        started.elapsed().as_millis()
    );
    Ok(value)
}

async fn say(models: &Path, text: &str, voice: &str) -> Result<(), BoxError> {
    let talker = timed("voice", || load_talker(models, voice, 8, 1.05))?;
    let (audio, _mic) = Audio::open(AudioConfig::default())?;
    let cx = Ctx::new(RequestId::new(1));
    let request = SpeechRequest {
        text: text.into(),
        ..SpeechRequest::default()
    };
    let clip = talker.synthesize(&request, &cx).await?;
    audio.speaker().play(&clip, &cx).await?;
    Ok(())
}

/// English for now, and asking before commitments unless told otherwise.
fn conversation_config(args: &TalkArgs) -> ConversationConfig {
    ConversationConfig {
        language: Some("en".into()),
        desk: DeskConfig {
            confirm: if args.confirm_everything {
                ConfirmPolicy::AllChanges
            } else {
                ConfirmPolicy::Commits
            },
            ..DeskConfig::default()
        },
        ..ConversationConfig::default()
    }
}

async fn talk(models: &Path, args: TalkArgs) -> Result<(), BoxError> {
    eprintln!("\x1b[2mloading models…\x1b[0m");
    let stt = timed("speech recognition (Parakeet)", || {
        let config = ParakeetConfig::new("parakeet", models.join("parakeet-tdt-0.6b-v3"))
            .with_languages(["en".into()]);
        Ok(Arc::new(ParakeetStt::load(&config)?))
    })?;
    let talker = timed("voice (Supertonic)", || {
        Ok(Arc::new(load_talker(
            models,
            &args.voice,
            args.steps,
            args.speed,
        )?))
    })?;
    let mut vad = timed("voice activity (Silero)", || {
        Ok(SileroVad::load(&models.join("silero-vad/silero_vad.onnx"))?)
    })?;
    let thinker = Arc::new(OpenAiThinker::new(
        &OpenAiConfig::new("thinker", &args.think_url, &args.model)?.with_reasoning_effort("none"),
    )?);
    let clock: Clock = Arc::new(jiff::Zoned::now);
    let tools = connect_tools(&args, Arc::clone(&clock)).await?;
    warm_up(stt.as_ref(), talker.as_ref()).await?;

    let stop = Cancellation::new();
    let terminal = Terminal::default();

    let (playback, frames, recorder, live_audio) = open_audio(&args)?;

    let mut engine = ConversationEngine::new(
        local_router(Layer::Stt, stt as Arc<dyn SpeechToText>)?,
        local_router(Layer::Think, thinker as Arc<dyn Thinker>)?,
        local_router(Layer::Speak, talker as Arc<dyn TextToSpeech>)?,
        playback,
    )
    .with_config(conversation_config(&args));
    if let Some(tools) = tools {
        engine = engine.with_tools(Arc::new(tools));
    }
    engine = engine.with_clock(clock);
    let started = Instant::now();
    let cx = Ctx::new(RequestId::new(0)).with_timeout(Duration::from_secs(120));
    engine.warm_up(&cx).await.map_err(|e| {
        format!(
            "thinker `{}` is not reachable ({e}). Is Ollama running, with the model pulled?",
            args.model
        )
    })?;
    eprintln!(
        "\x1b[2m  thinker warmed up in {} ms\x1b[0m",
        started.elapsed().as_millis()
    );

    if recorder.is_none() {
        eprintln!(
            "\n\x1b[1mListening.\x1b[0m Talk any time, interrupt any time. Ctrl-C to quit.\n"
        );
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.cancel();
        });
    }

    let file_mode = recorder.is_some();
    let mut turns_left = args.turns;
    run_session(
        &engine,
        &mut vad,
        frames,
        &terminal,
        SessionConfig {
            listening: EndpointConfig {
                end_silence: Duration::from_millis(args.pause_ms),
                ..EndpointConfig::listening()
            },
            ..SessionConfig::default()
        },
        &stop,
        |result| {
            terminal.turn(result);
            turns_left = turns_left.saturating_sub(1);
            if file_mode && turns_left == 0 {
                stop.cancel();
            }
        },
    )
    .await?;

    drop(live_audio);
    if let (Some(recorder), Some(output)) = (recorder, &args.output) {
        recorder.save(output)?;
        eprintln!("\x1b[2m  reply saved to {}\x1b[0m", output.display());
    }
    Ok(())
}

/// The speaker, the microphone frames and, in file mode, the recorder; or
/// the live devices.
type AudioSetup = (
    Arc<dyn Playback>,
    tokio::sync::mpsc::Receiver<Vec<f32>>,
    Option<Arc<file_io::Recorder>>,
    Option<Audio>,
);

fn open_audio(args: &TalkArgs) -> Result<AudioSetup, BoxError> {
    Ok(if let Some(input) = &args.input {
        let recorder = file_io::Recorder::new(24_000);
        let samples = file_io::read_wav(input, PIPELINE_RATE_HZ)?;
        (
            Arc::clone(&recorder) as Arc<dyn Playback>,
            file_io::microphone(samples),
            Some(recorder),
            None,
        )
    } else {
        let config = AudioConfig {
            echo_cancellation: !args.no_echo_cancellation,
            ..AudioConfig::default()
        };
        let (audio, frames) = Audio::open(config)?;
        let devices = audio.devices();
        eprintln!(
            "\x1b[2m  mic: {} ({} Hz) · speaker: {} ({} Hz) · echo cancellation {}\x1b[0m",
            devices.input,
            devices.input_rate_hz,
            devices.output,
            devices.output_rate_hz,
            if config.echo_cancellation {
                "on"
            } else {
                "off"
            }
        );
        (audio.speaker(), frames, None, Some(audio))
    })
}

/// Runs speech recognition and the voice once, so the first real turn is
/// as fast as every other.
async fn warm_up(stt: &dyn SpeechToText, talker: &dyn TextToSpeech) -> Result<(), BoxError> {
    let started = Instant::now();
    let cx = Ctx::new(RequestId::new(0)).with_timeout(Duration::from_secs(120));
    let silence = AudioClip::new(PIPELINE_RATE_HZ, vec![0.0; PIPELINE_RATE_HZ as usize])?;
    stt.transcribe(&silence, &SttHints::default(), &cx).await?;
    talker
        .synthesize(
            &SpeechRequest {
                text: "Ready.".into(),
                ..SpeechRequest::default()
            },
            &cx,
        )
        .await?;
    eprintln!(
        "\x1b[2m  warmed up in {} ms\x1b[0m",
        started.elapsed().as_millis()
    );
    Ok(())
}
