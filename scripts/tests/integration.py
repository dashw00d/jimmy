#!/usr/bin/env python3
"""Real-model HTTP, WebSocket, and browser Worker acceptance."""
import argparse
import base64
import asyncio
import contextlib
import http.server
import io
import json
from pathlib import Path
from urllib.parse import unquote, urlsplit
import socket
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.request
import wave
import statistics

from websockets.asyncio.client import connect
from websockets.exceptions import InvalidStatus

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = [
    {"type": "session.configure", "mode": "ptt"},
    {"type": "future.event"},
    {"type": "listen.start"},
    {"type": "listen.stop"},
    {"type": "cue.play", "id": "chime"},
    {"type": "speak.append", "text": "Hello, world."},
    {"type": "speak.flush"},
]
TYPES = ["session.ready", "speech.start", "speech.end", "transcript.final", "cue", "speak.start", "speak.end"]


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextlib.contextmanager
def server(binary, models, token=None):
    port = free_port()
    options = ["--token", token] if token else []
    with subprocess.Popen([str(binary.resolve()), "--model-dir", str(models.resolve()), "serve", "--bind",
                           f"127.0.0.1:{port}", *options],
                          text=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE) as process:
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(process.stderr.read())
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                        break
                except OSError:
                    time.sleep(0.025)
            else:
                raise TimeoutError("Jimmy server did not start")
            yield port
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


def request(port, route, data, content_type, token=None, origin=None):
    headers = {"Content-Type": content_type}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if origin:
        headers["Origin"] = origin
    try:
        with urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{route}", data, headers), timeout=30) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def speech(port, text, token=None):
    status, reply = request(port, "/v1/audio/speech", json.dumps({"input": text, "response_format": "wav"}).encode(),
                         "application/json", token)
    assert status == 200 and reply.startswith(b"RIFF"), (status, reply[:200])
    return reply


def pcm16k(wav):
    buff = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-i", "pipe:0", "-f", "s16le", "-ac", "1",
                           "-ar", "16000", "pipe:1"], input=wav, check=True, capture_output=True, timeout=10)
    return buff.stdout


def transcribe(port, wav, token=None):
    boundary = "jimmy-input"
    data = f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="input.wav"\r\nContent-Type: audio/wav\r\n\r\n'.encode()
    data += wav + f"\r\n--{boundary}--\r\n".encode()
    status, reply = request(port, "/v1/audio/transcriptions", data, f"multipart/form-data; boundary={boundary}", token)
    assert status == 200, (status, reply)
    return json.loads(reply)


async def exchange(ws, until, events, audio, timeout=15):
    async with asyncio.timeout(timeout):
        async for message in ws:
            if isinstance(message, bytes):
                turn, seq = struct.unpack_from("<II", message)
                audio.append((turn, seq, message[8:]))
            else:
                event = json.loads(message)
                events.append(event)
                if until(event):
                    return event
    raise AssertionError("Socket closed early")


async def ws_conformance(port):
    async with connect(f"ws://127.0.0.1:{port}/v1/stream") as ws:
        events, audio = [], []
        ready = await exchange(ws, lambda e: e["type"] == "session.ready", events, audio)
        assert ready["protocol"] == 1
        for event in SCRIPT:
            await ws.send(json.dumps(event))
        await exchange(ws, lambda e: e["type"] == "speak.end", events, audio)
        assert [e["type"] for e in events] == TYPES, events
        assert audio and [a[1] for a in audio] == list(range(len(audio)))
        assert events[-1]["reason"] == "done"
        print(f"WebSocket conformance passed ({len(audio)} PCM frames)", flush=True)


async def send_voice(ws, pcm):
    for at in range(0, len(pcm), 3_200):
        await ws.send(pcm[at:at + 3_200])
        await asyncio.sleep(0.018)
    await ws.send(bytes(32_000))


def wav24k(pcm):
    result = io.BytesIO()
    with wave.open(result, "wb") as stream:
        stream.setparams((1, 2, 24_000, 0, "NONE", "not compressed"))
        stream.writeframes(pcm)
    return result.getvalue()


