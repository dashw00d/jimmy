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
