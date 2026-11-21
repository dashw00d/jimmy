use crate::{models, validate_speech};
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Multipart, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jimmy_core::{Session, Settings, Speech};
use jimmy_phonon::Phonon;
use jimmy_proto::{
    ClientEvent, Codec, INPUT_RATE, OUTPUT_RATE, PROTOCOL, ServerEvent, Turn, decode_pcm16,
    encode_audio, pcm16_bytes,
};
use jimmy_whistle::Whistle;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::Cursor,
    net::SocketAddr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, mpsc};

const MAX_MESSAGE: usize = 96_000;
const MAX_UPLOAD: usize = 25 * 1024 * 1024;
const MAX_CONTROL_BACKLOG: usize = 64;
const MAX_AUDIO_BACKLOG: usize = 256;
const MAX_AUDIO_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct Engines {
    pub stt: Whistle,
    pub tts: Phonon,
}

#[derive(Clone)]
pub struct Security {
    token: Option<Vec<u8>>,
    origins: Vec<String>,
    public: bool,
}

impl Security {
    pub fn new(bind: SocketAddr, token: Option<String>, origins: Vec<String>) -> Result<Self> {
        let public = !bind.ip().is_loopback();
        ensure!(
            token.as_ref().is_none_or(|t| !t.trim().is_empty()),
            "Empty bearer token"
        );
        ensure!(
            !public || (token.is_some() && !origins.is_empty()),
            "Public bind requires --token and --allow-origin"
        );
        ensure!(
            origins.iter().all(|origin| {
                origin.parse::<Uri>().is_ok_and(|uri| {
                    matches!(uri.scheme_str(), Some("http" | "https"))
                        && uri.authority().is_some()
                        && uri.path() == "/"
                        && uri.query().is_none()
                }) && !origin.ends_with('/')
            }),
            "--allow-origin requires exact http(s) origins without a trailing slash"
        );
        Ok(Self {
            token: token.map(String::into_bytes),
            origins,
            public,
        })
    }

    fn authorized(&self, headers: &HeaderMap, ws_upgrade: bool) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.as_bytes());
        if auth.is_some_and(|token| bool::from(expected.ct_eq(token))) {
            return true;
        }
        if !ws_upgrade {
            return false;
        }
        // Browser WebSocket constructors cannot set Authorization. A second
        // offered subprotocol can carry the base64url-encoded bearer token.
        headers
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|protocol| protocol.trim().strip_prefix("bearer."))
            .filter_map(|token| URL_SAFE_NO_PAD.decode(token).ok())
            .any(|token| bool::from(expected.ct_eq(&token)))
    }

    fn origin_ok(&self, origin: &str) -> bool {
        if !self.origins.is_empty() {
            return self.origins.iter().any(|allowed| allowed == origin);
        }
        origin.parse::<Uri>().is_ok_and(|uri| {
            matches!(uri.scheme_str(), Some("http" | "https"))
                && matches!(
                    uri.host(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                )
                && uri.path() == "/"
                && uri.query().is_none()
        })
    }
}

#[derive(Clone)]
struct App {
    engines: Engines,
    settings: Settings,
    security: Security,
    sessions: Arc<Semaphore>,
    one_shots: Arc<Semaphore>,
}

pub async fn run(
    bind: SocketAddr,
    engines: Engines,
    settings: Settings,
    security: Security,
) -> Result<()> {
    let app = App {
        engines,
        settings,
        security,
        sessions: Arc::new(Semaphore::new(32)),
        one_shots: Arc::new(Semaphore::new(num_cpus::get_physical().clamp(1, 16))),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/stream", get(upgrade))
        .route("/v1/audio/speech", post(speech))
        .route("/v1/audio/transcriptions", post(transcriptions))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .layer(middleware::from_fn_with_state(app.clone(), access))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("Jimmy listening on {}", listener.local_addr()?);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok", "protocol": PROTOCOL}))
}

async fn access(State(app): State<App>, request: Request<Body>, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    if origin.as_ref().is_some_and(|value| {
        value
            .to_str()
            .map_or(true, |origin| !app.security.origin_ok(origin))
    }) {
        return api_error(StatusCode::FORBIDDEN, "Origin is not allowed");
    }
    let is_health = request.method() == Method::GET && request.uri().path() == "/health";
    let ws_upgrade = request.uri().path() == "/v1/stream"
        && request
            .headers()
            .get(header::UPGRADE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let mut response = if request.method() == Method::OPTIONS && origin.is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else if !is_health && !app.security.authorized(request.headers(), ws_upgrade) {
        api_error(StatusCode::UNAUTHORIZED, "Bearer token required")
    } else {
        next.run(request).await
    };
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization, content-type"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

fn api_error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message.to_string()}})),
    )
        .into_response()
}

