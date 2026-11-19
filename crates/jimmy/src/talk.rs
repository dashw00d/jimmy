//! Connects Jimmy's ears and voice to any agent: Codex, Claude, Ollama, an
//! OpenAI-compatible URL, or any command that reads a prompt and prints a reply.
use anyhow::{Context, Result, bail};
use jimmy_proto::{ClientEvent, Mode, ServerEvent, Turn};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    env,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

pub const INSTRUCTIONS: &str = "You are Jimmy, a voice assistant. Your reply is spoken aloud, so \
answer naturally in one to three short sentences of plain text, with no Markdown, lists, or code. \
Use the earlier conversation to answer follow-ups.";
const HISTORY: usize = 8;
const FILLER_AFTER: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(120);
const CODEX_MODEL: &str = "gpt-6.1-terra";
const CLAUDE_MODEL: &str = "haiku";
const OLLAMA_URL: &str = "http://localhost:11434/v1";

#[derive(Debug)]
pub enum Backend {
    Command {
        program: PathBuf,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        /// Put the voice instructions in the prompt because the command has no system prompt flag.
        instructions: bool,
    },
    Http {
        url: String,
        model: String,
        key: Option<String>,
    },
}

#[derive(Debug)]
pub struct Agent {
    /// Short spoken and printed name, such as "Codex".
    pub name: String,
    /// What runs, for the startup banner.
    pub detail: String,
    pub backend: Backend,
}

/// Picks an agent from `jimmy talk [AGENT] [-- COMMAND...]`.
pub fn resolve(
    agent: Option<&str>,
    command: &[String],
    model: Option<&str>,
    key: Option<String>,
) -> Result<Agent> {
    if let Some((program, args)) = command.split_first() {
        if agent.is_some() {
            bail!("Pass an agent name or a command after --, not both");
        }
        return Ok(Agent {
            name: "Agent".into(),
            detail: command.join(" "),
            backend: Backend::Command {
                program: find_program(program).unwrap_or_else(|| program.into()),
                args: args.to_vec(),
                cwd: None,
                instructions: false,
            },
        });
    }
    let Some(agent) = agent.map(str::trim).filter(|agent| !agent.is_empty()) else {
        return detect(model);
    };
    match agent.to_ascii_lowercase().as_str() {
        "codex" => codex(model),
        "claude" => claude(model),
        "ollama" => ollama(model),
        _ if agent.starts_with("http://") || agent.starts_with("https://") => {
            http(agent, model, key)
        }
        _ => Ok(Agent {
            name: "Agent".into(),
            detail: agent.into(),
            backend: shell(agent),
        }),
    }
}

fn detect(model: Option<&str>) -> Result<Agent> {
    if find_program("codex").is_some() {
        return codex(model);
    }
    if find_program("claude").is_some() {
        return claude(model);
    }
    if ollama_models(Duration::from_millis(500)).is_ok() {
        return ollama(model);
    }
    bail!(
        "No agent found. Install the Codex or Claude Code CLI, start Ollama, or name one:
  jimmy talk codex
  jimmy talk claude
  jimmy talk ollama
  jimmy talk http://localhost:1234/v1 --model NAME
  jimmy talk \"my-script.sh\"    (gets the prompt on stdin and speaks its output)"
    )
}

fn codex(model: Option<&str>) -> Result<Agent> {
    let program = find_program("codex")
        .context("The Codex CLI is not on PATH. Install it, then run `codex login`.")?;
    let model = model.unwrap_or(CODEX_MODEL);
    let mut args: Vec<String> = [
        "exec",
        "--skip-git-repo-check",
        "--ephemeral",
        "--sandbox",
        "read-only",
        "--model",
        model,
        "-c",
        "model_reasoning_effort=\"medium\"",
        "-c",
        "mcp_servers={}",
        "-c",
        "web_search=\"disabled\"",
    ]
    .map(String::from)
    .into();
    for feature in ["memories", "hooks", "plugins", "shell_tool", "unified_exec"] {
        args.extend(["--disable".into(), feature.into()]);
    }
    args.push("-".into());
    Ok(Agent {
        name: "Codex".into(),
        detail: format!("Codex CLI, {model}"),
        backend: Backend::Command {
            program,
            args,
            cwd: Some(env::temp_dir()),
            instructions: true,
        },
    })
}

