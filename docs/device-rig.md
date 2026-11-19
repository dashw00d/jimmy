# Device rig

`scripts/tests/device_rig.py` runs `jimmy pipe` through its real microphone and speaker path: CPAL, ALSA, the PipeWire ALSA plugin, resampling, and the pipe loop. It does this on a private audio server, so it is safe to run on a machine that is in use.

The rig starts its own `dbus-daemon`, `pipewire`, `wireplumber` and `pipewire-pulse` in a temporary `XDG_RUNTIME_DIR`, with their own home, settings and state directories. WirePlumber runs with its `policy` profile, which loads no hardware monitors, and the rig gives it no route to the system bus, which is where it would find Bluetooth. So no sound card or Bluetooth device is opened, your default devices and volumes are not read or changed, and nothing is audible. PipeWire and Jimmy keep the system bus, because they use it to ask for realtime scheduling as they do on a desktop. The server holds two virtual devices:

| Device | What it is |
| --- | --- |
| `rig_mic` | The default source. The rig plays WAVs into it and Jimmy captures from it. |
| `rig_speaker` | The default sink. Jimmy plays into it and the rig records its monitor. |

Both are 48 kHz stereo float, as PipeWire gives a USB microphone and a Bluetooth sink.

The rig's recorders ask for a long latency, so they never set the graph's quantum. Jimmy's own streams do, as on a desktop where Jimmy is the only client. `result.json` records the quantum the graph really ran at.

It needs Linux with PipeWire, WirePlumber, pipewire-pulse, `pactl`, `paplay`, `parec`, `pw-loopback`, `pw-metadata`, `pw-dump`, `pw-top`, and Python 3 with NumPy. The models come from `~/.cache/jimmy/models` unless `--model-dir` says otherwise.

## Modes

Every mode takes the binary to test first. Build it, then for example:

```sh
python3 scripts/tests/device_rig.py playback target/release/jimmy --quantum 2048 --out /tmp/rig/playback
```

### `playback`: is a long reply spoken smoothly?