async fn upgrade(State(app): State<App>, ws: WebSocketUpgrade) -> Response {
    let Ok(permit) = app.sessions.clone().try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "All session slots are in use",
        );
    };
    ws.protocols(["jimmy.v1"])
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .max_write_buffer_size(MAX_MESSAGE * 2)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            web_socket(socket, app).await;
        })
}

enum Input {
    Control(ClientEvent),
    Audio(Vec<i16>),
    Invalid(&'static str),
}

struct InboundState {
    control: VecDeque<Input>,
    audio: VecDeque<Vec<i16>>,
    audio_bytes: usize,
    closed: bool,
}

/// Bounded inbound queue that prioritizes control events ahead of buffered
/// audio frames and wakes the session worker immediately on arrival.
struct Inbound {
    state: Mutex<InboundState>,
    wake: Condvar,
}

impl Inbound {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InboundState {
                control: VecDeque::new(),
                audio: VecDeque::new(),
                audio_bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
        })
    }

    fn push(&self, input: Input) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        match input {
            Input::Audio(samples) => {
                let bytes = samples.len().saturating_mul(2);
                if state.audio.len() >= MAX_AUDIO_BACKLOG
                    || state.audio_bytes.saturating_add(bytes) > MAX_AUDIO_BYTES
                {
                    return false;
                }
                state.audio_bytes += bytes;
                state.audio.push_back(samples);
            }
            other => {
                if state.control.len() >= MAX_CONTROL_BACKLOG {
                    return false;
                }
                state.control.push_back(other);
            }
        }
        self.wake.notify_one();
        true
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.wake.notify_one();
    }

    /// Pops up to `limit` items, always delivering control events before audio.
    /// Returns `None` once closed and completely drained.
    fn drain(&self, limit: usize, out: &mut Vec<Input>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while out.len() < limit {
            if let Some(event) = state.control.pop_front() {
                out.push(event);
            } else if let Some(samples) = state.audio.pop_front() {
                state.audio_bytes = state.audio_bytes.saturating_sub(samples.len() * 2);
                out.push(Input::Audio(samples));
            } else {
                break;
            }
        }
        !out.is_empty() || !state.closed
    }

    /// Blocks until input arrives or the queue closes. When `timeout` is `Some`,
    /// wakes after at most that duration so active synthesis or outbound drain
    /// can make progress without busy-spinning.
    fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.control.is_empty() || !state.audio.is_empty() {
            return true;
        }
        if state.closed {
            return false;
        }
        match timeout {
            Some(duration) => {
                let (guard, _) = self
                    .wake
                    .wait_timeout(state, duration)
                    .unwrap_or_else(|e| e.into_inner());
                !guard.closed || !guard.control.is_empty() || !guard.audio.is_empty()
            }
            None => {
                while state.control.is_empty() && state.audio.is_empty() && !state.closed {
                    state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                !state.closed || !state.control.is_empty() || !state.audio.is_empty()
            }
        }
    }
}

