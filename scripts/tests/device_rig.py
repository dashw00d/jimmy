#!/usr/bin/env python3
"""Drives `jimmy pipe` through its real microphone and speaker path on a private audio server.

Linux with PipeWire, WirePlumber and pipewire-pulse only. The rig starts its own
PipeWire, WirePlumber and pipewire-pulse inside a temporary XDG_RUNTIME_DIR with a
private D-Bus session, creates a virtual microphone and a virtual speaker there, and
runs Jimmy against them. Your own audio server, default devices, volumes, Bluetooth
and speakers are never touched, and nothing is audible.

    python3 scripts/tests/device_rig.py playback BINARY   # long read-aloud, gap statistics
    python3 scripts/tests/device_rig.py stop BINARY       # speak.stop latency, then a wake
    python3 scripts/tests/device_rig.py wake BINARY       # wake matrix: level, pause, noise
    python3 scripts/tests/device_rig.py score BINARY MANIFEST   # rates over your own WAVs
    python3 scripts/tests/device_rig.py bargein BINARY    # talk into the mic while Jimmy speaks
    python3 scripts/tests/device_rig.py shell             # a shell inside the private server

Every mode takes --out DIR and leaves there the JSONL Jimmy emitted (with arrival
times), the recorded virtual speaker and microphone, and a result.json.
See docs/device-rig.md for each mode, its options and the files it writes.
"""
import argparse
import array
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import wave

MIC = "rig_mic"
SPEAKER = "rig_speaker"
RATE = 48000
NEEDED = ("dbus-daemon", "pipewire", "wireplumber", "pipewire-pulse", "pactl", "paplay", "parec", "pw-loopback",
          "pw-metadata", "pw-dump", "pw-top")
CONFIGURE = {"type": "session.configure", "mode": "wake", "wake": "hey jimmy", "wake_reply": False}
# An agent's reply as Plexa hands it to "Read aloud": several paragraphs with Markdown.
REPLY = """I looked into why the nightly build was failing, and the short version is that it was not the \
compiler at all. The cache key for the dependency layer included the wall clock, so every run rebuilt \
everything from scratch and then ran out of time.

Here is what I changed. First, the cache key now depends only on the lock file and the toolchain version. \
Second, the test step runs the fast unit tests before the slow integration tests, so a broken change is \
reported within two minutes instead of twenty. Third, I removed the retry loop around the upload step, \
because it was hiding a real permission error.

With those changes the build finished in eleven minutes, down from fifty four. All **three hundred and \
twelve** tests pass. One thing still worries me: the integration tests share a single database, so two \
runs at the same time can step on each other. I would give each run its own schema.

If you agree, I will open a pull request with the cache fix today, and a second one for the database \
change once you have had a look at the approach."""
ALSA = """
</usr/share/alsa/alsa.conf>
pcm.!default {
  type pipewire
  playback_node "-1"
  capture_node "-1"
}
ctl.!default { type pipewire }
"""

WAKE = "Hey Jimmy."
COMMANDS = [
    "What is the weather like today?",
    "Open the list of pull requests.",
    "Summarize the last commit for me.",
    "Run the tests again.",
]
# Speech with no wake phrase in it, for the background talker and the false-wake count.
CHATTER = [
    "The delivery arrived a day early, which never happens, so nobody was at the front desk to sign for it.",
    "We walked along the river until the path ran out, then turned back because the light was going.",
    "Jim said the meeting moved to Thursday, and honestly that works better for everyone on the team.",
    "If the oven is too hot the bread browns before it rises, so start low and turn it up at the end.",
    "She has been learning the cello for two years and can finally play the piece she started with.",
    "The train was full, so I stood by the door and read the same page four times without noticing.",
    "Hey, Jamie, did you see the game last night? I missed the second half completely.",
    "They repainted the hallway a shade of green that looks grey in the morning and blue at night.",
    "Remind me to call the dentist, and to pick up the parcel before the shop closes at six.",
    "Any gym will do, as long as it is open early and I do not have to drive across town to get there.",
    "The recipe says to chill the dough for an hour, but twenty minutes in the freezer does the same.",
    "He keeps a list of every book he has read since nineteen ninety, with a mark out of ten for each.",
]


def die_with_parent():
    """Ask the kernel to stop a child when the rig goes away, however that happens."""
    ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGTERM)


