#!/usr/bin/env python3
"""Two-turn `jimmy talk` test through Jimmy's real microphone and speaker path.

Linux with PipeWire or PulseAudio only. It creates two virtual devices, points
Jimmy's ALSA default at them, plays synthesized questions into the virtual mic,
and records what Jimmy says. Your real microphone and speakers are not used.

    python3 scripts/tests/talk.py target/release/jimmy            # auto-detected agent
    python3 scripts/tests/talk.py target/release/jimmy claude
    python3 scripts/tests/talk.py target/release/jimmy -- python3 bot.py
"""
import argparse
import array
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import wave

MIC = "jimmy_test_mic"
SPEAKER = "jimmy_test_speaker"
RATE = 16000
ALSA = f"""
pcm_type.pulse {{ lib "{{lib}}" }}
pcm.!default {{
  type asym
  playback.pcm {{ type pulse device "{SPEAKER}" }}
  capture.pcm {{ type pulse device "{MIC}.monitor" }}
}}
"""


def pulse_plugin():
    for root in ("/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib64", "/usr/lib"):
        path = Path(root) / "alsa-lib/libasound_module_pcm_pulse.so"
        if path.is_file():
            return path
    sys.exit("Install the ALSA PulseAudio plugin (libasound2-plugins).")


def pactl(*args):
    return subprocess.run(["pactl", *args], check=True, capture_output=True, text=True).stdout.strip()


class Speaker:
    """Records the virtual speaker and tracks when Jimmy last made sound."""

    def __init__(self):
        self.process = subprocess.Popen(
            ["parec", f"--device={SPEAKER}.monitor", f"--rate={RATE}", "--channels=1", "--format=s16le",
             "--latency-msec=20"],
            stdout=subprocess.PIPE,
        )
        self.samples = array.array("h")
        self.loud_at = 0.0
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while chunk := self.process.stdout.read(640):
            frame = array.array("h", chunk[: len(chunk) // 2 * 2])
            with self.lock:
                self.samples.extend(frame)
                if frame and max(map(abs, frame)) > 600:
                    self.loud_at = time.monotonic()

    def mark(self):
        with self.lock:
            return len(self.samples)

    def wait_quiet(self, since, quiet=1.2, timeout=60):
        """Waits for sound after `since`, then for `quiet` seconds of silence."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            loud = self.loud_at
            if loud > since and time.monotonic() - loud > quiet:
                return
            time.sleep(0.05)
        raise AssertionError("Jimmy did not speak")

    def save(self, start, path):
        with self.lock:
            pcm = self.samples[start:]
        with wave.open(str(path), "wb") as out:
            out.setnchannels(1)
            out.setsampwidth(2)
            out.setframerate(RATE)
            out.writeframes(pcm.tobytes())

    def stop(self):
        self.process.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary", type=Path)
    parser.add_argument("agent", nargs=argparse.REMAINDER, help="Arguments for `jimmy talk`")
    args = parser.parse_args()
    jimmy = str(args.binary.resolve())
    modules = []
    talk = speaker = None
    with tempfile.TemporaryDirectory(prefix="jimmy-talk-") as temp:
        temp = Path(temp)
        alsa = temp / "alsa.conf"
        alsa.write_text(ALSA.replace("{lib}", str(pulse_plugin())))
        env = {**os.environ, "ALSA_CONFIG_PATH": str(alsa)}
        questions = {
            "q1": "Hey Jimmy, my favorite snack is a grilled cheese sandwich. Please remember that.",
            "q2": "What snack did I ask you to remember?",
        }
        for name, text in questions.items():
            subprocess.run([jimmy, "say", text, "-o", temp / f"{name}.wav"], check=True, env=env)
        try:
            for name in (MIC, SPEAKER):
                modules.append(pactl("load-module", "module-null-sink", f"sink_name={name}",
                                     f"sink_properties=device.description={name}"))
            time.sleep(0.5)  # Let the new devices settle before Jimmy opens them.
            speaker = Speaker()
            talk = subprocess.Popen([jimmy, "talk", *args.agent], env=env, stdout=subprocess.PIPE, text=True)
            lines = queue.Queue()
            threading.Thread(target=lambda: [lines.put(line.rstrip("\n")) for line in talk.stdout], daemon=True).start()

            def expect(prefix, timeout):
                deadline = time.monotonic() + timeout
                while (left := deadline - time.monotonic()) > 0:
                    try:
                        line = lines.get(timeout=left)
                    except queue.Empty:
                        break
                    print(f"  {line}", flush=True)
                    if line.startswith(prefix):
                        return line
                raise AssertionError(f"No line starting with {prefix!r}")

            expect("Talking to ", 60)
            time.sleep(1)
            replies = []
            for name in questions:
                start = speaker.mark()
                asked = time.monotonic()
                subprocess.run(["paplay", f"--device={MIC}", temp / f"{name}.wav"], check=True)
                heard = expect("You: ", 30)
                reply = expect("", 150)  # The agent's reply line, such as "Codex: ...".
                assert ": " in reply and not reply.startswith("You: "), reply
                speaker.wait_quiet(asked)
                print(f"  (spoken reply finished {time.monotonic() - asked:.1f}s after the question)", flush=True)
                clip = temp / f"{name}-reply.wav"
                speaker.save(start, clip)
                spoken = subprocess.run([jimmy, "transcribe", clip], check=True, env=env,
                                        capture_output=True, text=True).stdout.strip()
                print(f"  Whistle heard Jimmy say: {spoken}", flush=True)
                replies.append((heard, reply, spoken))
            assert "grilled cheese" in replies[0][0].lower(), replies[0]
            assert "grilled cheese" not in replies[1][0].lower(), "The follow-up should not mention the snack"
            assert "grilled cheese" in replies[1][1].lower(), f"The agent forgot the snack: {replies[1][1]}"
            assert "cheese" in replies[1][2].lower(), f"Jimmy did not say the answer: {replies[1][2]}"
            print("PASS: Jimmy heard both questions without a second wake phrase, and the agent's spoken reply recalled the snack.")
        finally:
            if talk:
                talk.send_signal(signal.SIGINT)
                try:
                    talk.wait(10)
                except subprocess.TimeoutExpired:
                    talk.kill()
            if speaker:
                speaker.stop()
            for module in modules:
                subprocess.run(["pactl", "unload-module", module], check=False)


if __name__ == "__main__":
    main()
