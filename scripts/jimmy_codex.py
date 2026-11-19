#!/usr/bin/env python3
"""Connect Jimmy's voice sidecar to a short, read-only Codex conversation."""
import argparse
import asyncio
from collections import deque
from contextlib import suppress
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
INSTRUCTIONS = (
    "You are the spoken reply for Jimmy. Respond naturally in one or two short sentences. "
    "Return just the words to speak, with no Markdown. This is conversation only. "
    "Do not use tools, access files, or perform actions. Use the recent conversation to answer follow-ups."
)
DEFAULT_MODEL = "gpt-6.1-terra"
REASONING_EFFORT = "medium"
TIMEOUT = 60
FILLER_DELAY = 2


async def stop(process):
    if process.returncode is None:
        with suppress(ProcessLookupError):
            process.kill()
        await process.wait()


class Codex:
    def __init__(self, workdir, model=None):
        self.workdir = Path(workdir)
        self.model = model or DEFAULT_MODEL

    async def reply(self, history, text, on_delta):
        prompt = json.dumps({
            "recent_conversation": list(history),
            "latest_user_message": text,
        }, ensure_ascii=False)
        process = await asyncio.create_subprocess_exec(
            "codex", "app-server", "--stdio",
            "--disable", "memories", "--disable", "shell_tool", "--disable", "unified_exec",
            "--disable", "hooks", "--disable", "plugins",
            "-c", "mcp_servers={}", "-c", 'web_search="disabled"',
            cwd=self.workdir, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            limit=1024 * 1024,
        )

        async def write(message):
            process.stdin.write((json.dumps(message) + "\n").encode())
            await process.stdin.drain()

        async def read():
            line = await process.stdout.readline()
            if not line:
                raise RuntimeError("Codex app server closed the stream")
            return json.loads(line)

        notifications = deque()

        async def request(number, method, params):
            await write({"id": number, "method": method, "params": params})
            while True:
                event = await read()
                if "method" in event and "id" in event:
                    await decline(event)
                elif event.get("id") == number:
                    if "error" in event:
                        raise RuntimeError(event["error"]["message"])
                    return event["result"]
                elif "method" in event:
                    notifications.append(event)

        async def decline(event):
            await write({"id": event["id"], "error": {
                "code": -32601, "message": "Jimmy conversation does not grant tool requests",
            }})

        try:
            async with asyncio.timeout(TIMEOUT):
                await request(1, "initialize", {
                    "clientInfo": {"name": "jimmy_voice", "version": "0.1.0"},
                })
                await write({"method": "initialized"})
                params = {
                    "cwd": str(self.workdir), "ephemeral": True, "sandbox": "read-only",
                    "approvalPolicy": "never", "developerInstructions": INSTRUCTIONS,
                    "model": self.model,
                }
                thread = (await request(2, "thread/start", params))["thread"]["id"]
                result = await request(3, "turn/start", {
                    "threadId": thread, "effort": REASONING_EFFORT,
                    "input": [{"type": "text", "text": prompt}],
                })
                turn = result["turn"]["id"]
                commentary = set()
                streamed = set()
                chunks = []
                while True:
                    event = notifications.popleft() if notifications else await read()
                    method = event.get("method")
                    if method and "id" in event:
                        await decline(event)
                        continue
                    params = event.get("params", {})
                    if params.get("threadId") != thread:
                        continue
                    if method == "turn/completed" and params["turn"]["id"] == turn:
                        finished = params["turn"]
                        if finished["status"] != "completed":
                            raise RuntimeError((finished.get("error") or {}).get("message", finished["status"]))
                        break
                    if params.get("turnId") != turn:
                        continue
                    delta = ""
                    if method in ("item/started", "item/completed"):
                        item = params["item"]
                        if item["type"] == "agentMessage":
                            if item.get("phase") == "commentary":
                                commentary.add(item["id"])
                            elif method == "item/completed" and item["id"] not in streamed:
                                delta = item["text"]
                    elif method == "item/agentMessage/delta" and params["itemId"] not in commentary:
                        streamed.add(params["itemId"])
                        delta = params["delta"]
                    if delta:
                        chunks.append(delta)
                        await on_delta(delta)
                reply = "".join(chunks).strip()
                if not reply:
                    raise RuntimeError("Codex returned an empty reply")
                return reply
        finally:
            await stop(process)