async def wake_greeting(port, wake_pcm, followup_pcm):
    async with connect(f"ws://127.0.0.1:{port}/v1/stream") as ws:
        await ws.recv()
        await ws.send(json.dumps({"type": "session.configure", "mode": "wake"}))

        async def greeting():
            await send_voice(ws, wake_pcm)
            events, audio = [], []
            end = await exchange(ws, lambda event: event["type"] == "speak.end", events, audio)
            assert end["reason"] == "done" and audio, (events, len(audio))
            types = [event["type"] for event in events]
            assert types == ["wake", "speech.start", "speech.end", "speak.start", "speak.end"], events
            return wav24k(b"".join(frame[2] for frame in audio))

        first = await greeting()
        await send_voice(ws, followup_pcm)
        events = []
        followup = await exchange(ws, lambda event: event["type"] == "transcript.final", events, [])
        assert "morning" in followup["text"].lower(), followup
        assert all(e["type"] not in ("wake", "speak.start") for e in events), events
        await ws.send(json.dumps({"type": "session.configure", "followup_ms": 0}))
        second = await greeting()
        await ws.send(json.dumps({"type": "session.configure", "wake_reply": False}))
        await send_voice(ws, wake_pcm)
        events = []
        disabled = await exchange(ws, lambda event: event["type"] == "transcript.final", events, [])
        assert disabled["text"] == "" and not any(e["type"] == "speak.start" for e in events)
        replies = [transcribe(port, reply)["text"] for reply in [first, second]]
        assert "what" in replies[0].lower() and "up" in replies[0].lower(), replies
        assert "help" in replies[1].lower(), replies
        print(f"Standalone wake replies and hands-free follow-up passed: {replies}", flush=True)


async def wake_cancel(port, pcm):
    async with connect(f"ws://127.0.0.1:{port}/v1/stream") as ws:
        events, audio = [], []
        await exchange(ws, lambda e: e["type"] == "session.ready", events, audio)
        await ws.send(json.dumps({"type": "session.configure", "mode": "wake"}))
        await send_voice(ws, pcm)
        final = await exchange(ws, lambda e: e["type"] == "transcript.final", events, audio)
        assert any(e["type"] == "wake" for e in events), events
        assert "jimmy" not in final["text"].lower() and "weather" in final["text"].lower(), final
        partials = [event["committed"] for event in events if event["type"] == "transcript.partial"]
        assert all(b.startswith(a) for a, b in zip(partials, partials[1:])), partials
        print(f"Wake + phrase trim passed: {final['text']!r}, {len(partials)} partials", flush=True)
        turn = final["turn"]
        text = "The forecast is clear and sunny all day, with a gentle breeze from the north. " * 12
        await ws.send(json.dumps({"type": "speak.append", "text": text, "turn": turn}))
        await ws.send(json.dumps({"type": "speak.flush", "turn": turn}))
        await exchange(ws, lambda e: e["type"] == "speak.start", events, audio)
        first = await asyncio.wait_for(ws.recv(), 5)
        assert isinstance(first, bytes) and struct.unpack_from("<I", first)[0] == turn
        start = time.perf_counter()
        await ws.send(json.dumps({"type": "cancel", "turn": turn}))
        stop = await exchange(ws, lambda e: e["type"] == "speak.end", events, audio)
        elapsed = (time.perf_counter() - start) * 1000
        assert stop["turn"] == turn and stop["reason"] == "cancelled", stop
        await ws.send(json.dumps({"type": "speak.append", "text": "This is stale.", "turn": turn}))
        await ws.send(json.dumps({"type": "speak.append", "text": "This is a new turn."}))
        await ws.send(json.dumps({"type": "speak.flush"}))
        replies, frames = [], []
        await exchange(ws, lambda e: e["type"] == "speak.end", replies, frames)
        assert [e["type"] for e in replies] == ["speak.start", "speak.end"], replies
        assert all(f[0] == turn + 1 for f in frames) and frames[0][1] == 0, frames[:1]
        print(f"Cancel + stale turn check passed ({elapsed:.1f} ms to cancel event)", flush=True)