fn claude(model: Option<&str>) -> Result<Agent> {
    let program = find_program("claude").context(
        "The Claude Code CLI is not on PATH. Install it, then run `claude` once to sign in.",
    )?;
    let model = model.unwrap_or(CLAUDE_MODEL);
    let args = [
        "--print",
        "--model",
        model,
        "--tools",
        "",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--system-prompt",
        INSTRUCTIONS,
    ]
    .map(String::from)
    .into();
    Ok(Agent {
        name: "Claude".into(),
        detail: format!("Claude Code CLI, {model}"),
        backend: Backend::Command {
            program,
            args,
            cwd: Some(env::temp_dir()),
            instructions: false,
        },
    })
}

fn ollama(model: Option<&str>) -> Result<Agent> {
    let model = match model {
        Some(model) => model.to_owned(),
        None => ollama_models(Duration::from_secs(2))
            .context("Ollama is not running at localhost:11434. Start it with `ollama serve`.")?
            .into_iter()
            .next()
            .context("Ollama has no models. Run `ollama pull llama3.2` first.")?,
    };
    Ok(Agent {
        name: "Ollama".into(),
        detail: format!("Ollama, {model}"),
        backend: Backend::Http {
            url: OLLAMA_URL.into(),
            model,
            key: None,
        },
    })
}

fn ollama_models(timeout: Duration) -> Result<Vec<String>> {
    let tags = get_json("http://localhost:11434/api/tags", None, timeout)?;
    Ok(tags["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["name"].as_str().map(String::from))
        .collect())
}

fn http(url: &str, model: Option<&str>, key: Option<String>) -> Result<Agent> {
    let url = url
        .trim_end_matches('/')
        .trim_end_matches("/chat/completions")
        .to_owned();
    // Only hand the OpenAI key to OpenAI itself.
    let key = key.or_else(|| {
        url.starts_with("https://api.openai.com/")
            .then(|| env::var("OPENAI_API_KEY").ok())
            .flatten()
    });
    let model = match model {
        Some(model) => model.to_owned(),
        None => {
            let models = get_json(
                &format!("{url}/models"),
                key.as_deref(),
                Duration::from_secs(5),
            )
            .with_context(|| {
                format!("List models at {url}/models. Pass --model NAME to skip this")
            })?;
            models["data"][0]["id"]
                .as_str()
                .map(String::from)
                .with_context(|| format!("{url}/models lists no models. Pass --model NAME"))?
        }
    };
    Ok(Agent {
        name: "Agent".into(),
        detail: format!("{url}, {model}"),
        backend: Backend::Http { url, model, key },
    })
}

fn shell(command: &str) -> Backend {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    Backend::Command {
        program: program.into(),
        args: vec![flag.into(), command.into()],
        cwd: None,
        instructions: false,
    }
}

/// Finds an executable on PATH, including Windows `.cmd` and `.exe` shims.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(name);
    if path.components().count() > 1 {
        return path.is_file().then_some(path);
    }
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .map(String::from)
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    env::split_paths(&env::var_os("PATH")?).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}{ext}"));
            is_executable(&candidate).then_some(candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

fn get_json(url: &str, key: Option<&str>, timeout: Duration) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut request = agent.get(url);
    if let Some(key) = key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let mut body = String::new();
    request
        .call()?
        .into_body()
        .into_reader()
        .take(4_000_000)
        .read_to_string(&mut body)?;
    Ok(serde_json::from_str(&body)?)
}

pub struct Exchange {
    pub user: String,
    pub assistant: String,
}

/// The text a command reads on stdin: the latest words, with recent turns for context.
pub fn prompt(history: &VecDeque<Exchange>, text: &str, instructions: bool) -> String {
    if history.is_empty() && !instructions {
        return text.to_owned();
    }
    let mut out = String::new();
    if instructions {
        out.push_str(INSTRUCTIONS);
        out.push_str("\n\n");
    }
    if !history.is_empty() {
        out.push_str("Earlier in this conversation:\n");
        for exchange in history {
            out.push_str(&format!(
                "User: {}\nYou: {}\n",
                exchange.user, exchange.assistant
            ));
        }
        out.push('\n');
    }
    out.push_str("The user just said: ");
    out.push_str(text);
    out
}

fn messages(history: &VecDeque<Exchange>, text: &str) -> Value {
    let mut messages = vec![json!({"role": "system", "content": INSTRUCTIONS})];
    for exchange in history {
        messages.push(json!({"role": "user", "content": exchange.user}));
        messages.push(json!({"role": "assistant", "content": exchange.assistant}));
    }
    messages.push(json!({"role": "user", "content": text}));
    Value::Array(messages)
}

