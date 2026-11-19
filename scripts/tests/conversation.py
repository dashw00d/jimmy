#!/usr/bin/env python3
"""Record a two-turn Jimmy and Codex conversation through the real speech models."""
import argparse
import asyncio
import json
from pathlib import Path
import struct
import sys
import tempfile
import time

from integration import connect, pcm16k, send_voice, server, speech, transcribe, wav24k

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from jimmy_codex import Codex, Conversation


async def converse(port, clips, workdir, model):
    async with connect(f"ws://127.0.0.1:{port}/v1/stream") as ws:
        assert json.loads(await ws.recv())["type"] == "session.ready"
        await ws.send(json.dumps({"type": "session.configure", "mode": "wake", "wake_reply": False}))

        async def send(*events):
            for event in events:
                await ws.send(json.dumps(event))

        conversation = Conversation(Codex(workdir, model), send, report=lambda line: print(line, flush=True))
        spoken = []
        try:
            for index, clip in enumerate(clips):
                await send_voice(ws, clip)
                audio = bytearray()
                saw_wake = False
                first_reply_audio = None
                request_start = None
                filler_cut = False
                async with asyncio.timeout(75):
                    while True:
                        message = await ws.recv()
                        if isinstance(message, bytes):
                            _, sequence = struct.unpack_from("<II", message)
                            if sequence == 0:
                                audio.clear()
                            if first_reply_audio is None:
                                first_reply_audio = time.monotonic()
                            audio.extend(message[8:])
                            continue
                        event = json.loads(message)
                        saw_wake |= event["type"] == "wake"
                        await conversation.handle(event)
                        if event["type"] == "transcript.final":
                            request_start = time.monotonic()
                        if event["type"] == "speak.end":
                            if event["reason"] == "cancelled":
                                filler_cut = True
                                audio.clear()
                                first_reply_audio = None
                                continue
                            assert event["reason"] == "done" and audio, event
                            assert len(conversation.history) == index + 1, "Codex did not finish the reply"
                            assert saw_wake == (index == 0), "The follow-up should work without a wake phrase"
                            assert first_reply_audio is not None and request_start is not None
                            print(f"Answer audio at {first_reply_audio - request_start:.1f}s, "
                                  f"filler interrupted: {filler_cut}", flush=True)
                            spoken.append(wav24k(audio))
                            break
            context = list(conversation.history)
            assert "grilled cheese" in context[0]["user"].lower(), context[0]
            assert "grilled cheese" not in context[1]["user"].lower(), context[1]
            assert "grilled cheese" in context[1]["assistant"].lower(), context[1]
            return spoken
        finally:
            await conversation.cancel_request()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("models", type=Path)
    parser.add_argument("--model", help="Codex model override")
    parser.add_argument("--record-dir", type=Path, help="Save the two spoken replies as WAV files")
    args = parser.parse_args()
    with server(args.binary, args.models) as port, tempfile.TemporaryDirectory(prefix="jimmy-conversation-") as workdir:
        clips = [pcm16k(speech(port, text)) for text in [
            "Hey Jimmy, my favorite snack is a grilled cheese sandwich. Please remember that.",
            "What snack did I ask you to remember?",
        ]]
        replies = asyncio.run(converse(port, clips, workdir, args.model))
        heard = transcribe(port, replies[1])["text"]
        assert "grilled cheese" in heard.lower(), heard
        print(f"Whistle heard the second spoken reply as: {heard}", flush=True)
    if args.record_dir:
        args.record_dir.mkdir(parents=True, exist_ok=True)
        for index, data in enumerate(replies, 1):
            (args.record_dir / f"codex-reply-{index}.wav").write_bytes(data)


if __name__ == "__main__":
    main()
