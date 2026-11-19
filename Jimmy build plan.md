# Jimmy: build plan

Nov 19, 2026 · @Ryan Stefan

## Summary

Jimmy is one Rust binary with one job: turn speech into text, turn text into speech, and wake on "Hey Jimmy". It holds no agent logic. Anything that can open a pipe, a WebSocket or a Worker channel gets ears and a voice.

- **Rust from day one.** No FastAPI prototype. Phonon's runtime is already a Rust crate, and Whistle ships as a static C library with a header.
- **Two models, CPU by default.** Whistle for speech-to-text (16.9 MB, seven languages) and Phonon 31m for text-to-speech (64.5 MB, English).
- **One protocol, three transports.** stdio JSONL, WebSocket, and in-process (a Rust API natively, `postMessage` in a browser Worker).
- **Wake without a third model.** A voice activity detector gates Whistle, and Jimmy matches the transcript against the wake phrase. The phrase is a runtime flag, not a trained artifact.
- **The browser is a sibling package, not the same binary.** `jimmy-web` shares the protocol and the core state machine compiled to wasm, and loads the vendors' own wasm engines.
- **First gate before any pipeline code:** confirm the Cactus engine licence and that `libcactus.a` links on Windows x64.

## What changes from the draft

The draft's shape holds: a voice gateway that knows nothing about the agent. Nine specifics change.

| Draft said | Plan | Why |
| --- | --- | --- |
| FastAPI first, Rust later | Rust only | Both runtimes have native entry points; a Python prototype would be thrown away |
| Whistle's package gives live transcription for free | Jimmy implements the streamer itself | `cactus.stream` is a Python helper; the C header exposes whole-clip `cactus_transcribe` |
| Whistle and Phonon "fit surprisingly well" | They fit, with a language gap | Whistle hears seven languages; the Apache-licensed Phonon speaks English only |
| Wake word deferred to a native app | Wake is the default listen mode | VAD plus a transcript match costs no extra model |
| "Immediate interruption" | Barge-in rules depend on echo cancellation | Without it, Jimmy's own voice re-triggers the mic on open speakers |
| Opus output | PCM16 by default, Opus behind a feature flag | Localhost needs no codec; Opus adds a C dependency |
| One binary serves the browser too | Native binary plus `jimmy-web` | Each vendor ships its own browser artifact; Jimmy composes them rather than relinking them |
| Assistant adapter inside the gateway | Out of scope | Jimmy emits transcripts and accepts text; the caller owns routing |
| Five packages (`hara-server`, `hara-client`, ...) | One workspace, one binary, one npm package | Fewer moving parts for the same reach |

## Scope and non-goals

Jimmy does three things and exposes every input and output those three things need.

| In scope | Detail |
| --- | --- |
| Speech to text | Live partials, stable finals, word timestamps, detected language, keyword boosting, file or stream input |
| Text to speech | Streaming text in, streaming audio out, voice and rate selection, file or stream output |
| Wake | "Hey Jimmy" by default, configurable phrase, follow-up window, push-to-talk and open-mic modes |
| Turn control | Voice activity detection, endpointing, barge-in, cancel with turn IDs |
| Cues | Play a cached clip by ID with zero synthesis latency |
| Audio plumbing | Device capture and playback, resampling, PCM16 and optional Opus |

Out of scope, permanently unless this doc changes:

- LLM calls, agent routing, tool calls, conversation memory
- Speaker identification, diarization, translation
- Voice cloning workflow (Jimmy loads a voice file; making one is someone else's job)
- Telephony and WebRTC media servers
- User accounts (the server takes a bearer token and an origin allowlist, nothing more)

## Models and runtime

Both models are real, recent and CPU-first, and both can be driven from Rust without Python. Whistle was released on 2 October 2026 and Phonon was open-sourced later that month.

| Role | Choice | Weights | Rust entry point | Limits that shape the design |
| --- | --- | --- | --- | --- |
| Speech to text | [Whistle](https://cactuscompute.com/blog/whistle) | 16.9 MB, Apache 2.0 | FFI to `libcactus.a` via `cactus.h` | 16 kHz mono in; 30 s per pass; 320-token cap; seven languages |
| Text to speech | [Phonon 31m](https://github.com/gradium-ai/xn-ptts) | 64.5 MB, Apache 2.0 | `ptts` crate (MIT or Apache 2.0) | English only; 24 kHz mono out |
| Wake | VAD plus Whistle transcript match | none | in `jimmy-core` | See the Wake section |
| Voice activity | Pure-Rust detector behind a `Vad` trait | under 2 MB | in `jimmy-core` | Pick by measurement in M1 |

### Whistle facts that matter

- **C surface.** [`cactus.h`](https://cactuscompute.com/blog/needle-supported-devices) exposes `cactus_load`, `cactus_init`, `cactus_complete`, `cactus_transcribe` and `cactus_embed`. Each platform folder ships `libcactus.a` and the header, including `windows-x86_64`.
- **Streaming is ours to build.** The Python [`cactus.stream`](https://cactuscompute.com/blog/whistle-best-practices) re-transcribes the buffer on every chunk and commits words that two consecutive passes agree on. Jimmy ports that rule to Rust on top of `cactus_transcribe`.
- **Re-transcribing is cheap.** Cactus reports 5.9 ms to first token on a 5 s clip and 36.3 ms on a 30 s clip, on an Apple M4 Pro CPU.
- **It needs a gate.** The [package notes](https://pypi.org/project/cactus-needle/) say silence returns an empty transcript, but music and other sounds can still come out as text. A VAD in front is mandatory.
- **Extras worth exposing.** Word timestamps with probabilities, keyword boosting, automatic language detection, and a decoder-depth switch that trades accuracy for speed.

### Phonon facts that matter

- **Library, not sidecar.** `tts.stream(text)` yields PCM chunks frame by frame. Jimmy links `ptts` directly and ignores the bundled `ptts-ws-server` and `ptts-openai-server`.
- **Speed.** Gradium reports 26x real time with 5 ms to first audio on one MacBook Air M5 core, and 2.3x with 68 ms on one Raspberry Pi 5 core.
- **Licence split.** Only the English 31m model is Apache 2.0. The 40m models, including multilingual, are [CC BY-NC 4.0](https://gradium.ai/phonon) and need a commercial licence.
- **Voices.** Voice files are `.safetensors`. The 31m model ships with built-in English voices, and custom voices load from a path.
- **Browser.** The npm package `phonon-tts` runs in a wasm Worker on CPU by default. Version [0.7.0](https://newreleases.io/project/github/gradium-ai/xn-ptts/release/v0.7.0) added a purpose-built WebGPU engine.

### On WebGPU

WebGPU is opt-in for browser TTS only, and Jimmy does not depend on it. Whistle is a CPU engine with no GPU path. Phonon already runs many times faster than real time on one core, so a GPU buys little and adds a failure mode.

## Architecture

The core is a synchronous state machine with no I/O, no threads and no async runtime. Everything else is a thin shell around it, which is what lets the same logic run natively and in wasm.

&#91;embedded content: Jimmy architecture · 3 transports, 1 core, 3 engines\]

A caller picks a transport; the core never knows which. Swapping a model means writing one new trait implementation.

| Crate | Holds | Depends on |
| --- | --- | --- |
| `jimmy-proto` | Event types, binary frame header, generated TypeScript types | `serde` only |
| `jimmy-core` | `Session` state machine, VAD, wake matcher, endpointing, streaming agreement, sentence chunker, `Stt` and `Tts` traits | `jimmy-proto` |
| `jimmy-whistle` | `Stt` implementation: FFI bindings, static link of `libcactus.a` | `jimmy-core` |
| `jimmy-phonon` | `Tts` implementation over `ptts` | `jimmy-core`, `ptts` |
| `jimmy-audio` | Device capture and playback, resampling, ring buffers, optional Opus and echo cancellation | `cpal`, `rubato` |
| `jimmy` | The binary: CLI, stdio transport, WebSocket and HTTP server, model fetch | all of the above, `tokio`, `axum` |
| `jimmy-web` | wasm build of proto and core, Worker glue, TypeScript client | `jimmy-core`, vendor browser packages |

### The session

One `Session` owns one conversation's state: listen mode, current turn ID, audio buffer, committed and pending words, and the speak queue. Its whole surface is four calls.

```rust
session.push_audio(&pcm16k);        // mic frames in
session.push_event(ClientEvent);    // speak, cancel, configure, cue
session.poll() -> Option<ServerEvent>;   // transcripts, wake, state
session.pull_audio(&mut out) -> usize;   // synthesized PCM out
```

### Threading rules

1. The audio device callback never allocates, locks or runs inference. It only reads and writes lock-free ring buffers.
2. Each session's pipeline runs on one worker thread, so session state needs no locks.
3. Model weights load once per process and are shared read-only. Inference concurrency is bounded by a semaphore sized to physical cores.
4. `tokio` appears only in the `jimmy` binary, at the network edge. It hands frames to workers over bounded channels.
5. Cancel is a turn-ID bump. Workers check it between Whistle passes and between Phonon frames, so a cancel lands within one audio buffer.

### Text handling before synthesis

Agents emit markdown and code. A small filter strips markup, drops fenced code, and expands nothing else; Phonon's own preprocessor handles numbers and abbreviations. A chunker releases text to Phonon at sentence or clause boundaries, with a short first chunk to cut time to first audio.

## Interfaces

Every surface speaks the same events; they differ only in transport and in who owns the audio device.

| Surface | Invocation | Audio device owned by | Use |
| --- | --- | --- | --- |
| One-shot speak | `jimmy say "text"` or stdin; `-o out.wav`; `--voice`, `--rate` | Jimmy, or none with `-o` | Scripts, notifications |
| One-shot transcribe | `jimmy transcribe clip.wav`; `--json` for words and timings | none | Batch files, tests |
| Live listen | `jimmy listen`; `--mode wake\|ptt\|open`; `--wake "hey jimmy"` | Jimmy | Transcripts to stdout as text or JSONL |
| Sidecar | `jimmy pipe` | Jimmy | JSONL events on stdin and stdout; the desktop app's path |
| Server | `jimmy serve --bind 127.0.0.1:7071` | the client | WebSocket at `/v1/stream`, many sessions |
| HTTP one-shots | `POST /v1/audio/speech`, `POST /v1/audio/transcriptions` | the client | Drop-in for OpenAI-style clients |
| Rust library | `jimmy-core` plus the two model crates | the host app | Embedding with no process boundary |
| Browser | `jimmy-web` npm package | the page | In-tab inference, or a remote `jimmy serve` |
| Housekeeping | `jimmy pull`, `jimmy doctor`, `jimmy voices`, `jimmy devices` | none | Fetch models, self-test, list voices and devices |

### Rules across all surfaces

- **Plain text out by default, JSONL with `--json`.** Exit codes are meaningful, logs go to stderr, and stdout carries only data.
- **Config precedence:** flags, then `JIMMY_*` environment variables, then `jimmy.toml`, then defaults. No config is required to run.
- **`jimmy pipe` keeps audio off the pipe.** Jimmy owns the mic and speaker, so the host app exchanges only small JSON lines.
- **The server binds to localhost by default.** A public bind requires `--token` and `--allow-origin`, and enables per-session rate limits on synthesis.
- **The browser client has one switch.** `new Jimmy({ engine: "local" })` runs in a Worker; `{ engine: "wss://..." }` talks to a server. The event stream is identical.

## Wire protocol

Control is JSON, audio is binary, and every output carries a turn ID so late audio from a cancelled turn is dropped by the client.

### Framing

- **Control:** one JSON object per WebSocket text frame, or per line on stdio. Every object has `type`; server events add `turn`.
- **Audio in:** binary frames of PCM16 little-endian, 16 kHz mono by default. Other rates are declared in `session.configure` and resampled by Jimmy.
- **Audio out:** binary frames with an 8-byte header, `u32 turn` then `u32 seq`, followed by PCM16 at 24 kHz mono.
- **Version:** the server's first event is `session.ready` with `protocol: 1`. Unknown event types are ignored, never fatal.

### Client to Jimmy

| Event | Fields | Effect |
| --- | --- | --- |
| `session.configure` | `mode`, `wake`, `language`, `keywords`, `voice`, `rate`, `input_rate`, `output_codec`, `endpoint_ms` | Sets session options; valid at any time |
| binary frame | PCM | Mic audio |
| `listen.start` / `listen.stop` | none | Push-to-talk edges; `stop` forces a final transcript |
| `speak.append` | `text` | Adds text to the speak queue; may arrive token by token |
| `speak.flush` | none | Synthesizes whatever is buffered, even mid-sentence |
| `cue.play` | `id` | Plays a cached clip; queued in order with speech |
| `cancel` | optional `turn` | Stops listening or speaking for that turn and clears its queue |

### Jimmy to client

| Event | Fields | Meaning |
| --- | --- | --- |
| `session.ready` | `protocol`, `models`, `voices`, `sample_rates` | Models are warm |
| `wake` | `turn`, `phrase`, `confidence` | Wake phrase heard; a turn has opened |
| `speech.start` / `speech.end` | `turn` | VAD edges of user speech |
| `transcript.partial` | `turn`, `committed`, `pending` | `committed` only grows; replace `pending` on every event |
| `transcript.final` | `turn`, `text`, `language`, `words[]` | Each word carries start, end and probability |
| `speak.start` | `turn`, `sample_rate`, `codec` | Audio frames for this turn follow |
| binary frame | header plus PCM | Synthesized audio |
| `speak.end` | `turn`, `reason` | `done`, `cancelled` or `error` |
| `cue` | `turn`, `id` | Client-owned devices play the clip themselves |
| `error` | `code`, `message`, optional `turn` | Session survives unless `fatal: true` |

### Turn semantics

1. A turn opens on `wake`, on `listen.start`, or on the first `speak.append` after the previous turn closed.
2. User speech during Jimmy's speech is a barge-in. Jimmy emits `speak.end` with `cancelled`, bumps the turn, and starts transcribing.
3. After `cancel` or a barge-in, the client drops any binary frame whose `turn` is stale. Jimmy drops any `speak.append` it receives for a stale turn.
4. `cue.play` needs no synthesis, so it is the way to acknowledge instantly while the agent thinks.

## Wake: "Hey Jimmy"

Version 1 detects the wake phrase by transcribing the first seconds of each speech burst with Whistle and matching the text. It adds no model, no training step and no extra runtime, and it works wherever Whistle works.

### How it runs

1. **Idle.** Only the VAD runs, on 16 kHz frames. A 300 ms pre-roll buffer keeps the phrase's first syllable.
2. **Speech onset.** Jimmy buffers audio and runs Whistle at 0.8 s, then every 0.4 s, up to 2.5 s.
3. **Match.** The transcript's leading words are normalized and compared to the phrase with a small tolerance ("hey jimmie", "hay jimmy").
4. **Hit.** Jimmy emits `wake`, uses word timestamps to cut the phrase out, and keeps transcribing. "Hey Jimmy, what's the weather" works in one breath.
5. **Miss.** The buffer is dropped and Jimmy sleeps until the VAD sees 500 ms of silence. A TV in the room costs at most five Whistle passes per speech burst.
6. **Follow-up window.** After Jimmy finishes speaking, the mic stays open for 8 s without the phrase.

### Defaults and switches

| Setting | Default | Notes |
| --- | --- | --- |
| `--wake` | `"hey jimmy"` | Any phrase; two or more words recommended |
| `--mode` | `wake` for `listen` and `pipe`; `ptt` for browser clients | Mobile browsers need a user gesture for the mic |
| `--followup-ms` | 8000 | 0 disables the window |
| `--endpoint-ms` | 600 | Silence that ends a turn |
| Keyword boost for the phrase | off | Boosting raises recall and false accepts; decide from soak data |

### If soak testing fails the false-accept target

Add a trained classifier as a first stage behind a cargo feature. [livekit-wakeword](https://github.com/livekit/livekit-wakeword) is Apache 2.0, trains a custom phrase from one YAML file with synthetic speech, and ships a Rust crate. Its conv-attention model reports 0.08 false positives per hour at 86.1% recall on its own "hey livekit" test set. The cost is an ONNX dependency and a native-only wake path until a wasm route is proven.

## Targets and packaging

Tier 1 is the set of platforms where both vendors publish desktop builds today. "Any OS" means those five native targets plus the browser at 1.0, with mobile as a library later.

| Target | Tier | Whistle | Phonon | Notes |
| --- | --- | --- | --- | --- |
| Linux x64 | 1 | prebuilt static lib | Rust crate | Build against musl for a fully static binary |
| Linux ARM64 | 1 | prebuilt static lib | Rust crate | Covers Raspberry Pi 5 |
| macOS ARM64 | 1 | prebuilt static lib | Rust crate |  |
| macOS x64 | 1 | prebuilt static lib | Rust crate |  |
| Windows x64 | 1 | prebuilt static lib | Rust crate | Linking under MSVC is unproven; see Risks |
| Browser | 1 | vendor browser artifact | `phonon-tts` npm package | Needs wasm SIMD; WebGPU optional for TTS |
| Windows ARM64 | 2 | prebuilt static lib | compiles from source, untested | Build in CI, no release promise |
| iOS, Android | 3 | prebuilt static lib | Swift and Kotlin packages | `jimmy-core` as a library, not a binary |

### Shipping the models

| Build | Binary size | Models | For |
| --- | --- | --- | --- |
| `jimmy` (default) | target under 15 MB | Fetched by `jimmy pull` or on first run into the user cache, pinned by SHA-256 | Most installs |
| `jimmy-bundled` | about 100 MB | Compiled in with `include_bytes!` | Air-gapped machines, single-file deploys, app sidecars |

Both builds accept `--model-dir` to load local files and never touch the network when it is set.

### The browser build

`jimmy-web` runs everything in one Worker. An AudioWorklet captures and plays audio and trades frames with the Worker over a `SharedArrayBuffer` ring when the page is cross-origin isolated, and over `postMessage` otherwise. The Worker hosts `jimmy-core` compiled to wasm for turn logic, and calls the Cactus browser artifact and `phonon-tts` behind the same `Stt` and `Tts` interfaces. Browser capture uses the built-in echo canceller, so open-mic barge-in works there first.

### Release mechanics

- One GitHub Actions matrix builds all native targets and attaches checksums and an SBOM.
- Install paths: `cargo install jimmy`, a shell and PowerShell installer, Homebrew, and `npm i jimmy-web`.
- Vendor versions are pinned: one Cactus runtime version, one `ptts` version, one commit hash per model.

## Performance budgets and acceptance tests

These are targets for Jimmy, set from the vendors' published numbers and not yet measured. M0 replaces them with measurements on two reference machines: a desktop x64 and a Raspberry Pi 5.

| Budget | Desktop target | Pi 5 target |
| --- | --- | --- |
| Cold start to `session.ready` | under 500 ms | under 2 s |
| Idle listening, no speech (one core) | under 1% | under 5% |
| Partial transcript cadence | every 300 ms | every 600 ms |
| End of speech to `transcript.final` | endpoint silence plus 100 ms | endpoint silence plus 400 ms |
| `speak.append` to first audio frame | under 100 ms | under 250 ms |
| `cancel` to silence | under 50 ms | under 50 ms |
| Resident memory, both models warm | under 250 MB | under 250 MB |
| Wake false accepts | under 1 per 8 hours of TV audio | same |
| Wake misses, quiet room at 1 m | under 5% | same |

### Acceptance tests

- [ ] **Golden transcripts.** A fixed WAV corpus through `jimmy transcribe`; word error rate must not regress between releases.
- [ ] **Round trip.** `jimmy say` piped into `jimmy transcribe` returns the input text for 200 sentences, within a set error rate.
- [ ] **Streaming stability.** No committed word ever changes in a later `transcript.partial`.
- [ ] **Cancel.** After `cancel`, no audio frame for the stale turn is emitted and the next turn's first frame is clean.
- [ ] **Wake soak.** 24 hours of podcast and TV audio, with planted wake phrases at known times.
- [ ] **Protocol conformance.** One scripted suite runs against stdio, WebSocket and the browser Worker and expects identical event sequences.
- [ ] **Load.** 16 concurrent WebSocket sessions on the desktop machine hold the latency budgets at the 95th percentile.
- [ ] **Real-time safety.** A debug build asserts on any allocation or lock inside the audio callback.

## Milestones

Six milestones, each ending in something runnable. Effort is a rough estimate for one developer and totals six to seven weeks.

| Milestone | Builds | Exit test | Effort |
| --- | --- | --- | --- |
| **M0 Spike (gate)** | Rust FFI to `libcactus.a`; `ptts` call; measurements | A WAV transcribes and a sentence synthesizes from one Rust binary on Linux x64 and Windows x64; licence read; budgets replaced with measurements | 2 to 3 days |
| **M1 Core and one-shots** | `jimmy-proto`, `jimmy-core`, VAD choice, streaming agreement; `say`, `transcribe`, `listen --mode open`, `pull`, `doctor` | Golden transcript, round trip and streaming stability tests pass | 1 week |
| **M2 Duplex and wake** | Device I/O, wake, endpointing, cancel, barge-in, cues, `pipe` | Hands-free loop with an echo agent: "Hey Jimmy", partials, spoken reply, cue, interruption | 1 week |
| **M3 Server** | `serve`, WebSocket, HTTP one-shots, token and origin checks, bounded concurrency | Load test holds budgets at 16 sessions | 1 week |
| **M4 Browser** | `jimmy-web`, TypeScript client, local and remote engines | Conformance suite passes on all three transports | 1 to 2 weeks |
| **M5 Hardening and 1.0** | Wake soak, release matrix, installers, docs | Soak targets met; protocol version 1 frozen | 1 week |

M0 is a real gate. If the licence or the Windows link fails, the fallback is decided before M1 starts, not discovered during it.

M2 is the draft's "first experiment" (live transcription, streaming speech, the clip cue, interruption) plus wake. It lands after M1 because all four depend on the session state machine.

## Risks and open questions

The biggest risk is legal, not technical: sources disagree on the Cactus engine's licence, and Jimmy links that engine into every native build.

| Risk | What is known | Resolve in | Fallback |
| --- | --- | --- | --- |
| Cactus engine licence | The [Cactus repo](https://github.com/cactus-compute/cactus) states Apache 2.0 including prebuilt binaries. A [third-party review](https://github.com/IAFahim/needle-cs/issues/34) describes a custom licence, free only for individuals and for organizations under $2M in funding and revenue. Whistle's weights are Apache 2.0 either way. | M0: read the licence file shipped in the runtime folder | Another speech model behind the same `Stt` trait, at several times the size |
| Engine arrives as a prebuilt library | I found no confirmation that the engine source is public. Jimmy's platform reach is then Cactus's target list. | M0 | Accept the list; it covers every tier 1 target |
| Windows linking | Windows x64 ships `libcactus.a`, a GNU-style archive. Linking it from Rust's MSVC target is unproven. | M0 | Build the Windows binary for the `gnullvm` target |
| Options through the C API | Keyword boosting, word timestamps and decoder depth are documented for Python and the CLI. The C options format is not in what I read. | M0 | Wake match works without boosting; timestamps are required, so this is a gate |
| Thread safety of engine handles | Not documented in what I read. | M0 | One handle per worker thread |
| Echo on open speakers | Native capture has no echo canceller. Jimmy would hear itself and self-interrupt. | M2 | On native speakers, barge-in needs the wake phrase or push-to-talk; add an echo-cancel feature later |
| English-only voice | Whistle hears seven languages; the Apache Phonon model speaks one. | Now, by decision | Licence Phonon 40m, or leave replies in English |
| Long dictation | Whistle caps a pass at 30 s and 320 tokens. | M1 | Segment on pauses and force a commit at 25 s |
| Young dependencies | Both models are under two months old and their APIs are moving. | Ongoing | Pin versions; the `Stt` and `Tts` traits contain any swap |
| Browser engine glue | The Cactus browser artifact's JavaScript surface is not documented in what I read. | M4 spike | Browser uses a remote `jimmy serve` for speech-to-text only |

### Decisions needed from you

- [ ] Will Jimmy ship inside client or commercial work? This decides how strict the licence gate is.
- [ ] Does Jimmy need to speak any language besides English?
- [ ] Should the desktop app run Jimmy as a sidecar (`jimmy pipe`) or link `jimmy-core` directly? The plan assumes sidecar first.
- [ ] Which Phonon voice is the default?
- [ ] Are the names `jimmy` on crates.io and `jimmy-web` on npm free? Not checked.

## Sources

- [Whistle: Speech to Text in 16.9 MB](https://cactuscompute.com/blog/whistle), Cactus Compute
- [Getting the Most out of Whistle](https://cactuscompute.com/blog/whistle-best-practices), Cactus Compute
- [What Devices Are Supported on Needle](https://cactuscompute.com/blog/needle-supported-devices), Cactus Compute
- [cactus-compute/cactus](https://github.com/cactus-compute/cactus), GitHub
- [cactus-needle](https://pypi.org/project/cactus-needle/), PyPI
- [gradium-ai/xn-ptts](https://github.com/gradium-ai/xn-ptts), GitHub
- [Phonon](https://gradium.ai/phonon), Gradium
- [xn-ptts 0.7.0 release notes](https://newreleases.io/project/github/gradium-ai/xn-ptts/release/v0.7.0)
- [Gradium Open-Sources Phonon](https://www.marktechpost.com/2026/10/22/gradium-phonon-31m-on-device-tts/), MarkTechPost (64.5 MB file size)
- [livekit/livekit-wakeword](https://github.com/livekit/livekit-wakeword), GitHub
- [needle-cs issue 34](https://github.com/IAFahim/needle-cs/issues/34), GitHub (licence review)
