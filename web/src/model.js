import createCactus from './cactus/cactus.js';
import { PhononTTS } from './phonon/index.js';

const whistleUrl = 'https://huggingface.co/Cactus-Compute/whistle/resolve/88cb00eeb8928ddbcd410b491584f938ba030b07/whistle.cact';
const phononUrl = 'https://huggingface.co/gradium/phonon-31m-en/resolve/92741c5b07d085ff840e26fea8ce43110b87852f/';
const files = {
  'whistle.cact': 'b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb',
  'phonon/config.json': 'b5d0f88353638370d94035b7d73d7d6577c0554d78f8d7ecfd40490aae39ae85',
  'phonon/default-voice.safetensors': 'dc537478eaadc1a5257fcda2e3501ab81e14c111a5d95a5838f350db2afd0f1a',
  'phonon/tokenizer.json': '691a00068fa7b989f2f3e5ae5daa2d1a5bb319f2d5360deafcfb19a8bb6e7936',
  'phonon/model.q8.gguf': 'a7d9aad2cd6dc66473f24a08bc6d2939030ef4d751ef82349361fbef3272dbae',
};

async function checked(name, base) {
  const url = base ? new URL(name, base) : name === 'whistle.cact' ? whistleUrl : phononUrl + name.slice(7);
  const response = await fetch(url, { signal: AbortSignal.timeout(120_000) });
  if (!response.ok) throw new Error(`Fetch ${name}: HTTP ${response.status}`);
  const bytes = await response.arrayBuffer();
  const hash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)),
    byte => byte.toString(16).padStart(2, '0')).join('');
  if (hash !== files[name]) throw new Error(`SHA-256 mismatch for ${name}`);
  return bytes;
}

export async function loadEngines({ modelBase, ttsDevice = 'cpu' }) {
  const [cactus, whistle, config, tokenizer, voice, weights] = await Promise.all([
    fetch(new URL('./cactus/cactus.wasm', import.meta.url)).then(async response => {
      if (!response.ok) throw new Error(`Fetch Cactus WebAssembly: HTTP ${response.status}`);
      return createCactus({ wasmBinary: await response.arrayBuffer() });
    }),
    checked('whistle.cact', modelBase),
    checked('phonon/config.json', modelBase),
    checked('phonon/tokenizer.json', modelBase),
    checked('phonon/default-voice.safetensors', modelBase),
    checked('phonon/model.q8.gguf', modelBase),
  ]);
  const weightPtr = cactus._malloc(whistle.byteLength);
  if (!weightPtr) throw new Error('Cactus allocation failed');
  cactus.HEAPU8.set(new Uint8Array(whistle), weightPtr);
  if (cactus._cactus_load(weightPtr, BigInt(whistle.byteLength)) < 0) {
    throw new Error(cactus.UTF8ToString(cactus._cactus_last_error()));
  }
  cactus._cactus_set_threads(1);
  const capacity = 256 * 1024;
  const output = cactus._malloc(capacity);
  const input = cactus._malloc(30 * 16000 * 4);
  if (!input || !output) throw new Error('Cactus allocation failed');
  const stt = (audio, language, keywords) => {
    if (!audio.length) return '{"text":"","language":"","words":[]}';
    if (audio.length > 30 * 16000) throw new Error('Whistle input exceeds 30 seconds');
    const pcm = new Float32Array(cactus.HEAPU8.buffer, input, audio.length);
    for (let i = 0; i < audio.length; i++) pcm[i] = audio[i] / 32768;
    const result = cactus.ccall('cactus_transcribe', 'number',
      ['number', 'number', 'string', 'string', 'number', 'number', 'number'],
      [input, audio.length, language, keywords || null, 1, output, capacity]);
    if (result < 0) throw new Error(cactus.UTF8ToString(cactus._cactus_last_error()));
    return cactus.UTF8ToString(output);
  };

  const urls = [weights, tokenizer, config, voice].map(data => URL.createObjectURL(new Blob([data])));
  let phonon;
  try {
    phonon = await PhononTTS.load({
      lang: 'en', device: ttsDevice, threads: 1, cache: false,
      model: {
        weights: { q8: urls[0] }, tokenizer: urls[1], config: urls[2],
        voices: { default: urls[3] }, defaultVoice: 'default',
      },
    });
  } finally {
    for (const url of urls) URL.revokeObjectURL(url);
  }
  const streams = new Map();
  let nextId = 0;
  // Allow one outstanding vendor frame per stream. Promise completion never
  // mutates Rust's state and cancelled generations cannot refill a new stream.
  const start = (text, voice) => {
    const stream = phonon.stream(text, { voice: voice ?? undefined });
    stream.done.catch(() => {});
    const id = ++nextId;
    streams.set(id, { stream, iterator: stream[Symbol.asyncIterator](), reading: false });
    return id;
  };
  const pull = id => {
    const job = streams.get(id);
    if (!job) return null;
    if (job.error) throw job.error;
    if (job.result) {
      const result = job.result;
      job.result = undefined;
      if (result.done) return null;
      return Int16Array.from(result.value, sample => Math.max(-32768, Math.min(32767, Math.round(sample * 32768))));
    }
    if (!job.reading) {
      job.reading = true;
      job.iterator.next().then(
        result => { job.result = result; job.reading = false; },
        error => { job.error = error; job.reading = false; },
      );
    }
  };
  const cancel = id => {
    const job = streams.get(id);
    if (job) job.stream.cancel();
    streams.delete(id);
  };
  return { stt, start, pull, cancel, voices: [...phonon.voices] };
}
