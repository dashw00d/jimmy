#!/usr/bin/env python3
"""Score a JSONL wake soak against planted timestamps in seconds."""
import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("events", type=Path)
    parser.add_argument("markers", type=Path, help="One known wake-phrase start time in seconds per line")
    parser.add_argument("--minimum-hours", type=float, default=24)
    args = parser.parse_args()
    detections = [json.loads(line) for line in args.events.read_text().splitlines()]
    hours = detections.pop()["duration_seconds"] / 3600
    targets = [float(line) for line in args.markers.read_text().splitlines() if line.strip()]
    pending = set(range(len(detections)))
    misses = 0
    for target in sorted(targets):
        matching = next((i for i in sorted(pending) if target <= detections[i]["seconds"] < target + 3), None)
        if matching is None:
            misses += 1
        else:
            pending.remove(matching)
    assert hours > 0 and targets, "Soak needs audio and at least one marked wake phrase"
    false_accepts = len(pending)
    print(f"{hours:.2f} hours, {false_accepts} false accepts ({false_accepts / hours:.3f}/hr), "
          f"{misses}/{len(targets)} misses ({misses / len(targets):.2%})")
    assert hours >= args.minimum_hours, "Soak duration is too short"
    assert false_accepts / hours < 1 / 8 and misses / len(targets) < 0.05, "Wake targets missed"


if __name__ == "__main__":
    main()