#[derive(Clone, Default)]
struct Cancel(Arc<(AtomicBool, Mutex<Option<Child>>)>);

impl Cancel {
    fn cancelled(&self) -> bool {
        self.0.0.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.0.0.store(true, Ordering::Relaxed);
        if let Some(child) = self.0.1.lock().unwrap().as_mut() {
            kill(child);
        }
    }
}

#[cfg(unix)]
fn kill(child: &mut Child) {
    // The command runs in its own process group, so this also stops anything a shell started.
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

/// Removes terminal escape sequences and carriage returns from streamed output.
#[derive(Default)]
struct Clean {
    bytes: Vec<u8>,
    escape: u8,
}

impl Clean {
    fn push(&mut self, input: &[u8]) -> String {
        for &byte in input {
            match (self.escape, byte) {
                (0, 0x1b) => self.escape = 1,
                (0, b'\r') => {}
                (0, _) => self.bytes.push(byte),
                (1, b'[') => self.escape = 2,
                (1, b']') => self.escape = 3,
                (1, _) => self.escape = 0,
                (2, 0x40..=0x7e) => self.escape = 0,
                (3, 0x07) => self.escape = 0,
                (3, 0x1b) => self.escape = 1,
                _ => {}
            }
        }
        let valid = match std::str::from_utf8(&self.bytes) {
            Ok(_) => self.bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.bytes).into_owned();
                self.bytes.clear();
                return text;
            }
        };
        let rest = self.bytes.split_off(valid);
        String::from_utf8(std::mem::replace(&mut self.bytes, rest)).unwrap()
    }
}

