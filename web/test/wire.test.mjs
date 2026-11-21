import { test } from 'node:test';
import { deepEqual, throws } from 'node:assert/strict';
import { Jimmy, decodeAudio } from '../dist/index.js';
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

test('speakStop drops stale audio eagerly and sends speak.stop', async () => {
  const sent = [];
  const originalWs = globalThis.WebSocket;
  class FakeWebSocket {
    static OPEN = 1;
    readyState = 1;
    binaryType = 'blob';
    onmessage = null;
    onerror = null;
    onclose = null;
    constructor(url, protocols) {
      this.url = url;
      this.protocols = protocols;
      FakeWebSocket.last = this;
    }
    send(data) { sent.push(data); }
    close() { this.readyState = 3; }
  }
  globalThis.WebSocket = FakeWebSocket;
  try {
    const jimmy = new Jimmy({ engine: 'ws://127.0.0.1:7071/v1/stream' });
    const ws = FakeWebSocket.last;
    ws.onmessage({ data: JSON.stringify({ type: 'session.ready', turn: 0, protocol: 1, models: [], voices: [], sample_rates: [16000, 24000] }) });
    await jimmy.ready;
    const frames = [];
    jimmy.onAudio(frame => frames.push(frame));
    ws.onmessage({ data: JSON.stringify({ type: 'speak.start', turn: 1, sample_rate: 24000, codec: 'pcm16' }) });
    const pkt1 = Uint8Array.from([1, 0, 0, 0, 0, 0, 0, 0, 10, 0]).buffer;
    const pkt2 = Uint8Array.from([1, 0, 0, 0, 1, 0, 0, 0, 20, 0]).buffer;
    ws.onmessage({ data: pkt1 });
    deepEqual(frames.length, 1);
    jimmy.speakStop(1);
    ws.onmessage({ data: pkt2 });
    deepEqual(frames.length, 1, 'audio after speakStop for active turn must be dropped eagerly');
    deepEqual(JSON.parse(sent.at(-1)), { type: 'speak.stop', turn: 1 });
    await jimmy.close();
  } finally {
    globalThis.WebSocket = originalWs;
  }
});
