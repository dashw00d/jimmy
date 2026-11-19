#!/usr/bin/env python3
"""Real-model CLI, golden corpus, and Linux CPAL sidecar checks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request

LIBRISPEECH = "https://huggingface.co/datasets/hf-internal-testing/librispeech_asr_dummy/resolve/5be91486e11a2d616f4ec5db8d3fd248585ac07a/clean/validation-00000-of-00001.parquet"


def words(text):
    return re.findall(r"[a-z]+(?:'[a-z]+)?", text.lower().replace("’", "'"))


def errors(reference, hypothesis):
    a, b = words(reference), words(hypothesis)
    row = list(range(len(b) + 1))
    for i, expected in enumerate(a, 1):
        next_row = [i]
        for j, actual in enumerate(b, 1):
            next_row.append(min(row[j] + 1, next_row[-1] + 1, row[j - 1] + (expected != actual)))
        row = next_row
    return row[-1], len(a)


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("binary", type=Path)
    cli.add_argument("models", type=Path)
    cli.add_argument("--parquet", type=Path)
    cli.add_argument("--max-word-errors", type=int, default=119)
    cli.add_argument("--skip-pipe", action="store_true")
    args = cli.parse_args()
    binary = [str(args.binary.resolve()), "--model-dir", str(args.models.resolve())]

    def run(*argv, **kwargs):
        return subprocess.run([*binary, *map(str, argv)], check=True, capture_output=True, timeout=60, **kwargs).stdout

    with tempfile.TemporaryDirectory(prefix="jimmy-tests-") as temp:
        directory = Path(temp)
        print(run("doctor", "--json").decode().strip(), flush=True)
        clip = directory / "chime.wav"
        expected_text = "The weather is clear this morning."
        run("say", expected_text, "-o", clip)
        heard = json.loads(run("transcribe", clip, "--json"))
        assert words(heard["text"]) == words(expected_text), heard
        assert heard["language"] == "en" and all(word["end"] >= word["start"] for word in heard["words"]), heard
        assert run("say", "Hello Jimmy.", "-o", "-").startswith(b"RIFF")
        print("CLI WAV round trip passed", flush=True)

        if not args.skip_pipe:
            alsa = directory / "alsa.conf"
            alsa.write_text("pcm.!default { type null }\n")
            script = [
                {"type": "session.configure", "mode": "ptt"},
                {"type": "future.event"},
                {"type": "listen.start"},
                {"type": "listen.stop"},
                {"type": "cue.play", "id": "chime"},
                {"type": "speak.append", "text": "Hello, world."},
                {"type": "speak.flush"},
            ]
            output = run("pipe", "--cue-dir", directory, input="".join(json.dumps(event) + "\n" for event in script).encode(),
                         env={**os.environ, "ALSA_CONFIG_PATH": str(alsa)})
            events = [json.loads(line) for line in output.splitlines()]
            types = [e["type"] for e in events]
            assert types == ["session.ready", "speech.start", "speech.end", "transcript.final", "cue", "speak.start", "speak.end"], events
            assert events[-1]["reason"] == "done", events
            print("JSONL conformance passed with ALSA null capture and playback", flush=True)

        import pyarrow.parquet as pq

        corpus = args.parquet or directory / "validation.parquet"
        if not corpus.is_file():
            urllib.request.urlretrieve(LIBRISPEECH, corpus)
        misses = count = 0
        samples = 0
        for batch in pq.ParquetFile(corpus).iter_batches(batch_size=32, columns=["id", "audio", "text"]):
            for row in batch.to_pylist():
                subprocess.run(
                    ["ffmpeg", "-nostdin", "-v", "error", "-i", "pipe:0", "-f", "wav", "-ac", "1", "-ar", "16000", str(directory / "gold.wav"), "-y"],
                    input=row["audio"]["bytes"], check=True, capture_output=True, timeout=10,
                )
                heard = json.loads(run("transcribe", directory / "gold.wav", "--json"))
                m, n = errors(row["text"], heard["text"])
                misses += m
                count += n
                samples += 1
        rate = misses / count
        print(f"Golden LibriSpeech {samples} clips, {misses}/{count} word errors ({rate:.2%}), SHA-256 {hashlib.sha256(corpus.read_bytes()).hexdigest()}")
        assert misses <= args.max_word_errors, (misses, args.max_word_errors)


if __name__ == "__main__":
    main()
