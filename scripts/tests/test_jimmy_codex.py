"""Exercise the filler and text-stream ordering without an account or audio device."""
from pathlib import Path
import asyncio
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from jimmy_codex import Conversation


class ConversationTest(unittest.IsolatedAsyncioTestCase):
    async def test_cut_filler_before_sending_first_delta(self):
        events = []
        filler_sent = asyncio.Event()

        async def send(*batch):
            events.extend(batch)
            if any(event.get("text") == "Hmm. " for event in batch):
                filler_sent.set()

        class Model:
            async def reply(self, _, _text, emit):
                await asyncio.wait_for(filler_sent.wait(), 1)
                await emit("First sentence. ")
                self_test.assertEqual(events[-1]["text"], "First sentence. ")
                self_test.assertFalse(any(e["type"] == "speak.flush" for e in events))
                await emit("Second sentence.")
                return "First sentence. Second sentence."

        self_test = self
        with patch("jimmy_codex.FILLER_DELAY", 0.01):
            await Conversation(Model(), send, report=lambda _: None).respond(7, "Question")
        self.assertEqual([e["type"] for e in events], [
            "speak.append", "speak.stop", "speak.append", "speak.append", "speak.flush",
        ])
        self.assertTrue(all(e["turn"] == 7 for e in events))

    async def test_fast_reply_skips_filler(self):
        events = []

        async def send(*batch):
            events.extend(batch)

        class Model:
            async def reply(self, _, _text, emit):
                await emit("Quick answer.")
                return "Quick answer."

        await Conversation(Model(), send, report=lambda _: None).respond(7, "Question")
        self.assertEqual([e["type"] for e in events], ["speak.append", "speak.flush"])