fn run_command(
    backend: &Backend,
    input: String,
    env: &[(&str, String)],
    cancel: &Cancel,
    on_text: &mut dyn FnMut(String),
) -> Result<String> {
    let Backend::Command {
        program, args, cwd, ..
    } = backend
    else {
        unreachable!()
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .with_context(|| format!("Start {}", program.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    *cancel.0.1.lock().unwrap() = Some(child);
    if cancel.cancelled() {
        cancel.cancel();
    }
    thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let errors = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = (&mut stderr).take(1_000_000).read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    let mut clean = Clean::default();
    let mut reply = String::new();
    let mut buffer = [0; 4096];
    loop {
        let count = match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let text = clean.push(&buffer[..count]);
        if !text.is_empty() && !cancel.cancelled() {
            reply.push_str(&text);
            on_text(text);
        }
    }
    let child = cancel.0.1.lock().unwrap().take();
    let status = child.unwrap().wait()?;
    let errors = errors.join().unwrap_or_default();
    if cancel.cancelled() {
        bail!("cancelled");
    }
    if !status.success() && reply.trim().is_empty() {
        let detail = errors
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        bail!("{} exited with {status}: {detail}", program.display());
    }
    Ok(reply)
}

fn run_http(
    backend: &Backend,
    messages: Value,
    cancel: &Cancel,
    on_text: &mut dyn FnMut(String),
) -> Result<String> {
    let Backend::Http { url, model, key } = backend else {
        unreachable!()
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let body = serde_json::to_vec(&json!({
        "model": model, "stream": true, "messages": messages,
    }))?;
    let mut request = agent
        .post(format!("{url}/chat/completions"))
        .header("Content-Type", "application/json");
    if let Some(key) = key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let response = request
        .send(&body[..])
        .with_context(|| format!("Reach {url}"))?;
    let status = response.status();
    let mut reader = BufReader::new(response.into_body().into_reader());
    if !status.is_success() {
        let mut text = String::new();
        let _ = (&mut reader).take(8_192).read_to_string(&mut text);
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|error| error["error"]["message"].as_str().map(String::from))
            .unwrap_or(text);
        bail!("{url} returned {status}: {}", message.trim());
    }
    let mut reply = String::new();
    for line in reader.lines() {
        if cancel.cancelled() {
            bail!("cancelled");
        }
        let line = line?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(data).context("Parse streamed reply")?;
        if let Some(message) = chunk["error"]["message"].as_str() {
            bail!("{message}");
        }
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str()
            && !text.is_empty()
        {
            reply.push_str(text);
            on_text(text.to_owned());
        }
    }
    Ok(reply)
}

enum Message {
    Event(ServerEvent),
    Text(u64, String),
    Done(u64, Result<String, String>),
    Quit,
}

/// The running conversation. Dropping it stops any reply in progress.
pub struct Link {
    tx: mpsc::Sender<Message>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Link {
    pub fn event(&self, event: &ServerEvent) {
        let _ = self.tx.send(Message::Event(event.clone()));
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Job {
    id: u64,
    turn: Turn,
    text: String,
    started: Instant,
    cancel: Cancel,
    reading: bool,
    filler: bool,
}

/// Where the conversation reports what it hears and says.
pub trait Report: Send + 'static {
    fn line(&mut self, text: &str);
    fn problem(&mut self, text: &str);
}

pub struct Terminal;

impl Report for Terminal {
    fn line(&mut self, text: &str) {
        println!("{text}");
    }
    fn problem(&mut self, text: &str) {
        eprintln!("jimmy: {text}");
    }
}

impl Agent {
    /// Starts answering transcripts. `send` delivers speech events to the session.
    pub fn start(
        self,
        mode: Mode,
        send: impl Fn(ClientEvent) + Send + 'static,
        mut report: impl Report,
    ) -> Link {
        let (tx, rx) = mpsc::channel();
        let jobs = tx.clone();
        let backend = Arc::new(self.backend);
        let name = self.name;
        let detail = self.detail;
        let thread = thread::spawn(move || {
            let mut history = VecDeque::new();
            let mut job: Option<Job> = None;
            let mut next_id = 0;
            let stop = |job: &mut Option<Job>| {
                if let Some(job) = job.take() {
                    job.cancel.cancel();
                }
            };
            loop {
                let wait = job.as_ref().map_or(Duration::from_secs(3600), |job| {
                    let deadline = if job.reading || job.filler {
                        TIMEOUT
                    } else {
                        FILLER_AFTER
                    };
                    (job.started + deadline).saturating_duration_since(Instant::now())
                });
                match rx.recv_timeout(wait) {
                    Ok(Message::Event(ServerEvent::Ready { .. })) => {
                        let how = match mode {
                            Mode::Wake => "Say “Hey Jimmy” and ask a question.",
                            Mode::Open => "Start talking.",
                            Mode::Ptt => "Press Enter, talk, and press Enter again.",
                        };
                        report.line(&format!("Talking to {detail}. {how} Press Ctrl+C to stop."));
                    }
                    Ok(Message::Event(ServerEvent::SpeechStart { turn })) => {
                        if job.is_some() {
                            stop(&mut job);
                            send(ClientEvent::SpeakStop { turn: Some(turn) });
                        }
                    }
                    Ok(Message::Event(ServerEvent::Final { turn, transcript })) => {
                        let text = transcript.text.trim().to_owned();
                        if text.is_empty() {
                            continue;
                        }
                        stop(&mut job);
                        report.line(&format!("You: {text}"));
                        next_id += 1;
                        let cancel = Cancel::default();
                        let worker = cancel.clone();
                        let backend = backend.clone();
                        let tx = jobs.clone();
                        let id = next_id;
                        let history_json =
                            json!(history
                            .iter()
                            .map(|e: &Exchange| json!({"user": e.user, "assistant": e.assistant}))
                            .collect::<Vec<_>>())
                            .to_string();
                        let input = match &*backend {
                            Backend::Command { instructions, .. } => {
                                Err(prompt(&history, &text, *instructions))
                            }
                            Backend::Http { .. } => Ok(messages(&history, &text)),
                        };
                        let latest = text.clone();
                        thread::spawn(move || {
                            let texts = tx.clone();
                            let mut on_text = |text| {
                                let _ = texts.send(Message::Text(id, text));
                            };
                            let result = match input {
                                Err(prompt) => run_command(
                                    &backend,
                                    prompt,
                                    &[
                                        ("JIMMY_TEXT", latest),
                                        ("JIMMY_HISTORY", history_json),
                                        ("JIMMY_TURN", turn.to_string()),
                                    ],
                                    &worker,
                                    &mut on_text,
                                ),
                                Ok(messages) => run_http(&backend, messages, &worker, &mut on_text),
                            };
                            let _ =
                                tx.send(Message::Done(id, result.map_err(|e| format!("{e:#}"))));
                        });
                        job = Some(Job {
                            id,
                            turn,
                            text,
                            started: Instant::now(),
                            cancel,
                            reading: false,
                            filler: false,
                        });
                    }
                    Ok(Message::Event(_)) => {}
                    Ok(Message::Text(id, text)) => {
                        let Some(current) = job.as_mut().filter(|job| job.id == id) else {
                            continue;
                        };
                        if !current.reading {
                            if text.trim().is_empty() {
                                continue;
                            }
                            current.reading = true;
                            if current.filler {
                                send(ClientEvent::SpeakStop {
                                    turn: Some(current.turn),
                                });
                            }
                        }
                        send(ClientEvent::SpeakAppend {
                            text,
                            turn: Some(current.turn),
                        });
                    }
                    Ok(Message::Done(id, result)) => {
                        let Some(done) = job.take_if(|job| job.id == id) else {
                            continue;
                        };
                        match result.map(|reply| reply.trim().to_owned()) {
                            Ok(reply) if !reply.is_empty() => {
                                report.line(&format!("{name}: {reply}"));
                                history.push_back(Exchange {
                                    user: done.text,
                                    assistant: reply,
                                });
                                if history.len() > HISTORY {
                                    history.pop_front();
                                }
                            }
                            result => {
                                let error = result.err().unwrap_or_else(|| "empty reply".into());
                                report.problem(&format!("{name} failed: {error}"));
                                if !done.reading {
                                    if done.filler {
                                        send(ClientEvent::SpeakStop {
                                            turn: Some(done.turn),
                                        });
                                    }
                                    send(ClientEvent::SpeakAppend {
                                        text: format!(
                                            "Sorry, I couldn't get an answer from {}.",
                                            spoken_name(&name)
                                        ),
                                        turn: Some(done.turn),
                                    });
                                }
                            }
                        }
                        send(ClientEvent::SpeakFlush {
                            turn: Some(done.turn),
                        });
                    }
                    Ok(Message::Quit) | Err(RecvTimeoutError::Disconnected) => {
                        stop(&mut job);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        let Some(current) = job.as_mut() else {
                            continue;
                        };
                        if current.started.elapsed() >= TIMEOUT {
                            let turn = current.turn;
                            report.problem(&format!(
                                "{name} took longer than {}s",
                                TIMEOUT.as_secs()
                            ));
                            stop(&mut job);
                            send(ClientEvent::SpeakStop { turn: Some(turn) });
                            send(ClientEvent::SpeakAppend {
                                text: "Sorry, that took too long.".into(),
                                turn: Some(turn),
                            });
                            send(ClientEvent::SpeakFlush { turn: Some(turn) });
                        } else if !current.reading && !current.filler {
                            current.filler = true;
                            // Punctuation and a trailing space start playback without flushing the turn.
                            send(ClientEvent::SpeakAppend {
                                text: "Hmm. ".into(),
                                turn: Some(current.turn),
                            });
                        }
                    }
                }
            }
        });
        Link {
            tx,
            thread: Some(thread),
        }
    }
}

fn spoken_name(name: &str) -> &str {
    if name == "Agent" { "the agent" } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jimmy_proto::Transcript;

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Report for Log {
        fn line(&mut self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
        fn problem(&mut self, text: &str) {
            self.0.lock().unwrap().push(format!("problem: {text}"));
        }
    }

    fn final_event(turn: Turn, text: &str) -> ServerEvent {
        ServerEvent::Final {
            turn,
            transcript: Transcript {
                text: text.into(),
                language: "en".into(),
                words: Vec::new(),
            },
        }
    }

    fn spoken(events: &mpsc::Receiver<ClientEvent>) -> (String, Turn) {
        let mut text = String::new();
        loop {
            match events
                .recv_timeout(Duration::from_secs(10))
                .expect("speech")
            {
                ClientEvent::SpeakAppend { text: part, .. } => text.push_str(&part),
                ClientEvent::SpeakFlush { turn } => return (text, turn.unwrap()),
                ClientEvent::SpeakStop { .. } => text.clear(),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    fn talk(agent: Agent) -> (Link, mpsc::Receiver<ClientEvent>, Log) {
        let (tx, rx) = mpsc::channel();
        let log = Log::default();
        let link = agent.start(
            Mode::Wake,
            move |event| {
                let _ = tx.send(event);
            },
            log.clone(),
        );
        (link, rx, log)
    }

    #[cfg(unix)]
    #[test]
    fn command_gets_prompt_and_history_and_is_spoken() {
        let agent = resolve(
            Some(
                r#"printf 'Heard: %s. Turns so far: %s' "$JIMMY_TEXT" "$(cat | grep -c '^User:')""#,
            ),
            &[],
            None,
            None,
        )
        .unwrap();
        let (link, rx, log) = talk(agent);
        link.event(&final_event(3, "my snack is grilled cheese"));
        assert_eq!(
            spoken(&rx),
            (
                "Heard: my snack is grilled cheese. Turns so far: 0".into(),
                3
            )
        );
        link.event(&final_event(4, "what is my snack"));
        assert_eq!(
            spoken(&rx),
            ("Heard: what is my snack. Turns so far: 1".into(), 4)
        );
        drop(link);
        let log = log.0.lock().unwrap();
        assert_eq!(log[0], "You: my snack is grilled cheese");
        assert!(log[1].starts_with("Agent: Heard: my snack"), "{log:?}");
    }

    #[cfg(unix)]
    #[test]
    fn command_after_double_dash_runs_without_a_shell() {
        let command = ["cat".to_owned()];
        let (link, rx, _) = talk(resolve(None, &command, None, None).unwrap());
        link.event(&final_event(1, "echo me"));
        assert_eq!(spoken(&rx), ("echo me".into(), 1));
    }

    #[cfg(unix)]
    #[test]
    fn failing_command_says_sorry_and_slow_command_says_hmm_first() {
        let (link, rx, log) =
            talk(resolve(Some("echo broken >&2; exit 3"), &[], None, None).unwrap());
        link.event(&final_event(1, "hello"));
        assert_eq!(
            spoken(&rx),
            ("Sorry, I couldn't get an answer from the agent.".into(), 1)
        );
        assert!(
            log.0
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("broken")),
            "stderr should be reported"
        );
        drop(link);

        let (link, rx, _) =
            talk(resolve(Some("sleep 2.5; echo Done thinking."), &[], None, None).unwrap());
        link.event(&final_event(2, "think hard"));
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&first, ClientEvent::SpeakAppend { text, .. } if text == "Hmm. "),
            "{first:?}"
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ClientEvent::SpeakStop { turn: Some(2) }
        ));
        assert_eq!(spoken(&rx), ("Done thinking.\n".into(), 2));
    }

    #[cfg(unix)]
    #[test]
    fn new_speech_cancels_a_running_reply() {
        let (link, rx, _) = talk(resolve(Some("sleep 30; echo late"), &[], None, None).unwrap());
        link.event(&final_event(1, "slow question"));
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        link.event(&ServerEvent::SpeechStart { turn: 2 });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            ClientEvent::SpeakStop { turn: Some(2) }
        ));
        drop(link);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn openai_compatible_stream_is_spoken_with_history() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                let turns = body["messages"].as_array().unwrap().len();
                seen.lock().unwrap().push(body);
                let events = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"You sent \"}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":\"{turns} messages.\"}}}}]}}\n\n\
                     data: [DONE]\n\n"
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                    events.len()
                )
                .unwrap();
            }
        });
        let agent = resolve(Some(&url), &[], Some("test-model"), Some("secret".into())).unwrap();
        let (link, rx, _) = talk(agent);
        link.event(&final_event(1, "first"));
        assert_eq!(spoken(&rx), ("You sent 2 messages.".into(), 1));
        link.event(&final_event(2, "second"));
        assert_eq!(spoken(&rx), ("You sent 4 messages.".into(), 2));
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[1]["model"], "test-model");
        assert_eq!(bodies[1]["messages"][1]["content"], "first");
        assert_eq!(bodies[1]["messages"][2]["content"], "You sent 2 messages.");
        assert_eq!(bodies[1]["messages"][3]["content"], "second");
    }

    #[test]
    fn clean_strips_escapes_and_keeps_split_characters() {
        let mut clean = Clean::default();
        let bytes = "\x1b[1mHé\x1b[0m\r\n".as_bytes();
        let mut text = clean.push(&bytes[..6]);
        text += &clean.push(&bytes[6..]);
        assert_eq!(text, "Hé\n");
    }

    #[test]
    fn prompt_includes_recent_turns() {
        let mut history = VecDeque::new();
        assert_eq!(prompt(&history, "hi", false), "hi");
        history.push_back(Exchange {
            user: "my snack is cheese".into(),
            assistant: "Got it.".into(),
        });
        let text = prompt(&history, "what snack?", false);
        assert!(text.contains("User: my snack is cheese\nYou: Got it."));
        assert!(text.ends_with("The user just said: what snack?"));
    }
}