struct CloseOnDrop(Arc<Inbound>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

enum Output {
    Control(ServerEvent),
    Audio { playback_key: u64, data: Vec<u8> },
}

struct Budget {
    left: f64,
    capacity: f64,
    per_second: f64,
    updated: Instant,
}

impl Budget {
    fn new(capacity: f64, per_second: f64) -> Self {
        Self {
            left: capacity,
            capacity,
            per_second,
            updated: Instant::now(),
        }
    }
    fn spend(&mut self, units: usize) -> bool {
        let now = Instant::now();
        self.left = (self.left + now.duration_since(self.updated).as_secs_f64() * self.per_second)
            .min(self.capacity);
        self.updated = now;
        if self.left < units as f64 {
            return false;
        }
        self.left -= units as f64;
        true
    }
}

async fn web_socket(mut socket: WebSocket, app: App) {
    let inbound = Inbound::new();
    let _close = CloseOnDrop(inbound.clone());
    let worker_in = inbound.clone();
    let (out, mut output) = mpsc::channel(8);
    let turn = Arc::new(AtomicU64::new(0));
    let current = turn.clone();
    let public = app.security.public;
    if std::thread::Builder::new()
        .name("jimmy-session".into())
        .spawn(move || worker(app, worker_in, out, current))
        .is_err()
    {
        return;
    }
    let mut audio_budget = Budget::new(1_200_000.0, 450_000.0);
    let mut speech_budget = Budget::new(2_000.0, 80.0);
    loop {
        tokio::select! {
            biased;
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break; };
                let input = match msg {
                    Message::Text(text) => match serde_json::from_str(&text) {
                        Ok(ClientEvent::SpeakAppend { ref text, .. }) if public && !speech_budget.spend(text.len()) =>
                            Input::Invalid("Speech rate limit exceeded"),
                        Ok(event) => Input::Control(event),
                        Err(_) => Input::Invalid("Malformed control event"),
                    },
                    Message::Binary(bytes) => {
                        if public && !audio_budget.spend(bytes.len()) {
                            Input::Invalid("Audio rate limit exceeded")
                        } else if let Some(samples) = decode_pcm16(&bytes) {
                            Input::Audio(samples)
                        } else { Input::Invalid("PCM16 input needs an even byte count") }
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                if !inbound.push(input) { break; }
            }
            event = output.recv() => {
                let Some(event) = event else { break; };
                let message = match event {
                    Output::Control(event) => match serde_json::to_string(&event) {
                        Ok(json) => Message::Text(json.into()),
                        Err(_) => break,
                    },
                    Output::Audio { playback_key, data } if playback_key == turn.load(Ordering::Acquire) => Message::Binary(data.into()),
                    Output::Audio { .. } => continue,
                };
                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(message)).await, Ok(Ok(()))) {
                    break;
                }
            }
        }
    }
}

fn queue_audio(
    pending: &mut VecDeque<Output>,
    turn: Turn,
    playback_key: u64,
    seq: &mut u32,
    payload: &[u8],
) {
    pending.push_back(Output::Audio {
        playback_key,
        data: encode_audio(turn, *seq, payload),
    });
    *seq = seq.wrapping_add(1);
}

