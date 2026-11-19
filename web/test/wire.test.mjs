import { test } from 'node:test';
import { deepEqual, throws } from 'node:assert/strict';
import { decodeAudio } from '../dist/index.js';
import { makeRing, Ring } from '../dist/ring.js';

test('PCM is decoded with little-endian turn, sequence, and samples', () => {
  const bytes = Uint8Array.from([7, 0, 0, 0, 3, 0, 0, 0, 0, 128, 255, 127]);
  deepEqual(decodeAudio(bytes.buffer), { turn: 7, seq: 3, pcm: Int16Array.from([-32768, 32767]) });
  throws(() => decodeAudio(new ArrayBuffer(7)));
  throws(() => decodeAudio(new ArrayBuffer(9)));
});

test('the shared ring wraps and discards without overwriting unread samples', () => {
  const memory = makeRing(4);
  const writer = new Ring(memory);
  const reader = new Ring(memory);
  deepEqual(writer.write(new Int16Array([1, 2, 3, 4, 5])), 4);
  const out = new Int16Array(3);
  deepEqual(reader.read(out), 3);
  deepEqual([...out], [1, 2, 3]);
  deepEqual(writer.write(new Int16Array([5, 6, 7])), 3);
  deepEqual(reader.read(out), 3);
  deepEqual([...out], [4, 5, 6]);
  reader.discard();
  deepEqual(reader.read(out), 0);
});
