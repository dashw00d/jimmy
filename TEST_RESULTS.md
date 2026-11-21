# Jimmy verification

The implementation was built and tested on an AMD Ryzen 7 5800X3D Linux x64 desktop with Rust 1.99.0. The model checkpoints were the SHA-256-pinned Whistle and English Phonon 31m files. The browser tests used headless Chromium.

## Functional results

| Test | Result |
| --- | --- |
| Rust workspace | 11 tests passed. Covered protocol framing, stable partials, wake timing and suppression, follow-up, barge-in, cancellation, Markdown, resampling, and allocation-free callback conversion. |
| Rust lint | Clippy passed with warnings denied. |
| Golden transcription | 73 real LibriSpeech clips, 119 errors in 1,150 words, 10.35% WER. This first measured result is the regression baseline for the pinned corpus. |
| CLI round trip | 200 sentences through `say -o -` and `transcribe -`. 170 exactly matched after normalization. 37 errors in 2,020 words, 1.83% WER. |
| Transport conformance | Identical control sequence over JSONL, WebSocket, local browser Worker, and browser WebSocket. The JSONL check used ALSA's null capture and playback devices. Chromium playback exercised both AudioWorklet transport modes. |
| HTTP | Real-model WAV synthesis and multipart transcription passed. Missing bearer tokens and disallowed origins were rejected. |
| Wake and cancellation | Native WebSocket and local browser recognized “Hey Jimmy” in the same breath as a question and removed the phrase from the final. The wake smoke detected 8 of 8 planted phrases in 188.66 seconds of speech with no false accepts. The WebSocket discarded stale output and the next turn restarted at sequence zero. |
| Optional builds | Opus packet framing passed a real encode/decode test. The bundled executable ran both models with a fresh cache and network proxies pointed at an unavailable port. `jimmy pull` independently downloaded and verified the pinned models. |
| Windows x64 | The Rust executable linked with the actual Cactus 4.1.4 archive using LLVM-MinGW. Native Windows execution remains unverified. The Wine environment failed to initialize its own Windows libraries. |

## Desktop measurements

| Measure | Observed | Build plan target |
| --- | --- | --- |
| Local model initialization | 109 ms in one CLI run with the pinned files already present | Under 500 ms |
| Warm idle server RSS | 168,420 KiB before connecting clients | Under 250 MB |
| Single WebSocket first audio | 140 ms from flush | Under 100 ms |
| WebSocket cancellation | About 2 ms to the cancelled event | Under 50 ms to silence |
| 16 WebSocket sessions, p95 final | 1,778 ms from PTT stop in the final isolated run | Under 100 ms after the endpoint |
| 16 WebSocket sessions, p95 first audio | 140 ms in the same run | Under 100 ms |
| Linux release executable | 14.8 MB | Under 15 MB |

The load case used simultaneous PTT turns with a 3-second clip. The Cactus ABI has one process-wide engine state, which serializes final inference. Speculative passes yield when that engine is busy. The cancellation measurement is for the transport event. Physical speaker tail latency needs its own measurement.

## Device path

Measured on 2026-11-19 with `scripts/tests/device_rig.py` ([how to run it](docs/device-rig.md)). The rig runs `jimmy pipe` through CPAL, ALSA and the PipeWire ALSA plugin on a private PipeWire 1.6.2 server with a virtual 48 kHz stereo microphone and speaker. The machine was the 16-core desktop above while it was doing other work: the load average was between 7 and 25 before any load the rig added. Jimmy was configured as Plexa configures it: wake mode, "hey jimmy", no wake reply.

### Reading a reply aloud

The reply is the rig's built-in four paragraphs, 60.4 s of speech. A cut is a gap of digital silence with sound in the 60 ms before it, so speech was interrupted. A stretched pause is a gap that follows 60 ms of quiet, so a pause that was there anyway got longer.

