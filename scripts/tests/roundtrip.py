#!/usr/bin/env python3
"""Two hundred deterministic sentences through the public CLI and WAV pipe."""
import argparse
import concurrent.futures
import itertools
from pathlib import Path
import subprocess

from native import errors

SUBJECTS = [
    "The hikers", "Our neighbors", "The children", "The visitors", "The volunteers",
    "The musicians", "Those students", "The athletes", "The farmers", "The artists",
]
PREDICATES = [
    "will meet by the river when the weather clears",
    "are going to read the instructions again",
    "can hear the bells from across the valley",
    "asked about the best way to reach the village",
    "will bring a fresh loaf of bread to dinner",
    "watched the sunset from the little stone bridge",
    "want to learn what happened to the old library",
    "found a quiet place to sit under the trees",
    "decided to leave before the storm arrived",
    "could see a bright light beyond the hills",
    "will look for the missing papers in the morning",
    "are expecting a long day at the market",
    "cleared a place for the new garden",
    "heard a strange sound coming from the empty house",
    "will talk about their plans over breakfast",
    "enjoyed the warm sunshine after the heavy rain",
    "noticed footprints near the edge of the forest",
    "wondered where the small boat had gone",
    "have a good reason to remember this afternoon",
    "will wait until everyone is ready to begin",
]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("models", type=Path)
    parser.add_argument("--jobs", type=int, default=3)
    args = parser.parse_args()
    jimmy = [str(args.binary.resolve()), "--model-dir", str(args.models.resolve())]
    corpus = [f"{a} {b}." for a, b in itertools.product(SUBJECTS, PREDICATES)]

    def round_trip(text):
        wav = subprocess.run([*jimmy, "say", text, "-o", "-"], capture_output=True, check=True, timeout=45).stdout
        heard = subprocess.run([*jimmy, "transcribe", "-"], input=wav, capture_output=True, check=True, timeout=45).stdout.decode()
        return errors(text, heard)

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        misses = count = exact = 0
        for idx, (wrong, words) in enumerate(pool.map(round_trip, corpus), 1):
            misses += wrong
            count += words
            exact += wrong == 0
            if idx % 25 == 0:
                print(f"{idx}/{len(corpus)} round trips, {exact} exact, {misses}/{count} word errors", flush=True)
    rate = misses / count
    print(f"Round trip WER {rate:.2%}")
    assert rate <= 0.10, rate


if __name__ == "__main__":
    main()