async def socket_load(port, pcm, count=16):
    connected = asyncio.Barrier(count)

    async def session():
        async with connect(f"ws://127.0.0.1:{port}/v1/stream") as ws:
            events, audio = [], []
            await exchange(ws, lambda e: e["type"] == "session.ready", events, audio)
            await connected.wait()
            await ws.send(json.dumps({"type": "listen.start"}))
            for at in range(0, len(pcm), 3_200):
                await ws.send(pcm[at:at + 3_200])
                await asyncio.sleep(0.1)
            t = time.perf_counter()
            await ws.send(json.dumps({"type": "listen.stop"}))
            final = await exchange(ws, lambda e: e["type"] == "transcript.final", events, audio, 45)
            assert "morning" in final["text"].lower(), final
            final_ms = (time.perf_counter() - t) * 1000
            t = time.perf_counter()
            await ws.send(json.dumps({"type": "speak.append", "text": "Good morning, the sky is clear.", "turn": final["turn"]}))
            await ws.send(json.dumps({"type": "speak.flush", "turn": final["turn"]}))
            async with asyncio.timeout(45):
                while not isinstance(await ws.recv(), bytes):
                    pass
            return final_ms, (time.perf_counter() - t) * 1000

    result = await asyncio.gather(*(session() for _ in range(count)))
    final, first = zip(*result)
    p95 = lambda values: statistics.quantiles(values, n=20, method="inclusive")[-1]
    print(f"WebSocket {count} sessions p95 final {p95(final):.1f} ms, first audio {p95(first):.1f} ms", flush=True)
    return p95(final), p95(first)


async def authorization(port):
    try:
        async with connect(f"ws://127.0.0.1:{port}/v1/stream"):
            raise AssertionError("Unauthenticated connection accepted")
    except InvalidStatus as error:
        assert error.response.status_code == 401
    try:
        async with connect(f"ws://127.0.0.1:{port}/v1/stream", origin="https://wrong.example",
                           additional_headers={"Authorization": "Bearer integration-token"}):
            raise AssertionError("Disallowed origin accepted")
    except InvalidStatus as error:
        assert error.response.status_code == 403


class Static(http.server.SimpleHTTPRequestHandler):
    model_dir = None
    isolated = False

    def end_headers(self):
        if self.isolated:
            self.send_header("Cross-Origin-Opener-Policy", "same-origin")
            self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()

    def translate_path(self, path):
        path = unquote(urlsplit(path).path)
        if path.startswith("/models/"):
            filename = Path(path.removeprefix("/models/"))
            if ".." not in filename.parts:
                return str(self.model_dir / filename)
        filename = Path(path.lstrip("/"))
        if ".." in filename.parts:
            return str(ROOT / "web" / "does-not-exist")
        return str(ROOT / "web" / filename)

    def log_message(self, *_args):
        pass