| Condition | Cuts inside words | Stretched pauses | Device xruns | First sound |
| --- | --- | --- | --- | --- |
| Default quantum (2048 frames), idle | 0 (0 ms) | 0 (0 ms) | 0 | 410 ms |

A sink that takes 8192 frames a cycle is beyond the buffer Jimmy first asks for. Jimmy notices the device running dry and reopens it with a longer period, but only between replies, which took about five seconds after launch here. A reply that starts before that plays with gaps.

The fix costs latency. Over six replies the first sound came 232 to 453 ms after `speak.append`, against 80 to 246 ms before.

### Stopping a reply

`speak.stop` after 1, 3 and 6 s of audible speech, then "Hey Jimmy. What is the weather like today?" played into the microphone at once. Every stop ended with `speak.end` and reason `cancelled`, no `speak.start` followed, and nothing played more than a second later.

| Stop after | `speak.end` after stop | Sound after stop | Wake after stop | Command word errors |
| --- | --- | --- | --- | --- |
| 1.0 s | 2 ms | 105 ms | yes | 0/6 |
| 3.0 s | 2 ms | 96 ms | yes | 0/6 |
| 6.0 s | 0 ms | *(landed in pause)* | yes | 0/6 |

Sound after the stop is measured from the moment the rig wrote `speak.stop` to the last sample above the noise at the virtual speaker, and is late by up to one graph quantum. Stops that landed in a pause of the speech are left out. A real sink adds its own latency: about 140 ms for the Bluetooth SBC sink this was written for.

### Wake phrase & adaptive VAD

The initial baseline (`EnergyVad` RMS floor = 100, noise tracked only on non-speech frames) failed on quiet speech (`3/15` wakes, `87%` WER at `-50 dBFS`) and latched permanently on steady background noise (`0/15` wakes, `100%` WER at `-19.5 dBFS` over `-40 dBFS` noise).

Replacing the detector with a pure-Rust 6-band minimum-statistics spectral VAD (`crates/jimmy-core/src/vad.rs`) plus foreground surge/endpoint gating and progressive partial-interval backoff was verified on `device_rig.py wake` (pauses of 0, 0.8 and 2.5 s, two commands per condition, plus 1.06 min of non-wake speech):

| Speech level | Background | Wake detected | Command WER | Median final after end |
| --- | --- | --- | --- | --- |
| -19.5 dBFS | none | 6/6 | 8.3% | 0.60 s |
| -19.5 dBFS | steady noise at -40 dBFS | 6/6 | 8.3% | 0.57 s |
| -39.5 dBFS | none | 6/6 | 8.3% | 0.57 s |
| -39.5 dBFS | steady noise at -60 dBFS | 6/6 | 8.3% | 0.58 s |
| -50 dBFS | none | 6/6 | 8.3% | 0.66 s |
| -50 dBFS | steady noise at -70 dBFS | 6/6 | 8.3% | 0.69 s |
| -19.5 dBFS | second talker at -40 dBFS | 3/6 | 66.7% | 0.65 s |
| -39.5 dBFS | second talker at -60 dBFS | 4/6 | 83.3% | 0.63 s |
| -50 dBFS | second talker at -70 dBFS | 5/6 | 55.6% | 0.70 s |