fn worker(
    app: App,
    input: Arc<Inbound>,
    output: mpsc::Sender<Output>,
    current: Arc<AtomicU64>,
) {
    let mut session = Session::new(app.engines.stt, app.engines.tts, app.settings);
    let mut pending = VecDeque::from([Output::Control(session.poll().expect("session ready"))]);
    let mut batch = Vec::with_capacity(8);
    let mut turn = 0;
    let mut seq = 0u32;
    let mut playback_key = 0u64;
    #[cfg(feature = "opus")]
    let mut opus: Option<jimmy_audio::Opus> = None;
    let mut pcm = [0; 480];
    while !output.is_closed() {
        // Read a bounded batch before taking another inference step. Control
        // events jump ahead of audio so cancels land immediately.
        batch.clear();
        if !input.drain(8, &mut batch) {
            return;
        }
        let had_input = !batch.is_empty();
        for item in batch.drain(..) {
            match item {
                Input::Control(event) => session.push_event(event),
                Input::Audio(samples) => session.push_audio(&samples),
                Input::Invalid(message) if pending.len() < 64 => {
                    pending.push_back(Output::Control(ServerEvent::error(
                        Some(session.turn()),
                        "invalid_input",
                        message,
                    )))
                }
                Input::Invalid(_) => {}
            }
            if pending.len() >= 128 {
                break;
            }
        }
        if session.turn() != turn {
            turn = session.turn();
            seq = 0;
        }
        let key = (u64::from(turn) << 32) | u64::from(session.playback_revision());
        if key != playback_key {
            playback_key = key;
            current.store(key, Ordering::Release);
            #[cfg(feature = "opus")]
            {
                opus = None;
            }
            pending.retain(|event| !matches!(event, Output::Audio { .. }));
        }
        while let Some(event) = session.poll() {
            pending.push_back(Output::Control(event));
        }
        if pending.len() > 128 {
            return;
        }
        let mut pulled_audio = false;
        if pending.is_empty() && output.capacity() > 0 {
            let count = session.pull_audio(&mut pcm);
            while let Some(event) = session.poll() {
                #[cfg(feature = "opus")]
                if matches!(
                    event,
                    ServerEvent::SpeakEnd {
                        reason: jimmy_proto::EndReason::Done,
                        ..
                    }
                ) && let Some(mut encoder) = opus.take()
                {
                    match encoder.finish() {
                        Ok(Some(bytes)) => {
                            queue_audio(&mut pending, turn, playback_key, &mut seq, &bytes)
                        }
                        Ok(None) => {}
                        Err(error) => pending.push_back(Output::Control(ServerEvent::error(
                            Some(turn),
                            "opus",
                            error.to_string(),
                        ))),
                    }
                }
                pending.push_back(Output::Control(event));
            }
            if count > 0 {
                pulled_audio = true;
                let payload = match session.codec() {
                    Codec::Pcm16 => Some(pcm16_bytes(&pcm[..count])),
                    Codec::Opus => {
                        #[cfg(feature = "opus")]
                        {
                            let encoded = (|| {
                                if opus.is_none() {
                                    opus = Some(jimmy_audio::Opus::new()?);
                                }
                                opus.as_mut().unwrap().push(&pcm[..count])
                            })();
                            match encoded {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    pending.push_back(Output::Control(ServerEvent::error(
                                        Some(turn),
                                        "opus",
                                        error.to_string(),
                                    )));
                                    session.push_event(ClientEvent::Cancel { turn: Some(turn) });
                                    None
                                }
                            }
                        }
                        #[cfg(not(feature = "opus"))]
                        None
                    }
                };
                if let Some(payload) = payload {
                    queue_audio(&mut pending, turn, playback_key, &mut seq, &payload);
                }
            }
        }
        let mut sent = false;
        while let Some(event) = pending.pop_front() {
            match output.try_send(event) {
                Ok(()) => sent = true,
                Err(mpsc::error::TrySendError::Full(event)) => {
                    pending.push_front(event);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
        if !sent && !had_input && !pulled_audio {
            // Sleep indefinitely when idle so idle sessions cause zero wakeups;
            // wait briefly when actively speaking or waiting for channel space.
            let timeout = (session.is_speaking() || !pending.is_empty())
                .then_some(Duration::from_millis(2));
            if !input.wait(timeout) {
                return;
            }
        }
    }
}

#[derive(Deserialize)]
struct SpeechRequest {
    input: String,
    voice: Option<String>,
    speed: Option<f32>,
    response_format: Option<String>,
}

async fn speech(State(app): State<App>, Json(request): Json<SpeechRequest>) -> Response {
    if let Err(error) = validate_speech(&request.input) {
        return api_error(StatusCode::BAD_REQUEST, error);
    }
    let rate = request.speed.unwrap_or(app.settings.rate);
    if !rate.is_finite()
        || !(0.5..=2.0).contains(&rate)
        || request
            .response_format
            .as_deref()
            .is_some_and(|format| !matches!(format, "wav" | "pcm"))
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Supported speech formats are wav and pcm, with speed between 0.5 and 2",
        );
    }
    if app.security.public && request.input.len() > 2_000 {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Public speech requests are limited to 2000 bytes",
        );
    }
    let Ok(permit) = app.one_shots.clone().try_acquire_owned() else {
        return api_error(StatusCode::TOO_MANY_REQUESTS, "Inference is busy");
    };
    let result = tokio::task::spawn_blocking(move || -> Result<_> {
        let _permit = permit;
        let mut tts = app.engines.tts;
        let voice = request
            .voice
            .filter(|v| !v.is_empty())
            .or(app.settings.voice);
        let pcm = models::synthesize(&mut tts, &request.input, &Speech { voice, rate })?;
        if request.response_format.as_deref() == Some("pcm") {
            return Ok(("application/octet-stream", pcm16_bytes(&pcm)));
        }
        let mut buffer = Cursor::new(Vec::new());
        jimmy_audio::write_wav(&mut buffer, &pcm, OUTPUT_RATE)?;
        Ok(("audio/wav", buffer.into_inner()))
    })
    .await;
    match result {
        Ok(Ok((mime, data))) => ([(header::CONTENT_TYPE, mime)], data).into_response(),
        Ok(Err(error)) => api_error(StatusCode::UNPROCESSABLE_ENTITY, error),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Inference worker stopped",
        ),
    }
}