class Conversation:
    def __init__(self, codex, send, report=print):
        self.codex = codex
        self.send = send
        self.report = report
        self.history = deque(maxlen=8)
        self.task = None
        self.speaking = None

    async def cancel_request(self):
        if self.task is not None:
            self.task.cancel()
            with suppress(asyncio.CancelledError):
                await self.task
            self.task = None

    async def handle(self, event):
        kind = event["type"]
        if kind == "speech.start":
            self.speaking = None
            if self.task is not None and not self.task.done():
                await self.cancel_request()
                await self.send({"type": "speak.stop", "turn": event["turn"]})
        elif kind == "transcript.final":
            text = event["text"].strip()
            if text:
                await self.cancel_request()
                self.report(f"You: {text}")
                self.task = asyncio.create_task(self.respond(event["turn"], text))
        elif (kind == "speak.end" and event["reason"] == "done"
              and self.speaking and event["turn"] == self.speaking[0]):
            _, text, reply = self.speaking
            self.speaking = None
            self.history.append({"user": text, "assistant": reply})
        elif kind == "error":
            self.report(f"Jimmy: {event['message']}")

    async def respond(self, turn, text):
        started = time.monotonic()
        filler_started = False
        reading = False

        async def fill_pause():
            nonlocal filler_started
            await asyncio.sleep(FILLER_DELAY)
            filler_started = True
            # Punctuation and trailing space start playback without flushing the turn.
            await self.send({"type": "speak.append", "text": "Hmm. ", "turn": turn})

        filler = asyncio.create_task(fill_pause())

        async def stop_timer():
            filler.cancel()
            with suppress(asyncio.CancelledError):
                await filler

        async def speak_delta(delta):
            nonlocal reading
            events = []
            if not reading:
                if not delta.strip():
                    return
                reading = True
                await stop_timer()
                if filler_started:
                    events.append({"type": "speak.stop", "turn": turn})
                self.report(f"Codex started replying after {time.monotonic() - started:.1f}s")
            events.append({"type": "speak.append", "text": delta, "turn": turn})
            await self.send(*events)

        try:
            reply = await self.codex.reply(self.history, text, speak_delta)
        except (OSError, RuntimeError, TimeoutError) as error:
            self.report(f"Codex: {error or 'The request timed out.'}")
            if not reading:
                await speak_delta("I couldn't get a reply from Codex.")
        else:
            self.report(f"Codex ({time.monotonic() - started:.1f}s): {reply}")
            self.speaking = (turn, text, reply)
        finally:
            await stop_timer()
        await self.send({"type": "speak.flush", "turn": turn})


async def run(args):
    command = [str(args.jimmy.resolve()), "pipe", "--mode", "wake", "--wake-reply=false"]
    if args.model_dir:
        command.extend(["--model-dir", str(args.model_dir.resolve())])
    jimmy = await asyncio.create_subprocess_exec(
        *command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )

    async def send(*events):
        jimmy.stdin.write("".join(json.dumps(event) + "\n" for event in events).encode())
        await jimmy.stdin.drain()

    with tempfile.TemporaryDirectory(prefix="jimmy-codex-") as workdir:
        conversation = Conversation(
            Codex(workdir, args.model), send, report=lambda message: print(message, flush=True),
        )
        try:
            async for line in jimmy.stdout:
                event = json.loads(line)
                if event["type"] == "session.ready":
                    print('Say “Hey Jimmy” and ask a question. Press Ctrl+C to stop.', flush=True)
                await conversation.handle(event)
            return await jimmy.wait()
        finally:
            await conversation.cancel_request()
            await stop(jimmy)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jimmy", type=Path, default=ROOT / "target/release/jimmy")
    parser.add_argument("--model-dir", type=Path, help="Local Whistle and Phonon model directory")
    parser.add_argument("--model", help=f"Codex model override (default: {DEFAULT_MODEL})")
    args = parser.parse_args()
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        return 0
    except OSError as error:
        print(f"Jimmy: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