class Server:
    """A private PipeWire graph holding one virtual microphone and one virtual speaker."""

    def __init__(self, quantum=None, verbose=False):
        self.quantum = quantum
        self.verbose = verbose
        self.children = []
        self.root = None
        self.env = None

    def __enter__(self):
        missing = [tool for tool in NEEDED if not shutil.which(tool)]
        if missing:
            sys.exit(f"The device rig needs {', '.join(missing)} (PipeWire, WirePlumber and their tools).")
        self.root = Path(tempfile.mkdtemp(prefix="jimmy-rig-"))
        run = self.root / "run"
        run.mkdir(mode=0o700)
        for name in ("home", "config", "state", "data", "cache", "work"):
            (self.root / name).mkdir()
        (self.root / "asound.conf").write_text(ALSA)
        # Nothing of the caller's session is inherited: no runtime directory, no
        # session bus, no settings and no saved state.
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "LANG": "C.UTF-8",
            "HOME": str(self.root / "home"),
            "XDG_RUNTIME_DIR": str(run),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "PIPEWIRE_RUNTIME_DIR": str(run),
            "PULSE_SERVER": f"unix:{run}/pulse/native",
            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={run}/bus",
            "ALSA_CONFIG_PATH": str(self.root / "asound.conf"),
        }
        if "DBUS_SYSTEM_BUS_ADDRESS" in os.environ:
            self.env["DBUS_SYSTEM_BUS_ADDRESS"] = os.environ["DBUS_SYSTEM_BUS_ADDRESS"]
        # The home directory is private too, so point Jimmy at the models it already has.
        models = os.environ.get("JIMMY_MODEL_DIR") or Path.home() / ".cache/jimmy/models"
        if Path(models).is_dir():
            self.env["JIMMY_MODEL_DIR"] = str(Path(models).resolve())
        try:
            self._daemon("dbus", ["dbus-daemon", "--session", "--nofork", "--nopidfile",
                                  f"--address=unix:path={run}/bus"], run / "bus")
            self._daemon("pipewire", ["pipewire"], run / "pipewire-0")
            # The policy profile links streams and picks defaults but loads no
            # hardware monitors, so no real sound card or Bluetooth device is
            # opened. The session manager also gets no route to the system bus,
            # which is where it would find Bluetooth. PipeWire and Jimmy keep
            # theirs: they use it only to ask for realtime scheduling, as they
            # do on a desktop.
            self._daemon("wireplumber", ["wireplumber", "-p", "policy"], None,
                         {"DBUS_SYSTEM_BUS_ADDRESS": f"unix:path={self.root}/no-system-bus"})
            self._daemon("pipewire-pulse", ["pipewire-pulse"], run / "pulse" / "native")
            self._devices()
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def _daemon(self, name, command, socket, extra_env=None):
        log = open(self.root / f"{name}.log", "wb")
        child = subprocess.Popen(command, env={**self.env, **(extra_env or {})}, cwd=self.root / "work",
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                 preexec_fn=die_with_parent)
        self.children.append((name, child))
        deadline = time.monotonic() + 15
        while socket and not socket.exists():
            if child.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(f"{name} did not start: {(self.root / f'{name}.log').read_text()[-2000:]}")
            time.sleep(0.02)

    def run(self, *command, check=True, **kwargs):
        return subprocess.run([str(part) for part in command], env=self.env, check=check, capture_output=True,
                              text=True, **kwargs).stdout.strip()

    def _devices(self):
        deadline = time.monotonic() + 15
        while True:
            try:
                self.run("pactl", "info")
                break
            except subprocess.CalledProcessError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)
        shape = ["rate=48000", "channels=2", "channel_map=front-left,front-right"]
        self.run("pactl", "load-module", "module-null-sink", f"sink_name={SPEAKER}", *shape,
                 f"sink_properties=device.description={SPEAKER}")
        # A duplex node is a sink the rig plays into and a source Jimmy captures from.
        self.run("pactl", "load-module", "module-null-sink", "media.class=Audio/Duplex", f"sink_name={MIC}",
                 *shape, f"sink_properties=device.description={MIC}")
        self.run("pactl", "set-default-sink", SPEAKER)
        self.run("pactl", "set-default-source", MIC)
        if self.quantum:
            self.run("pw-metadata", "-n", "settings", "0", "clock.force-quantum", str(self.quantum))
        deadline = time.monotonic() + 10
        while True:
            info = self.run("pactl", "info")
            if f"Default Sink: {SPEAKER}" in info and f"Default Source: {MIC}" in info:
                break
            if time.monotonic() > deadline:
                raise RuntimeError(f"The virtual devices did not become the defaults:\n{info}")
            time.sleep(0.1)

    def spawn(self, command, **kwargs):
        """Starts a helper inside the private server."""
        kwargs.setdefault("stdin", subprocess.DEVNULL)
        kwargs.setdefault("cwd", self.root / "work")
        child = subprocess.Popen([str(part) for part in command], env=self.env, preexec_fn=die_with_parent,
                                 **kwargs)
        self.children.append((Path(str(command[0])).name, child))
        return child

    def graph(self):
        """What `pw-top` shows for each running node: its quantum, rate and error count.

        An error is a cycle the node missed. The sink then plays that node's
        share of the cycle as silence, whatever the node had ready.
        """
        text = self.run("pw-top", "-b", "-n", "2", check=False, timeout=20)
        rows = []
        for line in text[text.rfind("S   ID"):].splitlines()[1:]:
            fields = line.split()
            if len(fields) >= 10 and fields[0] == "R" and fields[2].isdigit() and int(fields[2]) > 0:
                rows.append({"node": fields[-1], "quantum": int(fields[2]), "rate": int(fields[3]),
                             "errors": int(fields[8])})
        return rows

    def scheduling(self, pid):
        """The scheduling class of each thread of a process, such as {"data-loop.0": "RR 20"}."""
        text = subprocess.run(["ps", "-L", "-o", "cls=,rtprio=,comm=", "-p", str(pid)], capture_output=True,
                              text=True).stdout
        return {" ".join(fields[2:]): " ".join(fields[:2]) for fields in map(str.split, text.splitlines())
                if len(fields) >= 3}

    def streams(self):
        """Lists the application streams on the private server as (name, media class, state)."""
        rows = []
        for item in json.loads(self.run("pw-dump")):
            props = item.get("info", {}).get("props", {}) if item.get("type", "").endswith("Node") else {}
            if props.get("media.class", "").startswith("Stream/"):
                rows.append((props.get("node.name"), props["media.class"], item["info"].get("state")))
        return rows

    def __exit__(self, *_):
        for _, child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
        for _, child in reversed(self.children):
            try:
                child.wait(5)
            except subprocess.TimeoutExpired:
                child.kill()
        if self.root and not self.verbose:
            shutil.rmtree(self.root, ignore_errors=True)
        elif self.root:
            print(f"Server files kept in {self.root}", file=sys.stderr)


def clock():
    return time.monotonic()