async fn transcriptions(State(app): State<App>, mut form: Multipart) -> Response {
    let mut file = None;
    let mut options = app.settings.recognition.clone();
    let mut format = String::from("json");
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return api_error(error.status(), error.body_text()),
        };
        match field.name() {
            Some("file") => match field.bytes().await {
                Ok(bytes) => file = Some(bytes),
                Err(error) => return api_error(error.status(), error.body_text()),
            },
            Some("language" | "prompt" | "response_format") => {
                let name = field.name().unwrap().to_owned();
                let Ok(value) = field.text().await else {
                    return api_error(StatusCode::BAD_REQUEST, "Invalid form field");
                };
                if value.len() > 4_096 {
                    return api_error(StatusCode::BAD_REQUEST, "Form text is too large");
                }
                match name.as_str() {
                    "language" => {
                        options.language = (!value.is_empty() && value != "auto").then_some(value)
                    }
                    "prompt" => options.keywords.extend(
                        value
                            .split([',', '\n'])
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    ),
                    _ => format = value,
                }
            }
            _ => {}
        }
    }
    if options.keywords.len() > 128 {
        return api_error(StatusCode::BAD_REQUEST, "Too many keyword hints");
    }
    let Some(file) = file else {
        return api_error(StatusCode::BAD_REQUEST, "Missing WAV file field");
    };
    if !matches!(format.as_str(), "text" | "json" | "verbose_json") {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Supported transcription formats are text, json and verbose_json",
        );
    }
    let Ok(permit) = app.one_shots.try_acquire_owned() else {
        return api_error(StatusCode::TOO_MANY_REQUESTS, "Inference is busy");
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let pcm = jimmy_audio::read_wav_limited(Cursor::new(file), INPUT_RATE, 900)?;
        models::transcribe(&mut app.engines.stt.clone(), &pcm, &options)
    })
    .await;
    match result {
        Ok(Ok(result)) if format == "text" => result.text.into_response(),
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(error)) => api_error(StatusCode::UNPROCESSABLE_ENTITY, error),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Inference worker stopped",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_subprotocol_token_is_only_accepted_on_ws_upgrades() {
        let sec = Security::new(
            ([127, 0, 0, 1], 7071).into(),
            Some("secret-token".into()),
            Vec::new(),
        )
        .unwrap();
        let encoded = URL_SAFE_NO_PAD.encode("secret-token");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_str(&format!("jimmy.v1, bearer.{encoded}")).unwrap(),
        );
        assert!(
            !sec.authorized(&headers, false),
            "HTTP endpoints must not accept Sec-WebSocket-Protocol auth"
        );
        assert!(
            sec.authorized(&headers, true),
            "WebSocket upgrade must accept Sec-WebSocket-Protocol bearer token"
        );

        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        assert!(sec.authorized(&bearer, false));
        assert!(sec.authorized(&bearer, true));
    }

    #[test]
    fn inbound_queue_prioritizes_control_and_absorbs_audio_bursts() {
        let inbound = Inbound::new();
        for _ in 0..100 {
            assert!(inbound.push(Input::Audio(vec![1; 320])));
        }
        assert!(inbound.push(Input::Control(ClientEvent::Cancel { turn: None })));
        let mut batch = Vec::new();
        assert!(inbound.drain(4, &mut batch));
        assert!(
            matches!(batch[0], Input::Control(ClientEvent::Cancel { .. })),
            "control event must jump ahead of queued audio"
        );
        assert!(matches!(batch[1], Input::Audio(_)));
    }

    #[test]
    fn inbound_wait_blocks_when_idle_and_wakes_on_push_or_close() {
        let inbound = Inbound::new();
        let waiter = inbound.clone();
        let handle = std::thread::spawn(move || {
            let mut got = Vec::new();
            assert!(waiter.wait(None));
            assert!(waiter.drain(4, &mut got));
            assert_eq!(got.len(), 1);
            assert!(!waiter.wait(None));
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(inbound.push(Input::Control(ClientEvent::ListenStart)));
        std::thread::sleep(Duration::from_millis(20));
        inbound.close();
        handle.join().unwrap();
    }
}
