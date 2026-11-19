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
    ClientEvent, Codec, INPUT_RATE, OUTPUT_RATE, ServerEvent, Turn, decode_pcm16, encode_audio,
    pcm16_bytes,
};
use jimmy_whistle::Whistle;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::Cursor,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, mpsc};

const MAX_MESSAGE: usize = 96_000;
const MAX_UPLOAD: usize = 25 * 1024 * 1024;

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

    fn authorized(&self, headers: &HeaderMap) -> bool {
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

async fn access(State(app): State<App>, request: Request<Body>, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    if origin.as_ref().is_some_and(|value| {
        value
            .to_str()
            .map_or(true, |origin| !app.security.origin_ok(origin))
    }) {
        return api_error(StatusCode::FORBIDDEN, "Origin is not allowed");
    }
    let mut response = if request.method() == Method::OPTIONS && origin.is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else if !app.security.authorized(request.headers()) {
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
    let (tx, rx) = mpsc::channel(24);
    let (out, mut output) = mpsc::channel(8);
    let turn = Arc::new(AtomicU64::new(0));
    let current = turn.clone();
    let public = app.security.public;
    if std::thread::Builder::new()
        .name("jimmy-session".into())
        .spawn(move || worker(app, rx, out, current))
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
                if tx.try_send(input).is_err() { break; }
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
    mut input: mpsc::Receiver<Input>,
    output: mpsc::Sender<Output>,
    current: Arc<AtomicU64>,
) {
    let mut session = Session::new(app.engines.stt, app.engines.tts, app.settings);
    let mut pending = VecDeque::from([Output::Control(session.poll().expect("session ready"))]);
    let mut turn = 0;
    let mut seq = 0u32;
    let mut playback_key = 0u64;
    #[cfg(feature = "opus")]
    let mut opus: Option<jimmy_audio::Opus> = None;
    let mut pcm = [0; 480];
    while !output.is_closed() {
        // Read a bounded batch before taking another inference step. A cancel
        // invalidates packets staged here and in the outbound channel.
        for _ in 0..8 {
            match input.try_recv() {
                Ok(Input::Control(event)) => session.push_event(event),
                Ok(Input::Audio(samples)) => session.push_audio(&samples),
                Ok(Input::Invalid(message)) if pending.len() < 64 => {
                    pending.push_back(Output::Control(ServerEvent::error(
                        Some(session.turn()),
                        "invalid_input",
                        message,
                    )))
                }
                Ok(Input::Invalid(_)) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
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
        if !sent {
            std::thread::sleep(Duration::from_millis(2));
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