class Recorder:
    """Records one device of the private server as 48 kHz mono and dates every sample.

    `origin` is the rig clock time of sample 0, taken from the least delayed chunk, so
    `origin + index / RATE` is when a sample left the device, late by at most about
    one graph quantum and the scheduling of this script.
    """

    def __init__(self, server, device):
        # A long latency, so the recorders never set the graph's quantum. Jimmy's
        # own streams do, as on a desktop where it is the only client.
        self.process = server.spawn(["parec", f"--device={device}", f"--rate={RATE}", "--channels=1",
                                     "--format=s16le", "--raw", "--latency-msec=200"], stdout=subprocess.PIPE)
        self.samples = array.array("h")
        self.origin = math.inf
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        fd = self.process.stdout.fileno()
        rest = b""
        while chunk := os.read(fd, 1 << 16):
            now = clock()
            chunk = rest + chunk
            rest = chunk[len(chunk) // 2 * 2:]
            with self.lock:
                self.samples.frombytes(chunk[: len(chunk) // 2 * 2])
                self.origin = min(self.origin, now - len(self.samples) / RATE)

    def wait_started(self, timeout=10):
        deadline = clock() + timeout
        while not self.samples:
            if clock() > deadline:
                raise RuntimeError("The recorder got no audio from the private server")
            time.sleep(0.02)

    def time_of(self, index):
        return self.origin + index / RATE

    def index_at(self, when):
        return max(0, round((when - self.origin) * RATE))

    def pcm(self):
        import numpy
        with self.lock:
            return numpy.array(self.samples, dtype=numpy.int16)


def save_wav(path, pcm, rate):
    with wave.open(str(path), "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(pcm.astype("<i2").tobytes())


def load_wav(path):
    """Reads a 16-bit WAV as mono samples and its rate."""
    import numpy
    with wave.open(str(path), "rb") as clip:
        if clip.getsampwidth() != 2:
            sys.exit(f"{path} must be a 16-bit PCM WAV")
        pcm = numpy.frombuffer(clip.readframes(clip.getnframes()), dtype="<i2").astype(numpy.float64)
        if clip.getnchannels() > 1:
            pcm = pcm.reshape(-1, clip.getnchannels()).mean(axis=1)
        return pcm, clip.getframerate()


class Jimmy:
    """A `jimmy pipe` process on the private server, with every event dated on arrival."""

    def __init__(self, server, binary, models, out, extra_env=None):
        self.out = out
        self.events = []
        self.lock = threading.Condition()
        self.log = open(out / "events.jsonl", "w")
        self.sent = open(out / "sent.jsonl", "w")
        server.env_for_jimmy = {**server.env, "JIMMY_PLAYBACK_LOG": "1", **(extra_env or {})}
        self.process = subprocess.Popen(
            [str(binary), "pipe", "--model-dir", str(models)], env=server.env_for_jimmy, cwd=server.root / "work",
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(out / "jimmy-stderr.log", "wb"),
            preexec_fn=die_with_parent)
        server.children.append(("jimmy", self.process))
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            now = clock()
            try:
                event = json.loads(line)
            except ValueError:
                continue
            self.log.write(json.dumps({"t": round(now, 4), **event}) + "\n")
            self.log.flush()
            with self.lock:
                self.events.append((now, event))
                self.lock.notify_all()

    def send(self, **event):
        now = clock()
        self.process.stdin.write((json.dumps(event) + "\n").encode())
        self.process.stdin.flush()
        self.sent.write(json.dumps({"t": round(now, 4), **event}) + "\n")
        self.sent.flush()
        return now

    def wait(self, kind, timeout, since=0.0, **fields):
        """Returns (time, event) for the first event of `kind` after `since`, or None."""
        deadline = clock() + timeout
        seen = 0
        with self.lock:
            while True:
                for when, event in self.events[seen:]:
                    if when >= since and event.get("type") == kind and all(
                            event.get(key) == value for key, value in fields.items()):
                        return when, event
                seen = len(self.events)
                left = deadline - clock()
                if left <= 0 or self.process.poll() is not None:
                    return None
                self.lock.wait(min(left, 0.25))

    def since(self, when, until=math.inf):
        with self.lock:
            return [(at, event) for at, event in self.events if when <= at < until]

    def cpu_seconds(self):
        try:
            fields = Path(f"/proc/{self.process.pid}/stat").read_text().rsplit(")", 1)[1].split()
        except OSError:
            return None
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")

    def close(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
        self.log.close()
        self.sent.close()


class Session:
    """Server, Jimmy and both recorders for one run. Files land in `out`."""

    def __init__(self, args):
        self.args = args
        self.out = Path(args.out)
        self.out.mkdir(parents=True, exist_ok=True)
        self.extra_env = dict(pair.split("=", 1) for pair in args.env)
        self.server = Server(quantum=args.quantum, verbose=args.verbose)
        self.load = []

    def __enter__(self):
        binary = Path(self.args.binary).resolve()
        if not binary.is_file():
            sys.exit(f"No Jimmy binary at {binary}")
        models = Path(self.args.model_dir).resolve()
        self.server.__enter__()
        try:
            # The recorders keep the virtual devices running, as a real card always is.
            self.speaker = Recorder(self.server, f"{SPEAKER}.monitor")
            self.mic = Recorder(self.server, MIC)
            self.jimmy = Jimmy(self.server, binary, models, self.out, self.extra_env)
            if not self.jimmy.wait("session.ready", 120):
                raise RuntimeError("Jimmy did not start: " + (self.out / "jimmy-stderr.log").read_text()[-2000:])
            self.speaker.wait_started()
            self.mic.wait_started()
            self.jimmy.send(**CONFIGURE)
            time.sleep(self.args.settle)
            self.streams = self.server.streams()
            names = [name for name, _, _ in self.streams]
            if not any("jimmy" in (name or "") for name in names):
                raise RuntimeError(f"Jimmy did not open its devices on the private server: {self.streams}")
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def start_load(self, count):
        for _ in range(count):
            self.load.append(self.server.spawn(["sh", "-c", "while :; do :; done"]))

    def stop_load(self):
        for child in self.load:
            child.kill()
        self.load = []

    def play(self, path, wait=True):
        """Plays a WAV into the virtual microphone. Returns the rig clock time it was started."""
        started = clock()
        child = self.server.spawn(["paplay", f"--device={MIC}", "--latency-msec=20", path])
        if wait:
            child.wait()
        return started

    def finish(self, result):
        import numpy
        speaker, mic = self.speaker.pcm(), self.mic.pcm()
        save_wav(self.out / "speaker.wav", speaker, RATE)
        save_wav(self.out / "mic.wav", mic, RATE)
        # Both on one time base: left is what Jimmy heard, right is what it played.
        shift = round((self.speaker.origin - self.mic.origin) * RATE)
        speaker = numpy.concatenate((numpy.zeros(max(shift, 0), numpy.int16), speaker[max(-shift, 0):]))
        length = min(len(mic), len(speaker))
        with wave.open(str(self.out / "reference.wav"), "wb") as both:
            both.setnchannels(2)
            both.setsampwidth(2)
            both.setframerate(RATE)
            both.writeframes(numpy.stack((mic[:length], speaker[:length]), axis=1).astype("<i2").tobytes())
        result["recordings"] = {
            "rate": RATE,
            "speaker_wav": "speaker.wav", "speaker_origin": round(self.speaker.origin, 4),
            "mic_wav": "mic.wav", "mic_origin": round(self.mic.origin, 4),
            "reference_wav": "reference.wav", "reference_origin": round(self.mic.origin, 4),
            "note": "An origin is the rig clock time of sample 0, on the clock of the `t` in events.jsonl "
                    "and sent.jsonl. reference.wav holds the microphone on the left and the speaker on the "
                    "right, lined up on that clock to within one graph quantum.",
        }
        result["jimmy_streams"] = self.streams
        result["quantum"] = self.args.quantum
        result["jimmy_env"] = self.extra_env
        # The quantum the graph really ran at, and the cycles each node missed.
        result["graph"] = self.server.graph()
        result["realtime"] = {
            "pipewire": self.server.scheduling(self.server.children[1][1].pid).get("data-loop.0"),
            # The PipeWire client thread and the two CPAL threads that carry Jimmy's audio.
            "jimmy": {name: policy for name, policy in self.server.scheduling(self.jimmy.process.pid).items()
                      if name.startswith(("data-loop", "cpal"))},
        }
        (self.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")

    def __exit__(self, *_):
        self.stop_load()
        if hasattr(self, "jimmy"):
            self.jimmy.close()
        self.server.__exit__(None, None, None)


def runs(mask):
    """Yields (start, end) of every run of True in a boolean array."""
    import numpy
    edges = numpy.diff(numpy.concatenate(([0], mask.astype(numpy.int8), [0])))
    return zip(numpy.flatnonzero(edges == 1), numpy.flatnonzero(edges == -1))


def level(pcm):
    import numpy
    return float(numpy.sqrt(numpy.mean(pcm.astype(numpy.float64) ** 2))) if len(pcm) else 0.0


def speech_gaps(pcm, rate=RATE):
    """Measures the breaks in recorded speech.

    Synthesized pauses are low noise, never digital silence, and the virtual speaker
    adds none. So a run of exact zeros is time when Jimmy had nothing to play:
      cut    there was sound in the 60 ms before it, so speech was interrupted
             (the click and stutter of an underrun)
      hold   it was quiet for 60 ms first, so a pause that was there anyway got longer
    """
    import numpy
    loud = numpy.flatnonzero(numpy.abs(pcm.astype(numpy.int32)) > 300)
    if not len(loud):
        return {"audible": False}
    first, last = int(loud[0]), int(loud[-1])
    body = pcm[first:last + 1]
    edge = rate * 60 // 1000
    cuts, holds = [], []
    for start, end in runs(body == 0):
        if end - start < rate * 3 // 1000:
            continue
        before = level(body[max(0, start - edge):start])
        after = level(body[end:end + edge])
        entry = {"at": round(start / rate, 3), "ms": round((end - start) * 1000 / rate, 1),
                 "level_before": round(before), "level_after": round(after)}
        (cuts if before > 150 else holds).append(entry)
    # Pauses as a listener hears them: at least 150 ms below speech level.
    frame = rate // 100
    count = len(body) // frame
    quiet = numpy.sqrt(numpy.mean(body[:count * frame].astype(numpy.float64).reshape(count, frame) ** 2, axis=1)) < 150
    pauses = sorted((int(end - start) * 10 for start, end in runs(quiet) if end - start >= 15), reverse=True)

    def summary(items):
        return {"count": len(items), "total_ms": round(sum(item["ms"] for item in items), 1),
                "longest_ms": max((item["ms"] for item in items), default=0)}

    seconds = len(body) / rate
    return {
        "audible": True,
        "first_sample": first, "last_sample": last,
        "seconds": round(seconds, 2),
        "cuts": summary(cuts), "cuts_per_minute": round(len(cuts) * 60 / seconds, 1),
        "holds": summary(holds),
        "pauses_over_150ms": {"count": len(pauses), "total_ms": sum(pauses), "longest_ms": pauses[:5]},
        "cut_list": cuts[:200], "hold_list": holds[:200],
    }


def playback(args):
    """Reads a long reply aloud and measures the breaks in what reaches the speaker."""
    text = Path(args.text).read_text() if args.text else REPLY
    with Session(args) as session:
        jimmy, speaker = session.jimmy, session.speaker
        echo = start_echo(session, args)
        if args.load:
            session.start_load(args.load)
            time.sleep(1.0)
        cpu = jimmy.cpu_seconds()
        asked = jimmy.send(type="speak.append", text=text)
        jimmy.send(type="speak.flush")
        started = jimmy.wait("speak.start", 60, since=asked)
        ended = jimmy.wait("speak.end", args.timeout, since=asked)
        time.sleep(2.0)
        used = jimmy.cpu_seconds() - cpu
        session.stop_load()
        pcm = speaker.pcm()[speaker.index_at(asked):]
        gaps = speech_gaps(pcm)
        result = {
            "mode": "playback", "load_processes": args.load, "echo": echo, "characters": len(text),
            # With an echo Jimmy hears itself: these are the turns it took that for.
            "wakes": sum(event["type"] == "wake" for _, event in jimmy.since(asked)),
            "playback_log": [line for line in (session.out / "jimmy-stderr.log").read_text().splitlines()
                             if line.startswith("jimmy: playback")],
            "speak_start_after_s": started and round(started[0] - asked, 3),
            "speak_end_after_s": ended and round(ended[0] - asked, 3),
            "speak_end_reason": ended and ended[1].get("reason"),
            "jimmy_cpu_seconds": round(used, 1),
            "errors": [event for _, event in jimmy.since(asked) if event.get("type") == "error"],
            "gaps": gaps,
        }
        if gaps["audible"]:
            result["first_sound_after_s"] = round(gaps["first_sample"] / RATE, 3)
            result["last_sound_after_s"] = round(gaps["last_sample"] / RATE, 3)
            if ended:
                # Positive when Jimmy reported the end before the speaker went quiet.
                result["sound_after_speak_end_s"] = round(result["last_sound_after_s"] - (ended[0] - asked), 3)
        session.finish(result)
    report_playback(result)
    return result


def report_playback(result):
    gaps = result["gaps"]
    print(f"playback  quantum={result['quantum']}  load={result['load_processes']} busy processes  "
          f"echo={result['echo']}")
    if not gaps["audible"]:
        print("  FAIL: nothing reached the virtual speaker")
        return
    print(f"  first sound {result['first_sound_after_s']:.2f} s after speak.append, speech lasted {gaps['seconds']:.1f} s, "
          f"Jimmy used {result['jimmy_cpu_seconds']} CPU-seconds")
    print(f"  cuts inside words:    {gaps['cuts']['count']:4d}  ({gaps['cuts_per_minute']}/min), "
          f"total {gaps['cuts']['total_ms']:.0f} ms, longest {gaps['cuts']['longest_ms']:.0f} ms")
    print(f"  stretched pauses:     {gaps['holds']['count']:4d}, total {gaps['holds']['total_ms']:.0f} ms, "
          f"longest {gaps['holds']['longest_ms']:.0f} ms")
    print(f"  pauses over 150 ms:   {gaps['pauses_over_150ms']['count']:4d}, longest {gaps['pauses_over_150ms']['longest_ms']} ms")
    print(f"  speak.end: {result['speak_end_reason']} at {result['speak_end_after_s']} s; "
          f"sound continued {result.get('sound_after_speak_end_s')} s after it")
    for line in result["playback_log"]:
        print(f"  {line}")


def words(text):
    return re.findall(r"[a-z0-9]+(?:'[a-z]+)?", text.lower().replace("’", "'"))


def word_errors(expected, heard):
    """Word edit distance and the number of expected words.

    Whistle sometimes splits a word ("to day"). That is the same words heard,
    so text that matches with the spaces taken out counts as no error.
    """
    a, b = words(expected), words(heard)
    if "".join(a) == "".join(b):
        return 0, len(a)
    row = list(range(len(b) + 1))
    for i, want in enumerate(a, 1):
        following = [i]
        for j, got in enumerate(b, 1):
            following.append(min(row[j] + 1, following[-1] + 1, row[j - 1] + (want != got)))
        row = following
    return row[-1], len(a)


def decibels(amplitude):
    return round(20 * math.log10(max(amplitude, 1e-9) / 32768), 1)


def trimmed(pcm, rate, margin=0.02):
    """Cuts the near-silent lead and tail off a synthesized clip."""
    import numpy
    loud = numpy.flatnonzero(numpy.abs(pcm) > 250)
    if not len(loud):
        return pcm
    edge = int(margin * rate)
    return pcm[max(0, loud[0] - edge):loud[-1] + edge]


def speech_level(pcm, rate):
    """RMS over the 20 ms frames that hold speech."""
    import numpy
    frame = rate // 50
    count = len(pcm) // frame
    if not count:
        return 0.0
    rms = numpy.sqrt(numpy.mean(pcm[:count * frame].reshape(count, frame) ** 2, axis=1))
    active = rms[rms > max(rms.max() / 10, 1e-9)]
    return float(numpy.sqrt(numpy.mean(active ** 2))) if len(active) else 0.0


def resampled(pcm, rate, to):
    import numpy
    if rate == to:
        return pcm
    points = numpy.arange(int(len(pcm) * to / rate)) * rate / to
    return numpy.interp(points, numpy.arange(len(pcm)), pcm)


def room_noise(length, rate, seed=7):
    """Steady noise with most of its energy low, like a fan or a computer."""
    import numpy
    white = numpy.random.default_rng(seed).standard_normal(length + rate)
    spectrum = numpy.fft.rfft(white)
    spectrum[1:] /= numpy.sqrt(numpy.arange(1, len(spectrum)))
    spectrum[0] = 0
    pink = numpy.fft.irfft(spectrum)[:length]
    return pink / numpy.sqrt(numpy.mean(pink ** 2))


def deaf_env(work):
    """An environment for file-only Jimmy commands, run outside the private server.

    They open no audio device. Should that ever change, ALSA's default here is
    the null device and there is no audio server to find, so they stay silent.
    The empty work directory also keeps a stray jimmy.toml out of it.
    """
    config = Path(work) / "asound.conf"
    config.write_text("pcm.!default { type null }\nctl.!default { type null }\n")
    return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "HOME": str(work),
            "XDG_RUNTIME_DIR": str(work), "ALSA_CONFIG_PATH": str(config)}


class Voice:
    """Stimuli spoken by the binary under test, kept between runs."""

    RATE = 24000

    def __init__(self, args):
        self.binary = Path(args.binary).resolve()
        self.models = Path(args.model_dir).resolve()
        self.cache = Path(args.stimuli or Path(args.out) / "stimuli").resolve()
        self.cache.mkdir(parents=True, exist_ok=True)

    def say(self, text):
        path = self.cache / (hashlib.sha256(text.encode()).hexdigest()[:16] + ".wav")
        if not path.exists():
            with tempfile.TemporaryDirectory(prefix="jimmy-rig-say-") as work:
                subprocess.run([self.binary, "say", text, "-o", path.resolve(), "--model-dir", self.models],
                               check=True, cwd=work, stdin=subprocess.DEVNULL, env=deaf_env(work))
        pcm, rate = load_wav(path)
        assert rate == self.RATE, rate
        return pcm

    def chatter(self, seconds):
        """Non-wake speech at least `seconds` long."""
        import numpy
        parts, total, index = [], 0, 0
        while total < seconds * self.RATE:
            part = trimmed(self.say(CHATTER[index % len(CHATTER)]), self.RATE)
            parts += [part, numpy.zeros(int(0.35 * self.RATE))]
            total += len(part) + len(parts[-1])
            index += 1
        return numpy.concatenate(parts)


def write_stimulus(path, pcm, rate):
    import numpy
    save_wav(path, numpy.clip(numpy.round(pcm), -32768, 32767), rate)
    return len(pcm) / rate


def prepared(speech, rate, gain):
    """Pads a stimulus and applies the microphone gain. Returns it and its speech level in dBFS."""
    import numpy
    pcm = numpy.concatenate((numpy.zeros(int(0.3 * rate)), speech, numpy.zeros(int(0.3 * rate))))
    return pcm * gain, decibels(speech_level(speech, rate) * gain)


class Background:
    """A sound that stays in the room for a whole block of trials.

    It plays into the microphone on its own stream, before the first trial and
    between trials too, because that is what a fan or a second talker does, and
    it is what Jimmy's noise tracking has to live with. `level` is its RMS in
    sample units after the microphone gain.
    """

    def __init__(self, session, kind, level, voice, out):
        import numpy
        self.kind = kind
        self.session = session
        self.child = None
        self.stopped = threading.Event()
        if kind in (None, "none"):
            return
        rate = voice.RATE
        if kind == "noise":
            pcm = room_noise(60 * rate, rate)
        elif kind == "speech":
            pcm = voice.chatter(60)
        else:
            pcm, clip_rate = load_wav(kind)
            pcm = resampled(pcm, clip_rate, rate)
        active = speech_level(pcm, rate) or float(numpy.sqrt(numpy.mean(pcm ** 2))) or 1.0
        self.wav = out / f"background-{Path(kind).stem}.wav"
        write_stimulus(self.wav, pcm * level / active, rate)
        self.level_db = decibels(level)
        threading.Thread(target=self._loop, daemon=True).start()
        time.sleep(3.0)

    def _loop(self):
        while not self.stopped.is_set():
            self.child = self.session.server.spawn(["paplay", f"--device={MIC}", self.wav])
            self.child.wait()

    def stop(self):
        self.stopped.set()
        if self.child:
            self.child.kill()
            time.sleep(1.5)


def run_trial(session, wav, seconds, expect_wake):
    """Plays one stimulus into the microphone and collects what Jimmy made of it."""
    jimmy = session.jimmy
    started = session.play(wav)
    ended = clock()
    # A bare wake phrase waits 3 s for its command, then the endpoint and the
    # final transcription follow. Allow for a busy machine.
    deadline = ended + (12 if expect_wake else 4)
    while clock() < deadline:
        events = jimmy.since(started)
        kinds = [event["type"] for _, event in events]
        woke = "wake" in kinds
        if woke and "transcript.final" in kinds:
            break
        if not woke and clock() > ended + 4:
            break
        time.sleep(0.05)
    # Let a late event land, and leave the 500 ms of quiet Jimmy wants between utterances.
    quiet_since = clock()
    while clock() - quiet_since < 1.2:
        latest = jimmy.since(started)
        if latest and latest[-1][0] > quiet_since:
            quiet_since = latest[-1][0]
        time.sleep(0.05)
    events = jimmy.since(started)
    wakes = [(at, event) for at, event in events if event["type"] == "wake"]
    finals = [(at, event) for at, event in events if event["type"] == "transcript.final"]
    return {
        "wav": str(wav), "played_at": round(started, 3), "seconds": round(seconds, 2),
        "wakes": len(wakes),
        "wake_after_start_s": round(wakes[0][0] - started, 2) if wakes else None,
        "wake_confidence": wakes[0][1].get("confidence") if wakes else None,
        "transcript": " ".join(event.get("text", "") for _, event in finals).strip(),
        "final_after_end_s": round(finals[-1][0] - (started + seconds), 2) if finals else None,
        "errors": [event for _, event in events if event["type"] == "error"],
        "events": [event["type"] for _, event in events],
    }


def rates(trials):
    """Wake, transcript and false-wake rates over scored trials."""
    wanted = [trial for trial in trials if trial["expect_wake"]]
    unwanted = [trial for trial in trials if not trial["expect_wake"]]
    summary = {"trials": len(trials)}
    if wanted:
        woke = [trial for trial in wanted if trial["wakes"]]
        scored = [trial for trial in wanted if trial.get("expect_text")]
        errors = total = exact = 0
        for trial in scored:
            wrong, count = word_errors(trial["expect_text"], trial["transcript"])
            trial["word_errors"] = wrong
            exact += wrong == 0
            errors += min(wrong, count)
            total += count
        delays = sorted(trial["final_after_end_s"] for trial in woke if trial["final_after_end_s"] is not None)
        summary.update({
            "wake_expected": len(wanted), "wake_detected": len(woke),
            "wake_rate": round(len(woke) / len(wanted), 3),
            "transcripts_scored": len(scored), "transcripts_exact": exact,
            "transcript_exact_rate": round(exact / len(scored), 3) if scored else None,
            "word_error_rate": round(errors / total, 3) if total else None,
            "median_final_after_end_s": delays[len(delays) // 2] if delays else None,
        })
    if unwanted:
        minutes = sum(trial["seconds"] for trial in unwanted) / 60
        false = sum(trial["wakes"] for trial in unwanted)
        summary.update({"non_wake_minutes": round(minutes, 2), "false_wakes": false,
                        "false_wakes_per_hour": round(false * 60 / minutes, 1) if minutes else None})
    return summary


def offline_transcript(args, wav):
    """What Whistle alone makes of a stimulus, with no microphone, VAD or endpointing involved."""
    with tempfile.TemporaryDirectory(prefix="jimmy-rig-stt-") as work:
        done = subprocess.run([Path(args.binary).resolve(), "transcribe", Path(wav).resolve(), "--model-dir",
                               Path(args.model_dir).resolve()], capture_output=True, text=True, cwd=work,
                              env=deaf_env(work))
    return done.stdout.strip() if done.returncode == 0 else None


def score(args):
    """Scores wake, transcript and false-wake rates over the WAVs of a manifest."""
    manifest = Path(args.manifest)
    entries = json.loads(manifest.read_text())
    voice = Voice(args)
    out = Path(args.out)
    (out / "played").mkdir(parents=True, exist_ok=True)
    trials = []
    clips = [(entry, *load_wav((manifest.parent / entry["wav"]).resolve())) for entry in entries]
    loudest = max(speech_level(pcm, rate) for _, pcm, rate in clips) or 1000.0
    with Session(args) as session:
        echo = start_echo(session, args)
        background = Background(session, args.background, loudest * args.gain * 10 ** (args.background_db / 20),
                                voice, out / "played")
        speaking = None
        for index, (entry, pcm, rate) in enumerate(clips):
            pcm, level_db = prepared(pcm, rate, args.gain)
            wav = out / "played" / f"{index:03d}-{Path(entry['wav']).stem}.wav"
            seconds = write_stimulus(wav, pcm, rate)
            if args.while_speaking and (speaking is None or not session.jimmy.wait("speak.start", 0, since=speaking)
                                        or session.jimmy.wait("speak.end", 0, since=speaking)):
                speaking = session.jimmy.send(type="speak.append", text=REPLY)
                session.jimmy.send(type="speak.flush")
                session.jimmy.wait("speak.start", 60, since=speaking)
                time.sleep(2.0)
            trial = run_trial(session, wav, seconds, entry.get("wake", True))
            trial.update({"source": entry["wav"], "expect_wake": entry.get("wake", True),
                          "expect_text": entry.get("text"), "speech_dbfs": level_db})
            trials.append(trial)
            print(f"  {entry['wav']}: wakes={trial['wakes']} transcript={trial['transcript']!r}", flush=True)
        background.stop()
        result = {"mode": "score", "gain": args.gain, "background": args.background,
                  "background_db": args.background_db, "while_speaking": args.while_speaking, "echo": echo,
                  "summary": rates(trials), "trials": trials}
        session.finish(result)
    print(json.dumps(result["summary"], indent=2))
    return result


def wake(args):
    """Wake phrase, a pause, then a command, across microphone levels, pauses and backgrounds."""
    import numpy
    voice = Voice(args)
    out = Path(args.out)
    (out / "played").mkdir(parents=True, exist_ok=True)
    rate = voice.RATE
    phrase = trimmed(voice.say(WAKE), rate)
    commands = [(text, trimmed(voice.say(text), rate)) for text in COMMANDS[:args.repeats]]
    joined = [(text, trimmed(voice.say(f"Hey Jimmy, {text[0].lower()}{text[1:]}"), rate))
              for text, _ in commands]
    reference = speech_level(phrase, rate)
    trials = []
    total = len(args.gains) * len(args.backgrounds) * len(args.pauses) * len(commands)
    with Session(args) as session:
        if args.load:
            session.start_load(args.load)
        for gain in args.gains:
            for kind in args.backgrounds:
                background = Background(session, kind, reference * gain * 10 ** (args.background_db / 20),
                                        voice, out / "played")
                for pause in args.pauses:
                    for index, (text, command) in enumerate(commands):
                        # A pause of 0 is the phrase and the command spoken in one breath.
                        speech = joined[index][1] if pause == 0 else numpy.concatenate(
                            (phrase, numpy.zeros(int(pause * rate)), command))
                        pcm, level_db = prepared(speech, rate, gain)
                        wav = out / "played" / f"{len(trials):03d}-gain{gain}-{kind}-pause{pause}.wav"
                        seconds = write_stimulus(wav, pcm, rate)
                        trial = run_trial(session, wav, seconds, True)
                        trial.update({"gain": gain, "background": kind, "pause_s": pause, "expect_wake": True,
                                      "expect_text": text, "speech_dbfs": level_db,
                                      "background_dbfs": getattr(background, "level_db", None)})
                        if args.offline:
                            # Exactly what reached Jimmy's input, background included.
                            mic = session.mic
                            capture = out / "played" / f"{len(trials):03d}-captured.wav"
                            save_wav(capture, mic.pcm()[mic.index_at(trial["played_at"]):
                                                        mic.index_at(trial["played_at"] + seconds + 0.5)], RATE)
                            heard = offline_transcript(args, capture)
                            trial["offline_transcript"] = heard
                            trial["offline_has_wake"] = heard is not None and "hey jimmy" in " ".join(words(heard))
                        trials.append(trial)
                        print(f"  [{len(trials)}/{total}] gain={gain} {kind} pause={pause}s {level_db} dBFS: "
                              f"wakes={trial['wakes']} heard={trial['transcript']!r} want={text!r}", flush=True)
                background.stop()
        false = None
        if args.false_wake_minutes > 0:
            false = []
            for gain in args.gains[:1]:
                pcm, level_db = prepared(voice.chatter(args.false_wake_minutes * 60), rate, gain)
                wav = out / "played" / f"chatter-gain{gain}.wav"
                seconds = write_stimulus(wav, pcm, rate)
                trial = run_trial(session, wav, seconds, False)
                trial.update({"gain": gain, "expect_wake": False, "speech_dbfs": level_db})
                false.append(trial)
                print(f"  non-wake speech, {seconds / 60:.1f} min at gain {gain}: wakes={trial['wakes']}", flush=True)
        session.stop_load()
        table = []
        for gain in args.gains:
            for background in args.backgrounds:
                for pause in [None, *args.pauses]:
                    group = [trial for trial in trials if trial["gain"] == gain and trial["background"] == background
                             and (pause is None or trial["pause_s"] == pause)]
                    if group:
                        row = {"gain": gain, "background": background, "pause_s": "all" if pause is None else pause,
                               "speech_dbfs": group[0]["speech_dbfs"], **rates(group)}
                        if args.offline:
                            row["offline_has_wake"] = sum(trial["offline_has_wake"] for trial in group)
                        table.append(row)
        result = {"mode": "wake", "load_processes": args.load, "background_db": args.background_db,
                  "summary": rates(trials + (false or [])), "by_condition": table, "trials": trials,
                  "non_wake": false}
        session.finish(result)
    print(f"wake  quantum={args.quantum}  load={args.load} busy processes")
    print("  gain  background  pause   level     wake      exact   word errors  final after end")
    for row in table:
        print(f"  {row['gain']:<5} {row['background']:<10} {str(row['pause_s']):<6} {row['speech_dbfs']:>6} dBFS  "
              f"{row['wake_detected']}/{row['wake_expected']:<6} {row['transcripts_exact']}/{row['transcripts_scored']:<5} "
              f"{row['word_error_rate']!s:<12} {row['median_final_after_end_s']} s")
    if false:
        print(f"  non-wake speech: {result['summary'].get('false_wakes')} false wakes in "
              f"{result['summary'].get('non_wake_minutes')} min")
    return result


def last_sound(pcm, start, threshold=300):
    """Index of the last sample above `threshold` at or after `start`, or None."""
    import numpy
    loud = numpy.flatnonzero(numpy.abs(pcm[start:].astype(numpy.int32)) > threshold)
    return start + int(loud[-1]) if len(loud) else None


def stop(args):
    """Stops a long reply part way, then asks for something straight away."""
    import numpy
    voice = Voice(args)
    out = Path(args.out)
    (out / "played").mkdir(parents=True, exist_ok=True)
    text = COMMANDS[0]
    speech = numpy.concatenate((trimmed(voice.say(WAKE), voice.RATE), numpy.zeros(int(0.4 * voice.RATE)),
                                trimmed(voice.say(text), voice.RATE)))
    pcm, _ = prepared(speech, voice.RATE, 1.0)
    wav = out / "played" / "wake-after-stop.wav"
    seconds = write_stimulus(wav, pcm, voice.RATE)
    attempts = []
    with Session(args) as session:
        jimmy, speaker = session.jimmy, session.speaker
        echo = start_echo(session, args)
        if args.load:
            session.start_load(args.load)
        for after in args.after:
            asked = jimmy.send(type="speak.append", text=REPLY)
            jimmy.send(type="speak.flush")
            # Stop `after` seconds into the audible speech, not into the synthesis.
            deadline = clock() + 60
            while last_sound(speaker.pcm(), speaker.index_at(asked)) is None and clock() < deadline:
                time.sleep(0.02)
            heard = clock()
            time.sleep(after)
            stopped = jimmy.send(type="speak.stop")
            ended = jimmy.wait("speak.end", 5, since=asked)
            trial = run_trial(session, wav, seconds, True) if args.wake_after else None
            time.sleep(3.0)
            recorded = speaker.pcm()
            final = last_sound(recorded, speaker.index_at(asked))
            tail = recorded[speaker.index_at(stopped + 1.0):]
            begun = numpy.flatnonzero(numpy.abs(recorded[speaker.index_at(asked):speaker.index_at(heard) + RATE]
                                                .astype(numpy.int32)) > 300)
            attempt = {
                "stopped_after_s": after,
                "first_sound_after_append_ms": len(begun) and round(
                    (speaker.time_of(speaker.index_at(asked) + int(begun[0])) - asked) * 1000),
                "speak_end_reason": ended and ended[1].get("reason"),
                "speak_end_after_stop_ms": ended and round((ended[0] - stopped) * 1000),
                # Late by up to one quantum: see Recorder.
                "sound_after_stop_ms": final is not None and round((speaker.time_of(final) - stopped) * 1000),
                "sound_later_than_1s": bool(len(tail) and numpy.max(numpy.abs(tail.astype(numpy.int32))) > 300),
                "speak_starts_after_stop": sum(event["type"] == "speak.start" for _, event in jimmy.since(stopped)),
            }
            if trial:
                wrong, count = word_errors(text, trial["transcript"])
                attempt.update({"wake_after_stop": bool(trial["wakes"]), "transcript": trial["transcript"],
                                "word_errors": wrong, "words": count,
                                "wake_played_after_stop_ms": round((trial["played_at"] - stopped) * 1000)})
            attempts.append(attempt)
            print(f"  stop after {after}s: {attempt}", flush=True)
        session.stop_load()
        result = {"mode": "stop", "load_processes": args.load, "echo": echo, "attempts": attempts,
                  "playback_log": [line for line in (session.out / "jimmy-stderr.log").read_text().splitlines()
                                   if line.startswith("jimmy: playback")]}
        session.finish(result)
    return result


def start_echo(session, args):
    """Feeds the virtual speaker back into the virtual microphone, as a room or a headset leak would."""
    if not getattr(args, "echo_gain", 0):
        return None
    server = session.server
    before = set(server.run("pactl", "list", "short", "sink-inputs").splitlines())
    server.spawn(["pw-loopback", "--name", "rig_echo", "--capture", SPEAKER, "--playback", MIC,
                  "--capture-props", "stream.capture.sink=true node.passive=true",
                  "--delay", str(args.echo_delay)])
    deadline = clock() + 10
    while True:
        added = set(server.run("pactl", "list", "short", "sink-inputs").splitlines()) - before
        if added:
            break
        if clock() > deadline:
            raise RuntimeError("The echo loopback did not connect")
        time.sleep(0.1)
    for line in added:
        server.run("pactl", "set-sink-input-volume", line.split()[0], f"{args.echo_gain:.4f}")
    return {"gain": args.echo_gain, "delay_s": args.echo_delay}


def measured_echo(speaker, mic, rate=RATE, search=1.0):
    """Finds the delay and gain of the speaker's sound inside the microphone recording."""
    import numpy
    step = 16  # 3 kHz is plenty to place speech
    a = speaker.astype(numpy.float64)[::step]
    b = mic.astype(numpy.float64)[::step]
    length = min(len(a), len(b))
    if length < rate // step:
        return None
    a, b = a[:length], b[:length]
    size = 1 << (2 * length - 1).bit_length()
    correlation = numpy.fft.irfft(numpy.fft.rfft(b, size) * numpy.conj(numpy.fft.rfft(a, size)), size)
    limit = int(search * rate / step)
    lag = int(numpy.argmax(correlation[:limit]))
    energy = float(numpy.dot(a[:length - lag], a[:length - lag]))
    if energy == 0:
        return None
    gain = float(numpy.dot(b[lag:], a[:length - lag])) / energy
    return {"delay_ms": round(lag * step * 1000 / rate), "gain": round(gain, 3)}


def bargein(args):
    """Plays a WAV into the microphone while Jimmy is reading a reply aloud, once for each --at."""
    import numpy
    voice = Voice(args)
    out = Path(args.out)
    (out / "played").mkdir(parents=True, exist_ok=True)
    if args.wav:
        speech, rate = load_wav(args.wav)
    else:
        speech, rate = trimmed(voice.say(f"Hey Jimmy, {args.say}"), voice.RATE), voice.RATE
    pcm, level_db = prepared(speech, rate, args.gain)
    wav = out / "played" / "bargein.wav"
    seconds = write_stimulus(wav, pcm, rate)
    trials = []
    with Session(args) as session:
        jimmy, speaker, mic = session.jimmy, session.speaker, session.mic
        echo = start_echo(session, args)
        measured = None
        for at in args.at:
            asked = jimmy.send(type="speak.append", text=REPLY)
            jimmy.send(type="speak.flush")
            speak_start = jimmy.wait("speak.start", 60, since=asked)
            deadline = clock() + 60
            while last_sound(speaker.pcm(), speaker.index_at(asked)) is None and clock() < deadline:
                time.sleep(0.02)
            heard_at = clock()
            time.sleep(at)
            before = jimmy.since(asked)
            trial = run_trial(session, wav, seconds, True)
            played = trial["played_at"]
            woke = jimmy.wait("wake", 0, since=played)
            ended = jimmy.wait("speak.end", 0, since=asked)
            recorded = speaker.pcm()
            sounding = last_sound(recorded, speaker.index_at(asked))
            # A reply that is still going was not interrupted. Stop it to clear the way for the next trial.
            left_running = not ended
            if left_running:
                jimmy.send(type="speak.stop")
                jimmy.wait("speak.end", 5, since=asked)
            time.sleep(2.5)
            first = speaker.index_at(asked) + int(numpy.flatnonzero(
                numpy.abs(recorded[speaker.index_at(asked):].astype(numpy.int32)) > 300)[0])
            if echo and measured is None:
                begin, until = speaker.index_at(asked), speaker.index_at(played)
                offset = mic.index_at(asked)
                measured = measured_echo(recorded[begin:until], mic.pcm()[offset:offset + until - begin])
            trials.append({
                "at_s": at,
                "played_after_first_sound_s": round(played - heard_at, 2),
                # Self-echo: anything Jimmy made of its own voice before the person spoke.
                "events_before_bargein": [event["type"] for _, event in before
                                          if event["type"] not in ("speak.start", "session.ready")],
                "wakes_before_bargein": sum(event["type"] == "wake" for _, event in before),
                "wake_detected": bool(woke),
                "wake_after_bargein_end_s": woke and round(woke[0] - played - seconds, 2),
                "transcript": trial["transcript"],
                "reply_interrupted": not left_running,
                "speak_end_reason": ended and ended[1].get("reason"),
                "sound_after_wake_ms": woke and sounding is not None and round(
                    (speaker.time_of(sounding) - woke[0]) * 1000),
                # Where the speaker's sound sits against Jimmy's own events, for an echo canceller.
                "reference": {
                    "speak_start_t": speak_start and round(speak_start[0], 4),
                    "first_sound_t": round(speaker.time_of(first), 4),
                    "first_sound_after_speak_start_ms": speak_start and round(
                        (speaker.time_of(first) - speak_start[0]) * 1000),
                    "bargein_t": round(played, 4), "bargein_seconds": round(seconds, 2),
                    "wake_t": woke and round(woke[0], 4),
                    "speak_end_t": ended and round(ended[0], 4),
                },
            })
            print(f"  at {at}s: wake={bool(woke)} heard={trial['transcript']!r} "
                  f"interrupted={not left_running} self-wakes={trials[-1]['wakes_before_bargein']}", flush=True)
        delays = sorted(trial["wake_after_bargein_end_s"] for trial in trials if trial["wake_detected"])
        result = {
            "mode": "bargein", "echo": echo, "echo_measured": measured, "speech_dbfs": level_db,
            "said": str(args.wav) if args.wav else f"Hey Jimmy, {args.say}",
            "summary": {
                "trials": len(trials),
                "wake_detected": sum(trial["wake_detected"] for trial in trials),
                "wake_rate": round(sum(trial["wake_detected"] for trial in trials) / len(trials), 3),
                "replies_interrupted": sum(trial["reply_interrupted"] for trial in trials),
                "self_wakes": sum(trial["wakes_before_bargein"] for trial in trials),
                "median_wake_after_bargein_end_s": delays[len(delays) // 2] if delays else None,
            },
            "trials": trials,
        }
        session.finish(result)
    print(json.dumps({key: result[key] for key in ("echo", "echo_measured", "speech_dbfs", "said", "summary")},
                     indent=2))
    return result


def shell(args):
    with Server(quantum=args.quantum, verbose=args.verbose) as server:
        print(f"Private audio server in {server.root}. Devices: {MIC} (microphone), {SPEAKER} (speaker).\n"
              "Exit the shell to stop it.", file=sys.stderr)
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        subprocess.run(command or [os.environ.get("SHELL", "/bin/sh")], env=server.env)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_subparsers(dest="mode", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--quantum", type=int, help="Force the graph quantum in frames (1024 is the usual "
                        "default; 2048 to 8192 resemble a high-latency sink such as Bluetooth)")
    common.add_argument("--verbose", action="store_true", help="Keep the private server's logs")
    run = argparse.ArgumentParser(add_help=False, parents=[common])
    run.add_argument("binary", type=Path, help="The jimmy binary to test")
    run.add_argument("--model-dir", type=Path, default=Path.home() / ".cache/jimmy/models")
    run.add_argument("--out", type=Path, default=Path("device-rig-out"), help="Where the evidence is written")
    run.add_argument("--settle", type=float, default=1.0, help="Seconds Jimmy idles after it starts, before the "
                     "test begins (default 1). It lengthens its device buffer for an unusual sink only while idle.")
    run.add_argument("--stimuli", type=Path, help="Where synthesized clips are kept; share it between runs to "
                     "play the same recordings to two builds (default: OUT/stimuli)")
    run.add_argument("--env", action="append", default=[], metavar="NAME=VALUE",
                     help="Extra environment for Jimmy, such as JIMMY_AUDIO_PERIOD=512")
    echo = argparse.ArgumentParser(add_help=False)
    echo.add_argument("--echo-gain", type=float, default=0.0, help="Loop the speaker into the microphone at this "
                      "linear gain (0.3 is a loud room, 0.03 a headset leak; 0 is off)")
    echo.add_argument("--echo-delay", type=float, default=0.05, help="Seconds of delay on that loop (0.05 for "
                      "speakers on the desk, 0.15 to 0.25 for Bluetooth)")
    sub = modes.add_parser("playback", parents=[run, echo], help="Long read-aloud and its gap statistics")
    sub.add_argument("--load", type=int, default=0, help="Busy processes to run meanwhile")
    sub.add_argument("--text", type=Path, help="Text file to read aloud instead of the built-in reply")
    sub.add_argument("--timeout", type=float, default=600)
    sub.set_defaults(run=playback)
    def numbers(kind):
        return lambda text: [kind(part) for part in text.split(",") if part]

    sub = modes.add_parser("stop", parents=[run, echo], help="speak.stop latency, then a wake straight after")
    sub.add_argument("--after", type=numbers(float), default=[1.0, 3.0, 6.0],
                     help="Seconds of audible speech before each stop")
    sub.add_argument("--no-wake-after", dest="wake_after", action="store_false")
    sub.add_argument("--load", type=int, default=0)
    sub.set_defaults(run=stop)
    sub = modes.add_parser("wake", parents=[run], help="Wake phrase, pause, command: levels, pauses, backgrounds")
    sub.add_argument("--gains", type=numbers(float), default=[1.0, 0.3, 0.1], help="Microphone gains to try")
    sub.add_argument("--pauses", type=numbers(float), default=[0, 0.3, 0.8, 1.5, 2.5],
                     help="Seconds between the phrase and the command; 0 is one breath")
    sub.add_argument("--backgrounds", type=numbers(str), default=["none", "noise", "speech"])
    sub.add_argument("--background-db", type=float, default=-15.0,
                     help="Background level against the wake phrase, in dB")
    sub.add_argument("--repeats", type=int, default=3, help="Commands per condition (at most 4)")
    sub.add_argument("--false-wake-minutes", type=float, default=3.0)
    sub.add_argument("--offline", action="store_true",
                     help="Also transcribe what the microphone captured in each trial from a file, to tell the "
                          "recognizer from the listening")
    sub.add_argument("--load", type=int, default=0)
    sub.set_defaults(run=wake)
    sub = modes.add_parser("score", parents=[run, echo], help="Wake, transcript and false-wake rates over WAVs")
    sub.add_argument("manifest", type=Path, help='JSON list of {"wav": PATH, "wake": true|false, "text": "..."}')
    sub.add_argument("--gain", type=float, default=1.0, help="Microphone gain applied to every WAV")
    sub.add_argument("--background", default="none", help="none, noise, speech, or a WAV to loop underneath")
    sub.add_argument("--background-db", type=float, default=-15.0)
    sub.add_argument("--while-speaking", action="store_true", help="Keep Jimmy reading a reply aloud meanwhile")
    sub.set_defaults(run=score)
    sub = modes.add_parser("bargein", parents=[run, echo], help="Talk into the microphone while Jimmy speaks")
    sub.add_argument("wav", nargs="?", type=Path, help="What to play; a wake phrase and --say by default")
    sub.add_argument("--say", default="stop reading.")
    sub.add_argument("--at", type=numbers(float), default=[2.0, 3.5, 5.0, 6.5, 8.0],
                     help="Seconds of audible speech before the WAV starts; one trial for each")
    sub.add_argument("--gain", type=float, default=1.0)
    sub.set_defaults(run=bargein)
    sub = modes.add_parser("shell", parents=[common], help="Run a shell or command inside the private server")
    sub.add_argument("command", nargs=argparse.REMAINDER)
    sub.set_defaults(run=shell)
    # Leave through the context managers on a kill, so no audio server is left behind.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    args = parser.parse_args()
    if hasattr(args, "out"):
        try:
            import numpy  # noqa: F401
        except ImportError:
            sys.exit("The device rig needs NumPy to analyse its recordings (pip install numpy).")
        # Helpers run in the private server's own directory.
        args.out = args.out.resolve()
    args.run(args)


if __name__ == "__main__":
    main()
