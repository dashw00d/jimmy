mod local;
mod models;
mod server;
mod talk;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use jimmy_core::{Recognition, Settings, Speech, Tts};
use jimmy_proto::{INPUT_RATE, Mode, OUTPUT_RATE};
use models::Models;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Cursor, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum ListenMode {
    Wake,
    Ptt,
    Open,
}

impl From<ListenMode> for Mode {
    fn from(mode: ListenMode) -> Self {
        match mode {
            ListenMode::Wake => Self::Wake,
            ListenMode::Ptt => Self::Ptt,
            ListenMode::Open => Self::Open,
        }
    }
}

#[derive(Clone, Default, Deserialize, clap::Args)]
#[serde(default, deny_unknown_fields)]
struct Options {
    #[arg(long, global = true, env = "JIMMY_MODEL_DIR")]
    model_dir: Option<PathBuf>,
    #[arg(long, global = true, env = "JIMMY_MODE")]
    mode: Option<ListenMode>,
    #[arg(long, global = true, env = "JIMMY_WAKE")]
    wake: Option<String>,
    /// Speak a short reply after a standalone wake phrase.
    #[arg(long, global = true, env = "JIMMY_WAKE_REPLY", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    wake_reply: Option<bool>,
    #[arg(long, global = true, env = "JIMMY_VOICE")]
    voice: Option<String>,
    #[arg(long, global = true, env = "JIMMY_VOICE_FILE")]
    voice_file: Option<PathBuf>,
    #[arg(long, global = true, env = "JIMMY_RATE")]
    rate: Option<f32>,
    #[arg(long, global = true, env = "JIMMY_LANGUAGE")]
    language: Option<String>,
    #[arg(
        long = "keyword",
        global = true,
        env = "JIMMY_KEYWORDS",
        value_delimiter = ','
    )]
    keywords: Option<Vec<String>>,
    #[arg(long, global = true, env = "JIMMY_ENDPOINT_MS")]
    endpoint_ms: Option<u32>,
    #[arg(long, global = true, env = "JIMMY_FOLLOWUP_MS")]
    followup_ms: Option<u32>,
    #[arg(long, global = true, env = "JIMMY_PARTIAL_MS")]
    partial_ms: Option<u32>,
    #[arg(long, global = true, env = "JIMMY_CUE_DIR")]
    cue_dir: Option<PathBuf>,
    #[arg(long, global = true, env = "JIMMY_BIND")]
    bind: Option<SocketAddr>,
    #[arg(long, global = true, env = "JIMMY_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, global = true, env = "JIMMY_ALLOW_ORIGIN", value_delimiter = ',')]
    allow_origin: Option<Vec<String>>,
    #[arg(long, global = true, env = "JIMMY_JSON", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    json: Option<bool>,
    /// `jimmy talk` agent, set in jimmy.toml.
    #[arg(skip)]
    agent: Option<String>,
    /// `jimmy talk --model`, set in jimmy.toml.
    #[arg(skip)]
    agent_model: Option<String>,
}

impl Options {
    fn layer(self, file: Self) -> Self {
        macro_rules! inherit {
            ($($field:ident),* $(,)?) => {
                Self { $($field: self.$field.or(file.$field)),* }
            }
        }
        inherit!(
            model_dir,
            mode,
            wake,
            wake_reply,
            voice,
            voice_file,
            rate,
            language,
            keywords,
            endpoint_ms,
            followup_ms,
            partial_ms,
            cue_dir,
            bind,
            token,
            allow_origin,
            json,
            agent,
            agent_model
        )
    }