Sends one long `speak.append` (four paragraphs, like an agent's reply) and `speak.flush`, records the virtual speaker, and measures the breaks in the speech.

```sh
python3 scripts/tests/device_rig.py playback BINARY                    # the quantum Jimmy asks for, idle
python3 scripts/tests/device_rig.py playback BINARY --quantum 4096     # a sink that takes 4096 frames a cycle
python3 scripts/tests/device_rig.py playback BINARY --load 16          # with 16 busy processes
python3 scripts/tests/device_rig.py playback BINARY --text reply.txt   # your own text
python3 scripts/tests/device_rig.py playback BINARY --echo-gain 0.2 --echo-delay 0.15   # the microphone hears the speaker
```

With `--echo-gain` Jimmy hears its own voice, as it does with a speaker in the room, and transcribes it while looking for the wake phrase. See `bargein` for the two echo options.

Synthesized pauses are low noise, never digital silence, and the virtual speaker adds no noise of its own. So a run of exact zeros of 3 ms or more is time when Jimmy had nothing to play. The rig sorts those runs by what surrounds them:

- **cuts inside words**: there was sound in the 60 ms before the gap, so speech was interrupted. This is the click and stutter of an underrun.
- **stretched pauses**: it was quiet for 60 ms first. A pause that was there anyway got longer, which is how Jimmy waits for slow synthesis.

It also reports the time to the first sound, how long the speech took, the pauses over 150 ms as a listener hears them, Jimmy's CPU time, and how `speak.end` lines up with the last sound.

### `stop`: does `speak.stop` silence Jimmy, and can it hear straight after?

Starts the long reply, sends `speak.stop` after 1, 3 and 6 seconds of audible speech (`--after 1,3,6`), then plays "Hey Jimmy. What is the weather like today?" into the microphone at once.

```sh
python3 scripts/tests/device_rig.py stop BINARY
python3 scripts/tests/device_rig.py stop BINARY --quantum 4096 --load 16
```

For each stop it reports the reason on `speak.end`, the time from `speak.stop` to that event and to the last sound at the speaker, whether anything played later (queued text must be dropped), and whether the wake and its command were heard.

### `wake`: the wake phrase, a pause, then a command

Speaks "Hey Jimmy.", a pause, and a command into the microphone, across microphone gains, pause lengths and backgrounds, then plays a few minutes of speech with no wake phrase in it and counts false wakes.

```sh
python3 scripts/tests/device_rig.py wake BINARY                         # the full matrix, about 20 minutes
python3 scripts/tests/device_rig.py wake BINARY --gains 1,0.3,0.1,0.03 --backgrounds none --offline
python3 scripts/tests/device_rig.py wake BINARY --load 16 --false-wake-minutes 0
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--gains` | `1,0.3,0.1` | Microphone gain. It scales the speech and its background together, like an input volume. |
| `--pauses` | `0,0.3,0.8,1.5,2.5` | Seconds between the phrase and the command. `0` is both in one breath. |
| `--backgrounds` | `none,noise,speech` | Steady room noise, or a second talker who never says the wake phrase. A background plays on its own stream for the whole block of trials, between them too, as a fan or a television would. |
| `--background-db` | `-15` | Background level against the wake phrase. |
| `--repeats` | `3` | Commands per condition. |
| `--false-wake-minutes` | `3` | Length of the non-wake speech. |
| `--offline` | off | Also runs `jimmy transcribe` on what the microphone captured in each trial, background included. If the recording is recognized but the live trial is not, the recognizer is fine and the listening (voice activity, endpointing) lost it. |
| `--load` | `0` | Busy processes to run meanwhile. |

The table it prints has one row per condition: the speech level in dBFS at Jimmy's input, wakes detected, commands transcribed exactly, the word error rate, and the delay from the end of the command to the final transcript.

### `score`: rates over your own recordings

Plays the WAVs of a manifest into the microphone one by one and reports wake, transcript and false-wake rates. Use this for recorded speech, a recorded room, or a television.

```json
[
  {"wav": "ryan-weather.wav", "wake": true, "text": "what is the weather like today"},
  {"wav": "ryan-quiet.wav", "wake": true},
  {"wav": "podcast-5min.wav", "wake": false}
]
```

Paths are relative to the manifest. `wake` says whether the clip holds the wake phrase (default true). `text` is the command expected after it; leave it out to score the wake only. Clips with `"wake": false` count toward false wakes per hour.

```sh
python3 scripts/tests/device_rig.py score BINARY clips.json
python3 scripts/tests/device_rig.py score BINARY clips.json --gain 0.3 --background noise --background-db -10
python3 scripts/tests/device_rig.py score BINARY clips.json --background tv.wav
python3 scripts/tests/device_rig.py score BINARY clips.json --while-speaking --echo-gain 0.3 --echo-delay 0.2
```

`--while-speaking` keeps Jimmy reading a reply aloud during the clips. `--echo-gain` and `--echo-delay` are described under `bargein`.

### `bargein`: talk while Jimmy speaks

Starts the long reply, waits some seconds of audible speech, then plays a WAV into the microphone (by default a synthesized "Hey Jimmy, stop reading."). It does this once for each value of `--at` (default `2,3.5,5,6.5,8`), because whether Jimmy hears an interruption depends on where in its own sentence it lands.

```sh
python3 scripts/tests/device_rig.py bargein BINARY                                        # headphones: no echo
python3 scripts/tests/device_rig.py bargein BINARY --echo-gain 0.3 --echo-delay 0.05     # speakers on the desk
python3 scripts/tests/device_rig.py bargein BINARY --echo-gain 0.1 --echo-delay 0.2      # a Bluetooth speaker
python3 scripts/tests/device_rig.py bargein BINARY interruption.wav --at 1,2,3,4,5,6,7,8
```

`--echo-gain` loops the virtual speaker back into the virtual microphone at that linear gain, through `pw-loopback`, with `--echo-delay` seconds of delay. That is Jimmy hearing itself: the gain stands for the room and the microphone's distance from the speaker, the delay for the path and the sink's latency. `echo_measured` in the result is the gain and delay actually found between the two recordings, so you can check the loop did what was asked.

The summary gives the wake rate over the trials, how many replies were interrupted, the delay from the end of the interruption to the wake, and `self_wakes`: wakes Jimmy raised on its own voice before anyone spoke. Each trial also lists what Jimmy made of its own voice (`events_before_bargein`), the transcript, how long sound continued after the wake, and a `reference` block with the times of `speak.start`, the first sound, the interruption, the wake and `speak.end`. A reply that was not interrupted is stopped by the rig so the next trial can start.

### `shell`: the private server by hand

```sh
python3 scripts/tests/device_rig.py shell                       # a shell with the private server as its audio
python3 scripts/tests/device_rig.py shell -- pactl list short sinks
python3 scripts/tests/device_rig.py shell --quantum 4096 -- target/release/jimmy devices
```

Inside it `pactl`, `paplay`, `parec`, `pw-top` and any ALSA program, Jimmy included, talk to the private server. `paplay --device=rig_mic clip.wav` speaks into the microphone and `parec --device=rig_speaker.monitor` hears the speaker.

## Options every mode takes

| Option | Meaning |
| --- | --- |
| `--quantum N` | Forces the graph quantum to N frames. Leave it out and the graph runs at what Jimmy's streams ask for, as it does on a desktop. Forcing 2048 to 8192 stands for a sink that takes that many frames at a time whatever its clients ask, which is how a high-latency sink can behave. |
| `--out DIR` | Where the evidence goes. Default `device-rig-out`. |
| `--model-dir DIR` | The models. Default `~/.cache/jimmy/models`. |
| `--settle SECONDS` | How long Jimmy idles after it starts, before the test begins. Default 1. Jimmy lengthens its device buffer for a sink that keeps running dry, but only between replies, and each step takes two to three seconds. Use `--settle 10` with `--quantum 8192` to measure the adapted state rather than the first seconds after launch. |
| `--stimuli DIR` | Where synthesized clips are kept. Synthesis is not repeatable, so give two runs the same directory to play both builds the same recordings. Default `OUT/stimuli`. |
| `--env NAME=VALUE` | Extra environment for Jimmy. `--env JIMMY_AUDIO_PERIOD=512` runs it with a 512-frame device period; `0` leaves the device's default. May be repeated. |
| `--verbose` | Keeps the private server's directory and logs, and prints where. |

## What a run leaves behind

| File | Content |
| --- | --- |
| `result.json` | The measurements, and the streams Jimmy opened on the private server. `graph` lists each running node with the quantum it ran at and its `errors`: cycles the node missed, which the sink plays as silence whatever the node had ready. `realtime` gives the scheduling class of PipeWire's data thread and of Jimmy's audio threads. |
| `events.jsonl` | Every event Jimmy emitted, with `t`, the rig clock time it arrived. |
| `sent.jsonl` | Every event the rig sent, with `t`. |
| `speaker.wav` | The virtual speaker, 48 kHz mono, from before Jimmy started. |
| `mic.wav` | The virtual microphone as Jimmy captured it, echo included. |
| `reference.wav` | Both on one time base: the microphone on the left, the speaker on the right. This is the near-end and far-end pair an echo canceller needs. |
| `jimmy-stderr.log` | Jimmy's standard error. With the playback log on (below) it has one line per reply. |
| `played/` | The exact stimuli that were played. |
| `stimuli/` | Synthesized clips, kept between runs (see `--stimuli`). |

### Playback reference timing

`result.json` has `recordings.speaker_origin` and `recordings.mic_origin`: the rig clock time of sample 0 of each recording. The time of sample `n` is `origin + n / 48000`, on the same clock as every `t` in `events.jsonl` and `sent.jsonl`. So you can place any event against the sound, and any sound against the microphone. The origins come from the least delayed chunk the recorder received, which makes them late by at most about one quantum (21 ms at 1024 frames).

`bargein` adds a `reference` block to each trial: when `speak.start` arrived, when the first sound left the speaker, the gap between them (Jimmy's output latency through the device), when the interruption started and how long it was, and when the wake and `speak.end` arrived.

The rig sets `JIMMY_PLAYBACK_LOG=1` for Jimmy. With it, `jimmy pipe` and `jimmy talk` print one line to standard error at the end of each reply:

```
jimmy: playback turn=1 end=Done underruns=0 stretched_ms=1175 device_xruns=1 period=2048 rate=48000 speed=1.02 stall_ms=21 reserve_ms=271 capture_lost_ms=0 capture_xruns=0
```

| Field | Meaning |
| --- | --- |
| `underruns` | Times the playback queue ran dry in the middle of speech. |
| `stretched_ms` | Silence added to natural pauses while waiting for synthesis. |
| `device_xruns` | Underruns the output device reported: its own buffer emptied. |
| `period` | The most frames the device asked for at once. |
| `speed` | Synthesis speed against real time. Below 1 it is slower than speaking. |
| `stall_ms` | The longest recent time the thread that feeds the speaker was away, usually in a transcription. |
| `reserve_ms` | The queued speech Jimmy was aiming to keep, from `speed` and `stall_ms`. |
| `capture_lost_ms` | Microphone audio dropped because nothing read it in time. |
| `capture_xruns` | Overruns the capture device reported. |
 The playback counters run from when the output device was last opened, the capture counters from the start of the process.

## Reading the results

- If `realtime.pipewire` is not `RR` or `FF`, the private server could not get realtime scheduling (no rtkit, or no permission). Under `--load` its graph then misses cycles that a desktop's would not. Look at `graph[].errors` before blaming Jimmy for short gaps.
- With an echo, a gap in `speaker.wav` is repeated in `mic.wav` after the echo delay. Count gaps in the speaker recording only.
- Synthesis is not repeatable. Give two builds the same `--stimuli` directory when you compare their wake numbers.
- The machine's other work shows. A run under `--load 16` on a machine that is already busy is harder than the same run on an idle one. Compare builds back to back.

## What the rig cannot show

The virtual devices are perfect: no clock drift, no analogue noise, no gain stage, no radio. A USB microphone's real level and noise floor, a Bluetooth sink's codec latency and its behaviour when the link is poor, and acoustic echo in a real room all need the real hardware. The echo loop is a single delayed copy with no reverberation. Use `score` with recordings made on that hardware to bring some of it in.