def browser(port, model_dir, launch, wake_pcm, standalone_wake, audio_worklet=False, isolated=False):
    from playwright.sync_api import sync_playwright

    Static.model_dir = model_dir.resolve()
    Static.isolated = isolated
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Static) as site:
        threading.Thread(target=site.serve_forever, daemon=True).start()
        try:
            with sync_playwright() as playwright:
                chrome = playwright.chromium.launch(executable_path=launch, headless=True, args=["--disable-gpu"])
                page = chrome.new_page()
                page.on("pageerror", lambda error: print(f"Browser page error: {error}", flush=True))
                page.on("response", lambda response: print(f"Browser HTTP {response.status}: {response.url}", flush=True)
                        if response.status >= 400 and not response.url.endswith("/favicon.ico") else None)
                page.goto(f"http://127.0.0.1:{site.server_port}/test/harness.html")
                assert page.evaluate("crossOriginIsolated") == isolated
                for engine in [f"ws://127.0.0.1:{port}", "local"]:
                    report = page.evaluate("""async ({ engine, script, worklet }) => {
                      const { Jimmy } = await import('/dist/index.js');
                      const jimmy = new Jimmy({engine, modelBase: '/models/', token: 'integration-token'});
                      let events = [], count = 0;
                      try {
                        let complete;
                        const done = new Promise(resolve => { complete = resolve; });
                        jimmy.onEvent(event => {
                          events.push(event);
                          if (event.type === 'speak.end' || event.type === 'error') complete();
                        });
                        jimmy.onAudio(frame => { count += frame.pcm.length; });
                        await jimmy.ready;
                        if (worklet) await jimmy.openAudio({ microphone: false });
                        for (const event of script) jimmy.send(event);
                        await Promise.race([done, new Promise((_, reject) => setTimeout(() => reject(Error(JSON.stringify(events))), 20000))]);
                        return {events, count};
                      } finally { await jimmy.close(); }
                    }""", {"engine": engine, "script": SCRIPT, "worklet": audio_worklet})
                    assert [e["type"] for e in report["events"]] == TYPES, report
                    assert report["count"] > 10_000 and report["events"][-1]["reason"] == "done", report
                    print(f"Browser {engine.split(':')[0]} conformance passed ({report['count']} PCM samples)", flush=True)
                for pcm, standalone in [(wake_pcm, False), (standalone_wake, True)]:
                    report = page.evaluate("""async ({ bytes }) => {
                  const { Jimmy } = await import('/dist/index.js');
                  const jimmy = new Jimmy({ engine: 'local', modelBase: '/models/' });
                  try {
                    await jimmy.ready;
                    jimmy.configure({ mode: 'wake' });
                    let samples = 0;
                    jimmy.onAudio(frame => { samples += frame.pcm.length; });
                    const result = new Promise((resolve, reject) => {
                      let wake = false;
                      jimmy.onEvent(event => {
                        if (event.type === 'wake') wake = true;
                        if (event.type === 'transcript.final') resolve({ wake, text: event.text });
                        if (event.type === 'speak.end') resolve({ wake, samples, reason: event.reason });
                        if (event.type === 'error') reject(Error(event.message));
                      });
                    });
                    const data = Uint8Array.from(atob(bytes), c => c.charCodeAt(0));
                    const pcm = new Int16Array(data.buffer);
                    for (let at = 0; at < pcm.length; at += 1600) {
                      jimmy.pushAudio(pcm.subarray(at, at + 1600));
                      await new Promise(resolve => setTimeout(resolve, 8));
                    }
                    jimmy.pushAudio(new Int16Array(16000));
                    return await Promise.race([result, new Promise((_, reject) => setTimeout(() => reject(Error('Wake timed out')), 20000))]);
                  } finally { await jimmy.close(); }
                }""", {"bytes": base64.b64encode(pcm).decode()})
                    assert report["wake"], report
                    if standalone:
                        assert report["reason"] == "done" and report["samples"] > 1_000, report
                        print(f"Browser local standalone wake reply passed ({report['samples']} PCM samples)", flush=True)
                    else:
                        assert "weather" in report["text"].lower() and "jimmy" not in report["text"].lower(), report
                        print(f"Browser local Whistle wake and phrase trim passed: {report['text']!r}", flush=True)
                chrome.close()
        finally:
            site.shutdown()


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("binary", type=Path)
    cli.add_argument("models", type=Path)
    cli.add_argument("--browser", type=Path)
    cli.add_argument("--audio-worklet", action="store_true")
    cli.add_argument("--isolated", action="store_true")
    cli.add_argument("--skip-load", action="store_true")
    cli.add_argument("--strict-latency", action="store_true")
    args = cli.parse_args()
    latency = None
    with server(args.binary, args.models) as port:
        reference = "The weather is clear this morning."
        wav = speech(port, reference)
        with wave.open(io.BytesIO(wav), "rb") as stream:
            assert (stream.getframerate(), stream.getnchannels(), stream.getsampwidth()) == (24000, 1, 2)
        result = transcribe(port, wav)
        assert result["text"].lower() == reference.lower() and result["words"], result
        print("HTTP one-shot WAV round trip passed", flush=True)
        asyncio.run(ws_conformance(port))
        wake_pcm = pcm16k(speech(port, "Hey Jimmy, what is the weather today?"))
        standalone_wake = pcm16k(speech(port, "Hey Jimmy."))
        asyncio.run(wake_greeting(port, standalone_wake, pcm16k(wav)))
        asyncio.run(wake_cancel(port, wake_pcm))
        if not args.skip_load:
            latency = asyncio.run(socket_load(port, pcm16k(wav)))
    with server(args.binary, args.models, "integration-token") as port:
        asyncio.run(authorization(port))
        assert request(port, "/v1/audio/speech", b"{}", "application/json")[0] == 401
        assert request(port, "/v1/audio/speech", b"{}", "application/json", "integration-token", "https://wrong.example")[0] == 403
        print("Bearer token and origin rejection passed", flush=True)
        if args.browser:
            browser(port, args.models, str(args.browser), wake_pcm, standalone_wake, args.audio_worklet, args.isolated)
    if args.strict_latency and latency is not None:
        assert latency[0] < 100 and latency[1] < 100, f"Desktop latency budget missed: {latency}"


if __name__ == "__main__":
    main()
