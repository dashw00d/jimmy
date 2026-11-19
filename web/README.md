# Jimmy Web

Use Jimmy's local voice engines in a browser Worker, or connect to a Jimmy WebSocket. Both paths send the same control events and PCM frames.

```ts
import { Jimmy } from 'jimmy-web';

const jimmy = new Jimmy({ engine: 'local' });
// For a remote session, use { engine: 'wss://voice.example.com/v1/stream', token }.

jimmy.onEvent(event => {
  if (event.type === 'transcript.final') console.log(event.text);
});

await jimmy.ready;

// Call from an audio button's click handler.
await jimmy.openAudio();
jimmy.listenStart();
jimmy.listenStop();

jimmy.speakAppend('Hello from Jimmy.');
jimmy.speakFlush();
```

Push-to-talk is the default. In wake mode, a standalone “Hey Jimmy” gets a short local greeting after the silence timeout. Set `wake_reply: false` in `configure()` if your application supplies its own greeting. Jimmy then waits up to 3 seconds of silence for a command after the wake phrase and sends an empty `transcript.final` if none comes. Use `jimmy.configure({ mode: 'wake' })` for wake detection or `mode: 'open'` for continuous listening. Set `microphone: false` when opening playback only. To manage audio yourself, subscribe with `onAudio` and send 16 kHz PCM16 through `pushAudio`. `close()` stops playback and capture and releases the connection.

The local engine requires a secure browser context and Wasm SIMD. CPU is the default. Use `ttsDevice: 'webgpu'` to opt in to the vendor's WebGPU TTS engine. AudioWorklet uses shared rings in cross-origin isolated pages and transferred messages otherwise. The local Worker downloads pinned, SHA-256-verified model files. Use `modelBase` to serve the same model files from your own site.

The package contains its Worker, Worklet, and Wasm assets under `dist/`. Serve that directory intact if your bundler does not copy the assets automatically. `workerUrl` and `workletUrl` override the entry points.

## Build from this workspace

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.114 --locked
cd web
npm ci
npm run build
npm test
npm pack
```
