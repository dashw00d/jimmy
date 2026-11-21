//! Connects Jimmy's ears and voice to any agent: Codex, Claude, Ollama, an
//! OpenAI-compatible URL, or any command that reads a prompt and prints a reply.
use anyhow::{Context, Result, bail};
use jimmy_proto::{ClientEvent, Mode, ServerEvent, Turn};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    env,
    io::{self, BufRead, BufReader, Read, Write},
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
    Codex {
        program: PathBuf,
        model: String,
        cwd: PathBuf,
    },
    Claude {
        program: PathBuf,
        model: String,
        cwd: PathBuf,
    },
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
    let model = model.unwrap_or(CODEX_MODEL).to_owned();
    Ok(Agent {
        name: "Codex".into(),
        detail: format!("Codex CLI, {model}"),
        backend: Backend::Codex {
            program,
            model,
            cwd: env::temp_dir(),
        },
    })
}

fn claude(model: Option<&str>) -> Result<Agent> {
    let program = find_program("claude").context(
        "The Claude Code CLI is not on PATH. Install it, then run `claude` once to sign in.",
    )?;
    let model = model.unwrap_or(CLAUDE_MODEL).to_owned();
    Ok(Agent {
        name: "Claude".into(),
        detail: format!("Claude Code CLI, {model}"),
        backend: Backend::Claude {
            program,
            model,
            cwd: env::temp_dir(),
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
        if let Some(child) = self.0.1.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
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

fn spawn_managed(mut command: Command, cancel: &Cancel) -> Result<(std::process::ChildStdin, std::process::ChildStdout, thread::JoinHandle<String>)> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    *cancel.0.1.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    if cancel.cancelled() {
        cancel.cancel();
    }
    let errors = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = (&mut stderr).take(1_000_000).read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    Ok((stdin, stdout, errors))
}

fn finish_managed(cancel: &Cancel, errors: thread::JoinHandle<String>, kill_child: bool) -> Result<(std::process::ExitStatus, String)> {
    let Some(mut child) = cancel.0.1.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        bail!("command process handle missing");
    };
    if kill_child {
        kill(&mut child);
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    Ok((status, errors))
}

fn last_error_line(errors: &str) -> &str {
    errors
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
}

/// Streams token deltas from `codex app-server --stdio` via JSON-RPC.
fn run_codex(
    program: &PathBuf,
    model: &str,
    cwd: &PathBuf,
    prompt_text: String,
    cancel: &Cancel,
    on_text: &mut dyn FnMut(String),
) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args([
            "app-server",
            "--stdio",
            "--disable",
            "memories",
            "--disable",
            "shell_tool",
            "--disable",
            "unified_exec",
            "--disable",
            "hooks",
            "--disable",
            "plugins",
            "-c",
            "mcp_servers={}",
            "-c",
            "web_search=\"disabled\"",
        ])
        .current_dir(cwd);
    let (mut stdin, stdout, errors) = spawn_managed(command, cancel)
        .with_context(|| format!("Start {}", program.display()))?;
    let mut reader = BufReader::new(stdout);
    let mut notifications = VecDeque::new();

    let send = |stdin: &mut std::process::ChildStdin, msg: &Value| -> Result<()> {
        serde_json::to_writer(&mut *stdin, msg)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    };
    let read_event = |reader: &mut BufReader<std::process::ChildStdout>| -> Result<Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let count = reader.read_line(&mut line)?;
            if count == 0 {
                bail!("Codex app-server closed the stream");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed).context("Parse Codex app-server event");
        }
    };
    let decline = |stdin: &mut std::process::ChildStdin, event: &Value| -> Result<()> {
        send(
            stdin,
            &json!({
                "id": event["id"],
                "error": {
                    "code": -32601,
                    "message": "Jimmy conversation does not grant tool requests",
                }
            }),
        )
    };
    let request = |stdin: &mut std::process::ChildStdin,
                   reader: &mut BufReader<std::process::ChildStdout>,
                   notifications: &mut VecDeque<Value>,
                   id: u64,
                   method: &str,
                   params: Value|
     -> Result<Value> {
        send(stdin, &json!({"id": id, "method": method, "params": params}))?;
        loop {
            if cancel.cancelled() {
                bail!("cancelled");
            }
            let event = read_event(reader)?;
            if event.get("method").is_some() && event.get("id").is_some() {
                decline(stdin, &event)?;
            } else if event.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = event.get("error") {
                    let msg = err["message"].as_str().unwrap_or("Codex RPC error");
                    bail!("{msg}");
                }
                return Ok(event["result"].clone());
            } else if event.get("method").is_some() {
                notifications.push_back(event);
            }
        }
    };

    let outcome: Result<String> = (|| {
        request(
            &mut stdin,
            &mut reader,
            &mut notifications,
            1,
            "initialize",
            json!({"clientInfo": {"name": "jimmy", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        send(&mut stdin, &json!({"method": "initialized"}))?;
        let started = request(
            &mut stdin,
            &mut reader,
            &mut notifications,
            2,
            "thread/start",
            json!({
                "cwd": cwd.to_string_lossy(),
                "ephemeral": true,
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "developerInstructions": INSTRUCTIONS,
                "model": model,
            }),
        )?;
        let thread_id = started["thread"]["id"]
            .as_str()
            .context("Codex thread/start missing thread id")?
            .to_owned();
        let turn_res = request(
            &mut stdin,
            &mut reader,
            &mut notifications,
            3,
            "turn/start",
            json!({
                "threadId": thread_id,
                "effort": "medium",
                "input": [{"type": "text", "text": prompt_text}],
            }),
        )?;
        let turn_id = turn_res["turn"]["id"]
            .as_str()
            .context("Codex turn/start missing turn id")?
            .to_owned();

        let mut commentary = HashSet::<String>::new();
        let mut streamed = HashSet::<String>::new();
        let mut reply = String::new();
        loop {
            if cancel.cancelled() {
                bail!("cancelled");
            }
            let event = match notifications.pop_front() {
                Some(ev) => ev,
                None => read_event(&mut reader)?,
            };
            let Some(method) = event.get("method").and_then(Value::as_str) else {
                continue;
            };
            if event.get("id").is_some() {
                decline(&mut stdin, &event)?;
                continue;
            }
            let params = &event["params"];
            if params.get("threadId").and_then(Value::as_str) != Some(thread_id.as_str()) {
                continue;
            }
            if method == "turn/completed"
                && params["turn"]["id"].as_str() == Some(turn_id.as_str())
            {
                let status = params["turn"]["status"].as_str().unwrap_or("unknown");
                if status != "completed" {
                    let msg = params["turn"]["error"]["message"]
                        .as_str()
                        .unwrap_or(status);
                    bail!("{msg}");
                }
                break;
            }
            if params.get("turnId").and_then(Value::as_str) != Some(turn_id.as_str()) {
                continue;
            }
            let mut delta: Option<String> = None;
            if matches!(method, "item/started" | "item/completed") {
                let item = &params["item"];
                if item["type"].as_str() == Some("agentMessage")
                    && let Some(item_id) = item["id"].as_str()
                {
                    if item["phase"].as_str() == Some("commentary") {
                        commentary.insert(item_id.to_owned());
                    } else if method == "item/completed"
                        && !streamed.contains(item_id)
                        && let Some(text) = item["text"].as_str()
                    {
                        delta = Some(text.to_owned());
                    }
                }
            } else if method == "item/agentMessage/delta"
                && let Some(item_id) = params["itemId"].as_str()
                && !commentary.contains(item_id)
                && let Some(text) = params["delta"].as_str()
            {
                streamed.insert(item_id.to_owned());
                delta = Some(text.to_owned());
            }
            if let Some(text) = delta
                && !text.is_empty()
                && !cancel.cancelled()
            {
                reply.push_str(&text);
                on_text(text);
            }
        }
        Ok(reply)
    })();

    drop(stdin);
    let (_, err_text) = finish_managed(cancel, errors, true)?;
    if cancel.cancelled() {
        bail!("cancelled");
    }
    outcome.map_err(|err| {
        let detail = last_error_line(&err_text);
        if detail != "no output" {
            anyhow::anyhow!("{err} ({detail})")
        } else {
            err
        }
    })
}

/// Streams token deltas from `claude --print --output-format stream-json`.
fn run_claude(
    program: &PathBuf,
    model: &str,
    cwd: &PathBuf,
    prompt_text: String,
    cancel: &Cancel,
    on_text: &mut dyn FnMut(String),
) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args([
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
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ])
        .current_dir(cwd);
    let (mut stdin, stdout, errors) = spawn_managed(command, cancel)
        .with_context(|| format!("Start {}", program.display()))?;
    thread::spawn(move || {
        let _ = stdin.write_all(prompt_text.as_bytes());
    });
    let mut reader = BufReader::new(stdout);
    let mut reply = String::new();
    let mut saw_delta = false;
    let mut stream_error: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if cancel.cancelled() {
            break;
        }
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match event["type"].as_str() {
            Some("stream_event") => {
                let inner = &event["event"];
                if inner["type"].as_str() == Some("content_block_delta")
                    && inner["delta"]["type"].as_str() == Some("text_delta")
                    && let Some(text) = inner["delta"]["text"].as_str()
                    && !text.is_empty()
                {
                    saw_delta = true;
                    reply.push_str(text);
                    on_text(text.to_owned());
                }
            }
            Some("result") => {
                if event["is_error"].as_bool() == Some(true) {
                    stream_error = event["result"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| Some("Claude returned an error".into()));
                } else if !saw_delta
                    && let Some(text) = event["result"].as_str()
                    && !text.is_empty()
                {
                    reply.push_str(text);
                    on_text(text.to_owned());
                }
            }
            _ => {}
        }
    }
    let (status, err_text) = finish_managed(cancel, errors, false)?;
    if cancel.cancelled() {
        bail!("cancelled");
    }
    if let Some(msg) = stream_error {
        bail!("{msg}");
    }
    if !status.success() && reply.trim().is_empty() {
        bail!(
            "{} exited with {status}: {}",
            program.display(),
            last_error_line(&err_text)
        );
    }
    Ok(reply)
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
        .envs(env.iter().map(|(key, value)| (key, value)));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let (mut stdin, mut stdout, errors) = spawn_managed(command, cancel)
        .with_context(|| format!("Start {}", program.display()))?;
    thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
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
    let (status, errors) = finish_managed(cancel, errors, false)?;
    if cancel.cancelled() {
        bail!("cancelled");
    }
    if !status.success() && reply.trim().is_empty() {
        bail!(
            "{} exited with {status}: {}",
            program.display(),
            last_error_line(&errors)
        );
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
    fn reply_start(&mut self, name: &str);
    fn reply_chunk(&mut self, text: &str);
    fn reply_end(&mut self);
}

pub struct Terminal;

impl Report for Terminal {
    fn line(&mut self, text: &str) {
        println!("{text}");
    }
    fn problem(&mut self, text: &str) {
        eprintln!("jimmy: {text}");
    }
    fn reply_start(&mut self, name: &str) {
        print!("{name}: ");
        let _ = io::stdout().flush();
    }
    fn reply_chunk(&mut self, text: &str) {
        print!("{text}");
        let _ = io::stdout().flush();
    }
    fn reply_end(&mut self) {
        println!();
    }
}

enum WorkerInput {
    Prompt(String),
    Messages(Value),
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
            let stop = |job: &mut Option<Job>, report: &mut dyn Report| {
                if let Some(job) = job.take() {
                    if job.reading {
                        report.reply_end();
                    }
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
                            stop(&mut job, &mut report);
                            send(ClientEvent::SpeakStop { turn: Some(turn) });
                        }
                    }
                    Ok(Message::Event(ServerEvent::Final { turn, transcript })) => {
                        let text = transcript.text.trim().to_owned();
                        if text.is_empty() {
                            continue;
                        }
                        stop(&mut job, &mut report);
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
                            Backend::Codex { .. } | Backend::Claude { .. } => {
                                WorkerInput::Prompt(prompt(&history, &text, false))
                            }
                            Backend::Command { instructions, .. } => {
                                WorkerInput::Prompt(prompt(&history, &text, *instructions))
                            }
                            Backend::Http { .. } => WorkerInput::Messages(messages(&history, &text)),
                        };
                        let latest = text.clone();
                        thread::spawn(move || {
                            let texts = tx.clone();
                            let mut on_text = |text| {
                                let _ = texts.send(Message::Text(id, text));
                            };
                            let result = match (&*backend, input) {
                                (
                                    Backend::Codex {
                                        program,
                                        model,
                                        cwd,
                                    },
                                    WorkerInput::Prompt(prompt_text),
                                ) => run_codex(
                                    program,
                                    model,
                                    cwd,
                                    prompt_text,
                                    &worker,
                                    &mut on_text,
                                ),
                                (
                                    Backend::Claude {
                                        program,
                                        model,
                                        cwd,
                                    },
                                    WorkerInput::Prompt(prompt_text),
                                ) => run_claude(
                                    program,
                                    model,
                                    cwd,
                                    prompt_text,
                                    &worker,
                                    &mut on_text,
                                ),
                                (Backend::Command { .. }, WorkerInput::Prompt(prompt_text)) => {
                                    run_command(
                                        &backend,
                                        prompt_text,
                                        &[
                                            ("JIMMY_TEXT", latest),
                                            ("JIMMY_HISTORY", history_json),
                                            ("JIMMY_TURN", turn.to_string()),
                                        ],
                                        &worker,
                                        &mut on_text,
                                    )
                                }
                                (Backend::Http { .. }, WorkerInput::Messages(messages)) => {
                                    run_http(&backend, messages, &worker, &mut on_text)
                                }
                                _ => unreachable!(),
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
                            report.reply_start(&name);
                            if current.filler {
                                send(ClientEvent::SpeakStop {
                                    turn: Some(current.turn),
                                });
                            }
                        }
                        report.reply_chunk(&text);
                        send(ClientEvent::SpeakAppend {
                            text,
                            turn: Some(current.turn),
                        });
                    }
                    Ok(Message::Done(id, result)) => {
                        let Some(done) = job.take_if(|job| job.id == id) else {
                            continue;
                        };
                        if done.reading {
                            report.reply_end();
                        }
                        match result.map(|reply| reply.trim().to_owned()) {
                            Ok(reply) if !reply.is_empty() => {
                                if !done.reading {
                                    report.line(&format!("{name}: {reply}"));
                                }
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
                        stop(&mut job, &mut report);
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
                            stop(&mut job, &mut report);
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
    struct Log {
        lines: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        current: Arc<Mutex<String>>,
    }

    impl Report for Log {
        fn line(&mut self, text: &str) {
            self.lines.lock().unwrap().push(text.to_owned());
        }
        fn problem(&mut self, text: &str) {
            self.lines
                .lock()
                .unwrap()
                .push(format!("problem: {text}"));
        }
        fn reply_start(&mut self, name: &str) {
            let mut cur = self.current.lock().unwrap();
            cur.clear();
            cur.push_str(&format!("{name}: "));
        }
        fn reply_chunk(&mut self, text: &str) {
            self.chunks.lock().unwrap().push(text.to_owned());
            self.current.lock().unwrap().push_str(text);
        }
        fn reply_end(&mut self) {
            let line = std::mem::take(&mut *self.current.lock().unwrap());
            if !line.is_empty() {
                self.lines.lock().unwrap().push(line);
            }
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
        let log = log.lines.lock().unwrap();
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
            log.lines
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

    #[cfg(unix)]
    #[test]
    fn codex_app_server_streams_deltas_and_skips_commentary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-codex");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, sys, time
for line in sys.stdin:
    msg = json.loads(line)
    m = msg.get("method")
    if m == "initialize":
        print(json.dumps({"id": msg["id"], "result": {}}), flush=True)
    elif m == "thread/start":
        print(json.dumps({"id": msg["id"], "result": {"thread": {"id": "thr-1"}}}), flush=True)
    elif m == "turn/start":
        print(json.dumps({"id": msg["id"], "result": {"turn": {"id": "turn-1"}}}), flush=True)
        # Emit commentary (must be ignored)
        print(json.dumps({"method": "item/started", "params": {"threadId": "thr-1", "turnId": "turn-1", "item": {"id": "c1", "type": "agentMessage", "phase": "commentary"}}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thr-1", "turnId": "turn-1", "itemId": "c1", "delta": "thinking..."}}), flush=True)
        # Emit streamed reply deltas
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thr-1", "turnId": "turn-1", "itemId": "m1", "delta": "Hello "}}), flush=True)
        time.sleep(0.05)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thr-1", "turnId": "turn-1", "itemId": "m1", "delta": "streamed world."}}), flush=True)
        print(json.dumps({"method": "item/completed", "params": {"threadId": "thr-1", "turnId": "turn-1", "item": {"id": "m1", "type": "agentMessage", "text": "Hello streamed world."}}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {"threadId": "thr-1", "turn": {"id": "turn-1", "status": "completed"}}}), flush=True)
        break
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let agent = Agent {
            name: "Codex".into(),
            detail: "fake codex".into(),
            backend: Backend::Codex {
                program: script,
                model: "test".into(),
                cwd: dir.path().to_path_buf(),
            },
        };
        let (link, rx, log) = talk(agent);
        link.event(&final_event(1, "hi"));
        let mut parts = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ClientEvent::SpeakAppend { text, .. } => parts.push(text),
                ClientEvent::SpeakFlush { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        drop(link);
        assert_eq!(parts, vec!["Hello ", "streamed world."]);
        assert_eq!(*log.chunks.lock().unwrap(), vec!["Hello ", "streamed world."]);
        let lines = log.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l == "Codex: Hello streamed world."), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn claude_stream_json_streams_text_deltas() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, sys
_ = sys.stdin.read()
print(json.dumps({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "First "}}}), flush=True)
print(json.dumps({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "second."}}}), flush=True)
print(json.dumps({"type": "result", "is_error": False, "result": "First second."}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let agent = Agent {
            name: "Claude".into(),
            detail: "fake claude".into(),
            backend: Backend::Claude {
                program: script,
                model: "haiku".into(),
                cwd: dir.path().to_path_buf(),
            },
        };
        let (link, rx, log) = talk(agent);
        link.event(&final_event(1, "hi"));
        let mut parts = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ClientEvent::SpeakAppend { text, .. } => parts.push(text),
                ClientEvent::SpeakFlush { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        drop(link);
        assert_eq!(parts, vec!["First ", "second."]);
        assert_eq!(*log.chunks.lock().unwrap(), vec!["First ", "second."]);
    }
}