    fn settings(&self) -> Result<Settings> {
        let settings = Settings {
            mode: self.mode.map_or(Mode::Wake, Into::into),
            wake: self.wake.clone().unwrap_or_else(|| "hey jimmy".into()),
            wake_reply: self.wake_reply.unwrap_or(true),
            endpoint_ms: self.endpoint_ms.unwrap_or(600),
            followup_ms: self.followup_ms.unwrap_or(8_000),
            partial_ms: self.partial_ms.unwrap_or(300),
            voice: self.voice.clone(),
            rate: self.rate.unwrap_or(1.0),
            recognition: Recognition {
                language: self
                    .language
                    .clone()
                    .filter(|lang| lang != "auto" && !lang.is_empty()),
                keywords: self.keywords.clone().unwrap_or_default(),
            },
            ..Settings::default()
        };
        ensure!(
            (0.5..=2.0).contains(&settings.rate),
            "--rate must be between 0.5 and 2"
        );
        ensure!(
            (100..=5_000).contains(&settings.endpoint_ms),
            "--endpoint-ms must be between 100 and 5000"
        );
        ensure!(
            settings.followup_ms <= 60_000,
            "--followup-ms must be at most 60000"
        );
        ensure!(
            (100..=2_000).contains(&settings.partial_ms),
            "--partial-ms must be between 100 and 2000"
        );
        ensure!(
            settings.wake.len() <= 128 && settings.wake.chars().any(char::is_alphanumeric),
            "Invalid wake phrase"
        );
        Ok(settings)
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Local speech recognition, synthesis, and wake detection",
    after_help = "Run `jimmy` with no command to talk to the first agent it finds (same as `jimmy talk`)."
)]
struct Cli {
    #[command(flatten)]
    options: Options,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Talk to an agent: Jimmy hears you, sends your words to it, and speaks its reply.
    #[command(after_help = "\
Examples:
  jimmy talk                      Use Codex, Claude Code, or Ollama, whichever is installed
  jimmy talk codex                Codex CLI (gpt-6.1-terra unless --model is set)
  jimmy talk claude               Claude Code CLI (haiku unless --model is set)
  jimmy talk ollama               Local Ollama server, first model unless --model is set
  jimmy talk http://localhost:1234/v1 --model NAME
                                  Any OpenAI-compatible chat completions server
  jimmy talk \"python3 bot.py\"     Any shell command
  jimmy talk -- my-agent --flag   Any program, run without a shell

A command gets your words on stdin, with recent turns for context once there are some, and
JIMMY_TEXT (just your latest words), JIMMY_HISTORY (JSON), and JIMMY_TURN in its environment.
Whatever it prints is spoken as it arrives.")]
    Talk {
        /// codex, claude, ollama, an http(s) URL, or a shell command.
        #[arg(env = "JIMMY_AGENT")]
        agent: Option<String>,
        /// Model for codex, claude, ollama, or a URL.
        #[arg(long, env = "JIMMY_AGENT_MODEL")]
        model: Option<String>,
        /// Bearer token for a URL agent. OPENAI_API_KEY is used for api.openai.com.
        #[arg(long, env = "JIMMY_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// A program and its arguments, after --.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Speak text, or read it from standard input.
    Say {
        text: Option<String>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Transcribe a WAV file. Use "-" for standard input.
    Transcribe { clip: PathBuf },
    /// Capture live audio. In PTT mode press Enter to start or stop.
    Listen,
    /// Exchange JSONL events. Jimmy owns the microphone and speaker.
    Pipe,
    /// Serve WebSocket streaming and HTTP one-shots.
    Serve,
    /// Fetch or verify the pinned checkpoints.
    Pull,
    /// Run a real-model synthesis and transcription self-test.
    Doctor,
    /// List the loaded Phonon checkpoint voices.
    Voices,
    /// List native capture and playback devices.
    Devices,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("jimmy: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let file = match std::fs::read_to_string("jimmy.toml") {
        Ok(contents) => toml::from_str(&contents).context("Parse jimmy.toml")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Options::default(),
        Err(error) => return Err(error).context("Read jimmy.toml"),
    };
    let options = cli.options.layer(file);
    let settings = options.settings()?;
    let models = Models::new(options.model_dir.clone())?;
    let json = options.json.unwrap_or(false);
    let voice_file = options.voice_file.as_deref();
    let speech = Speech {
        voice: settings.voice.clone(),
        rate: settings.rate,
    };
    let command = cli.command.unwrap_or(Command::Talk {
        agent: std::env::var("JIMMY_AGENT").ok(),
        model: std::env::var("JIMMY_AGENT_MODEL").ok(),
        api_key: std::env::var("JIMMY_API_KEY").ok(),
        command: Vec::new(),
    });
    match command {
        Command::Talk {
            agent,
            model,
            api_key,
            command,
        } => {
            let agent = talk::resolve(
                agent.or(options.agent.clone()).as_deref(),
                &command,
                model.or(options.agent_model.clone()).as_deref(),
                api_key,
            )?;
            local::run(
                models.whistle()?,
                Some(models.phonon(voice_file, 1)?),
                settings,
                false,
                false,
                options.cue_dir.as_deref(),
                Some(agent),
            )?;
        }
        Command::Pull => {
            models.pull()?;
            if json {
                print_json(&serde_json::json!({"model_dir": models.root()}))?;
            } else {
                println!("{}", models.root().display());
            }
        }
        Command::Devices => {
            for (direction, name) in jimmy_audio::devices()? {
                if json {
                    print_json(&serde_json::json!({"direction": direction, "name": name}))?;
                } else {
                    println!("{direction}\t{name}");
                }
            }
        }
        Command::Voices => {
            for voice in models.phonon(voice_file, 1)?.voices() {
                if json {
                    print_json(&serde_json::json!({"voice": voice}))?;
                } else {
                    println!("{voice}");
                }
            }
        }
        Command::Say { text, output } => {
            let text = match text {
                Some(text) => text,
                None => {
                    let mut text = String::new();
                    io::stdin().take(65_537).read_to_string(&mut text)?;
                    text
                }
            };
            validate_speech(&text)?;
            let mut tts = models.phonon(voice_file, 1)?;
            if let Some(path) = output {
                let pcm = models::synthesize(&mut tts, &text, &speech)?;
                if path == Path::new("-") {
                    let mut result = Cursor::new(Vec::new());
                    jimmy_audio::write_wav(&mut result, &pcm, OUTPUT_RATE)?;
                    io::stdout().write_all(&result.into_inner())?;
                } else {
                    jimmy_audio::write_wav(File::create(path)?, &pcm, OUTPUT_RATE)?;
                }
            } else {
                local::say(&mut tts, &text, &speech)?;
            }
        }
        Command::Transcribe { clip } => {
            let pcm = if clip == Path::new("-") {
                jimmy_audio::read_wav(io::stdin().lock(), INPUT_RATE)?
            } else {
                jimmy_audio::read_wav(
                    File::open(&clip).with_context(|| format!("Open {}", clip.display()))?,
                    INPUT_RATE,
                )?
            };
            let result = models::transcribe(&mut models.whistle()?, &pcm, &settings.recognition)?;
            if json {
                print_json(&result)?;
            } else {
                println!("{}", result.text);
            }
        }
        Command::Doctor => doctor(&models, voice_file, json)?,
        Command::Listen => {
            let tts = (settings.mode == Mode::Wake && settings.wake_reply)
                .then(|| models.phonon(voice_file, 1))
                .transpose()?;
            if !json {
                eprintln!("jimmy: Transcribing only. To hear answers, run `jimmy talk` instead.");
            }
            local::run(models.whistle()?, tts, settings, false, json, None, None)?;
        }
        Command::Pipe => local::run(
            models.whistle()?,
            Some(models.phonon(voice_file, 1)?),
            settings,
            true,
            true,
            options.cue_dir.as_deref(),
            None,
        )?,
        Command::Serve => {
            let mut settings = settings;
            settings.mode = options.mode.map_or(Mode::Ptt, Into::into);
            settings.opus_supported = cfg!(feature = "opus");
            let bind = options
                .bind
                .unwrap_or_else(|| ([127, 0, 0, 1], 7071).into());
            let security = server::Security::new(
                bind,
                options.token,
                options.allow_origin.unwrap_or_default(),
            )?;
            let cores = num_cpus::get_physical().max(1);
            let engines = server::Engines {
                stt: models.whistle()?,
                tts: models.phonon(voice_file, cores.min(16))?,
            };
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(cores.clamp(1, 4))
                .enable_all()
                .build()?;
            runtime.block_on(server::run(bind, engines, settings, security))?;
        }
    }
    Ok(())
}

fn validate_speech(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("Speech input is empty");
    }
    ensure!(text.len() <= 65_536, "Speech input exceeds 65536 bytes");
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn doctor(models: &Models, voice_file: Option<&Path>, json: bool) -> Result<()> {
    let start = Instant::now();
    let mut stt = models.whistle()?;
    let mut tts = models.phonon(voice_file, 1)?;
    let warm_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let text = "Hello from Jimmy. The speech engines are ready.";
    let start = Instant::now();
    let output = models::synthesize(
        &mut tts,
        text,
        &Speech {
            voice: None,
            rate: 1.0,
        },
    )?;
    let tts_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let start = Instant::now();
    let heard = models::transcribe(
        &mut stt,
        &models::at_input_rate(&output),
        &Recognition::default(),
    )?;
    let stt_ms = start.elapsed().as_secs_f64() * 1_000.0;
    ensure!(
        output.len() > 2_400 && !heard.words.is_empty(),
        "The speech round trip returned no timed words"
    );
    if json {
        print_json(&serde_json::json!({
            "whistle_runtime": jimmy_whistle::VERSION, "warm_ms": warm_ms,
            "tts_ms": tts_ms, "stt_ms": stt_ms, "seconds": output.len() as f64 / f64::from(OUTPUT_RATE),
            "transcript": heard,
        }))?;
    } else {
        println!(
            "Models ready in {warm_ms:.1} ms\nSynthesis {tts_ms:.1} ms, transcription {stt_ms:.1} ms\n{}",
            heard.text
        );
    }
    Ok(())
}