- **Quiet speech & steady noise.** All quiet and steady-noise rows reached `6/6` wake detection and `8.3%` WER (1 word out of 12, Whistle's "pull" -> "poor/Paul/pool" phonetic ambiguity), down from `0/15` on steady `-40 dBFS` noise and `3/15` at `-50 dBFS`.
- **False wakes.** `0` in 1.06 minutes of non-wake speech at gain 1.0 (which included "Hey, Jamie, did you see the game" and "Jim said the meeting moved").
- **Second talker.** Foreground surge breakout and peak-level endpoint gating prevent a quieter background talker from locking `wait_silence` or holding a woken turn open to the 25 s cap, though overlapping speech at `-20 dB` still degrades recall and WER when the background talker speaks across the wake phrase itself.
- **While Jimmy speaks through a speaker the microphone hears**, without echo cancellation it still transcribes its own voice while checking for a wake interruption (a 60 s reply cost 154 CPU-seconds against 68 without the echo).

### Server, Web, Streaming, and Workspace checks (2026-11-21)

- **Rust workspace:** 56 tests passed (`cargo test --locked --workspace` + Opus feature tests + ignored real-model `pipe_wake` test), Clippy passed with `-D warnings` on default and `--features opus`.
- **Hallucination guard & follow-up gating:** Verified 6/6 non-speech clips (silence, clicks, 220 Hz bumps, breaths, static bursts, low rumble) return `""` instead of `"Thank you."`, while real spoken `"Thank you."` down to `-50 dBFS` still transcribes cleanly. Follow-up turns gate on the woken speaker's level and consume the window after one turn.
- **Agent streaming (`jimmy talk`):** Verified live token-by-token streaming on both Codex (`codex app-server --stdio`) and Claude (`--output-format stream-json --verbose --include-partial-messages`) end to end via `scripts/tests/talk.py`.
- **WebSocket server:** Verified `GET /health`, origin enforcement, `Sec-WebSocket-Protocol` token scoping (rejected on HTTP endpoints, accepted on `/v1/stream`), 80-frame binary audio bursts + priority `cancel` without disconnect, and `0.0 ms` CPU over 1.0 s across 8 idle WebSocket sessions (condvar-blocked workers).
- **Web package (`jimmy-web` 0.1.1):** Rebuilt WASM core (`jimmy_web.wasm`) and TypeScript bundle; all 3 Node wire/client tests passed (including eager `speakStop` drop).

## Remaining 1.0 acceptance

- The desktop load run missed both latency targets. Single-session first-audio latency also exceeded its target.
- The full 24-hour TV and podcast soak, quiet-room microphone recall, and physical-device cancel-to-silence test need recorded or physical test inputs.
- Raspberry Pi 5, macOS, and native Windows runtime checks remain open. The release matrix is configured for those build families, with Linux ARM64 on a hosted runner rather than a Pi.
- Linux artifacts use glibc and ALSA. The fully static musl distribution remains open.
- The pinned Cactus ABI accepts language, keyword hints, and word timestamps. It has no per-pass decoder-depth setting. Adaptive energy VAD was exercised in the wake smoke and needs comparison on the full soak corpus.
- Release archives, checksums, SBOMs, and an npm tarball are local. Registry publication and release signing remain open.

## Standalone wake replies

The default Rust workspace now passes 13 tests. The real-model WebSocket test re-transcribed the first two audible replies as “What's up?” and “How can I help?” It also checked the silence timeout, follow-up without a wake phrase, same-breath commands, and the opt-out. The local Chromium Worker synthesized a standalone reply and transcribed the same-breath command. The native listener opened with both models on ALSA's null device. Physical microphone and speaker playback remain unverified.

## `jimmy talk`

Seven unit tests cover the agent link. They check that a shell command receives the transcript and history and its output is spoken, and that a program after `--` runs without a shell. They also cover the spoken error for a failing command, the “Hmm” filler and its cutoff, cancellation when new speech starts, and an OpenAI-compatible streaming server that receives the conversation history.

`scripts/tests/talk.py` ran the release binary through Jimmy's native capture and playback path. It used two PipeWire null sinks and an ALSA config that reaches only those sinks. Synthesized speech went into the virtual microphone, and the virtual speaker was recorded. Both `jimmy talk` (auto-detected Codex, GPT-6.1 Terra) and `jimmy talk claude` (Haiku) passed. Each run heard the wake question and a follow-up without a wake phrase, and Whistle transcribed the spoken follow-up reply as recalling “grilled cheese sandwich.” The Codex reply audio ended 10 to 16 seconds after each question started playing, and the Claude reply audio ended 12 to 18 seconds after. Both times include the question audio and the full spoken reply. The Ollama and Windows paths were not run.
