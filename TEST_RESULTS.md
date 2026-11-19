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

The reply is the rig's built-in four paragraphs, 60.4 s of speech. "Before" is commit e338c35. A cut is a gap of digital silence with sound in the 60 ms before it, so speech was interrupted. A stretched pause is a gap that follows 60 ms of quiet, so a pause that was there anyway got longer.

@@PLAYBACK@@

The rows with a forced sink size before the change were recorded on an earlier version of the rig whose private server had no realtime scheduling. They fail for a reason scheduling does not touch: the device buffer of 1024 frames is smaller than what the sink takes in one cycle.

A sink that takes 8192 frames a cycle is beyond the buffer Jimmy first asks for. Jimmy notices the device running dry and reopens it with a longer period, but only between replies, which took about five seconds after launch here. A reply that starts before that plays with gaps.

The fix costs latency. Over six replies the first sound came 232 to 453 ms after `speak.append`, against 80 to 246 ms before.

### Stopping a reply

`speak.stop` after 1, 3 and 6 s of audible speech, then "Hey Jimmy. What is the weather like today?" played into the microphone at once. Every stop on both builds ended with `speak.end` and reason `cancelled`, no `speak.start` followed, and nothing played more than a second later.

@@STOP@@

Sound after the stop is measured from the moment the rig wrote `speak.stop` to the last sample above the noise at the virtual speaker, and is late by up to one graph quantum. Stops that landed in a pause of the speech are left out. A real sink adds its own latency: about 140 ms for the Bluetooth SBC sink this was written for.

### Wake phrase baseline

This work did not change voice activity detection, endpointing or wake matching. These numbers are the baseline for the listening rebuild. Each trial is a synthesized "Hey Jimmy.", a pause of 0, 0.3, 0.8, 1.5 or 2.5 s, and one of three commands. The level is the RMS of the speech at Jimmy's input.

| Speech level | Background | Wake detected | Command exact | Word error rate |
| --- | --- | --- | --- | --- |
| -19.5 dBFS | none | 15/15 | 5/15 | 14% |
| -30 dBFS | none | 15/15 | 5/15 | 18% |
| -39.5 dBFS | none | 15/15 | 7/15 | 17% |
| -42.6 dBFS | none | 9/9 | 4/9 | 15% |
| -45.5 dBFS | none | 9/9 | 3/9 | 30% |
| -47.4 dBFS | none | 9/9 | 1/9 | 52% |
| -50 dBFS | none | 3/15 | 0/15 | 87% |
| -19.5 dBFS | steady noise at -40 dBFS | 0/15 | 0/15 | 100% |
| -39.5 dBFS | steady noise at -60 dBFS | 15/15 | 8/15 | 8% |
| -19.5 dBFS | second talker at -40 dBFS | 4/15 | 1/15 | 91% |
| -39.5 dBFS | second talker at -60 dBFS | 11/15 | 6/15 | 38% |

- **Quiet speech.** The energy detector calls a 20 ms frame speech when its RMS is above 100, which is -50.3 dBFS. From -45 dBFS down the quiet ends of words fall under that and commands are cut short. At -50 dBFS the wake phrase is only heard when the command follows in the same breath (3/3); with any pause it is missed (0/12). Whistle alone still transcribed 9 of those 15 commands exactly from the same captured audio, so the recognizer is not what loses them. A USB microphone at a low input volume lands in this range.
- **Steady noise.** Noise above the same floor from the start is taken for speech, and the noise estimate only moves on frames that are not speech. Jimmy then waits for half a second of quiet that never comes. All 15 trials produced no event of any kind.
- **A second talker** 20 dB below the speech cost 11 of 15 wakes at the higher level.
- **Pause length** made no difference at -39.5 dBFS and above: 3/3 at every pause from 0 to 2.5 s.
- **Wake confidence** is the lowest word probability of the matched phrase and ranged from 0.09 to 0.87, with a median of 0.50 at the four levels without background. Nothing compares it to a threshold.
- **False wakes.** None in 3.07 minutes of non-wake speech, which included "Hey, Jamie, did you see the game" and "Jim said the meeting moved".
- **The recognizer on a whole utterance** often drops or mangles the phrase: "Hey Jimmy" was in the transcript of the captured trial 9/15 times at -19.5 dBFS, 4/15 at -30 dBFS and 0/15 at -39.5 dBFS ("Jimmy, what is the weather", "Hajimi open the list"). Live detection works because it transcribes the first second on its own.
- **While Jimmy speaks through a speaker the microphone hears**, it transcribes its own voice: a 60 s reply cost 154 CPU-seconds against 68 without the echo. @@BARGEIN@@

@@WAKELOAD@@

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
