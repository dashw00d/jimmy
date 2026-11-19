#!/usr/bin/env python3
"""Regenerate the README screenshots in docs/screenshots/ from real Jimmy output.

Each image is a rendering of the actual output of a release `jimmy` binary
running against the pinned models. Nothing is mocked: the script runs the
commands, captures stdout and stderr, and renders the text with headless Chrome.

Requirements: a built release binary with models in the cache (`jimmy pull`),
`curl`, `jq`, Python `websockets` and `Pillow`, and Chrome or Chromium.

    python3 scripts/screenshots.py target/release/jimmy [--chrome /usr/bin/google-chrome]
"""
import argparse, asyncio, html, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

PORT = 7071
WIDTH = 1280
FONT_PX = 15
OUT = Path(__file__).resolve().parent.parent / "docs" / "screenshots"

PAGE = """<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0;background:#0d1117}}
.win{{margin:0;width:{width}px;background:#161b22;color:#e6edf3;font:{font}px/1.45 'DejaVu Sans Mono',monospace}}
.bar{{background:#21262d;padding:10px 16px;display:flex;gap:8px;align-items:center;color:#8b949e;font-size:13px}}
.dot{{width:12px;height:12px;border-radius:50%;display:inline-block}}
pre{{margin:0;padding:16px 20px 20px;white-space:pre-wrap;word-break:break-all}}
.p{{color:#7ee787}} .c{{color:#e6edf3;font-weight:bold}} .d{{color:#8b949e}} .o{{color:#a5d6ff}} .i{{color:#d2a8ff}}
</style><div class="win"><div class="bar"><span class="dot" style="background:#ff5f56"></span><span class="dot" style="background:#ffbd2e"></span><span class="dot" style="background:#27c93f"></span><span style="margin-left:10px">{title}</span></div><pre>{body}</pre></div>"""


def esc(s):
    return html.escape(s.rstrip("\n"))


def render(title, parts, name, chrome):
    """parts: list of (css_class, text)."""
    body = "".join(f'<span class="{c}">{esc(t)}</span>\n' for c, t in parts)
    with tempfile.TemporaryDirectory() as tmp:
        page = Path(tmp) / "p.html"
        page.write_text(PAGE.format(width=WIDTH, font=FONT_PX, title=html.escape(title), body=body))
        raw = Path(tmp) / "raw.png"
        subprocess.run([chrome, "--headless=new", "--no-sandbox", "--disable-gpu", "--hide-scrollbars",
                        f"--window-size={WIDTH},2400", f"--screenshot={raw}", page.as_uri()],
                       check=True, capture_output=True)
        from PIL import Image
        im = Image.open(raw).convert("RGB")
        bg = im.getpixel((5, im.height - 5))
        rows = [y for y in range(im.height) if im.getpixel((5, y)) != bg]
        im = im.crop((0, 0, WIDTH, max(rows) + 1 if rows else im.height))
        OUT.mkdir(parents=True, exist_ok=True)
        im.quantize(colors=64, method=Image.Quantize.MEDIANCUT).save(OUT / name, optimize=True)
    print("wrote", OUT / name)


def shell(cmd, env, cwd):
    """Run a command line through bash, returning combined stdout/stderr."""
    r = subprocess.run(["bash", "-c", cmd], env=env, cwd=cwd, capture_output=True, text=True, timeout=300)
    return (r.stdout + r.stderr).rstrip("\n")


def session(cmds, env, cwd):
    parts = []
    for cmd in cmds:
        parts.append(("p", "$ " + cmd))
        out = shell(cmd, env, cwd)
        if out:
            parts.append(("o", out))
    return parts


async def ws_trace():
    import websockets
    sent = [{"type": "session.configure", "mode": "ptt"},
            {"type": "speak.append", "text": "The weather is clear."},
            {"type": "speak.flush"}]
    parts = [("d", "# client <-> ws://127.0.0.1:%d/v1/stream (audio frames summarized)" % PORT)]
    async with websockets.connect(f"ws://127.0.0.1:{PORT}/v1/stream") as ws:
        first = await asyncio.wait_for(ws.recv(), 120)
        parts.append(("o", "<- " + first))
        for ev in sent:
            await ws.send(json.dumps(ev, separators=(",", ":")))
            parts.append(("c", "-> " + json.dumps(ev, separators=(",", ":"))))
        frames = total = 0
        while True:
            msg = await asyncio.wait_for(ws.recv(), 120)
            if isinstance(msg, bytes):
                frames += 1
                total += len(msg)
                continue
            if frames:
                parts.append(("d", f"<- {frames} binary audio frames, {total} bytes (turn id + seq + mono 24 kHz PCM16)"))
                frames = total = 0
            parts.append(("o", "<- " + msg))
            if json.loads(msg)["type"] == "speak.end":
                return parts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jimmy", help="path to a release jimmy binary")
    ap.add_argument("--chrome", default=shutil.which("google-chrome") or shutil.which("chromium") or "chromium")
    args = ap.parse_args()
    binary = Path(args.jimmy).resolve()
    work = Path(tempfile.mkdtemp(prefix="jimmy-shots-"))
    bin_dir = work / "bin"
    bin_dir.mkdir()
    (bin_dir / "jimmy").symlink_to(binary)  # so the prompt shows plain `jimmy`
    cwd = work / "demo"
    cwd.mkdir()
    env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "HOME": str(work), "NO_COLOR": "1"}
    # Keep the real model cache even though HOME points elsewhere.
    env.setdefault("JIMMY_MODEL_DIR", os.environ.get("JIMMY_MODEL_DIR", ""))
    if not env["JIMMY_MODEL_DIR"]:
        probe = subprocess.run([str(binary), "pull", "--json"], capture_output=True, text=True, check=True)
        env["JIMMY_MODEL_DIR"] = json.loads(probe.stdout)["model_dir"]
    server = None
    try:
        render("jimmy: speech in, text out", session([
            'jimmy say "Hello from Jimmy. Everything runs on this machine." -o hello.wav',
            "jimmy transcribe hello.wav",
            "jimmy transcribe hello.wav --json | jq -c '{text, language, words: (.words | length)}'",
        ], env, cwd), "cli-say-transcribe.png", args.chrome)

        render("jimmy --help", session(["jimmy --help | head -16"], env, cwd), "cli-help.png", args.chrome)

        server = subprocess.Popen(["jimmy", "serve", "--bind", f"127.0.0.1:{PORT}"], env=env, cwd=cwd,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        print(server.stdout.readline().strip())  # "Jimmy listening on ..."
        base = f"http://127.0.0.1:{PORT}"
        render("jimmy serve: OpenAI-style HTTP endpoints", [("p", "$ jimmy serve --bind 127.0.0.1:%d" % PORT),
               ("o", "Jimmy listening on 127.0.0.1:%d" % PORT), ("d", "")] + session([
            f"""curl -s {base}/v1/audio/speech -H 'content-type: application/json' -d '{{"input":"The build finished with no errors.","response_format":"wav"}}' -o speech.wav && ls -l speech.wav | awk '{{print $5, $9}}'""",
            f"curl -s {base}/v1/audio/transcriptions -F file=@speech.wav -F response_format=text",
            f"curl -s {base}/v1/audio/transcriptions -F file=@speech.wav | jq -c '{{text, language, words: (.words | length)}}'",
        ], env, cwd), "http-api.png", args.chrome)

        render("jimmy serve: WebSocket streaming", asyncio.run(ws_trace()), "websocket-session.png", args.chrome)
    finally:
        if server:
            server.terminate()
            try:
                server.wait(10)
            except subprocess.TimeoutExpired:
                server.kill()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
