import type { ClientEvent, Configure, ServerEvent, Turn } from './protocol.js';
import { makeRing, Ring } from './ring.js';

export type { ClientEvent, ServerEvent, Configure, Word, Transcript, Turn } from './protocol.js';

export interface JimmyOptions {
  engine: 'local' | `ws://${string}` | `wss://${string}`;
  token?: string;
  /** Absolute model folder URL, or a path relative to the page. */
  modelBase?: string;
  ttsDevice?: 'cpu' | 'webgpu';
  /** Worker entry points can be overridden by applications with custom asset hosting. */
  workerUrl?: string | URL;
  workletUrl?: string | URL;
}

export interface JimmyAudioFrame {
  turn: Turn;
  seq: number;
  pcm: Int16Array;
}

type Ready = Extract<ServerEvent, { type: 'session.ready' }>;
type Listener = (event: ServerEvent) => void;
type AudioListener = (frame: JimmyAudioFrame) => void;

function socketProtocols(token?: string): string[] {
  if (!token) return ['jimmy.v1'];
  const bytes = new TextEncoder().encode(token);
  const encoded = btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join(''))
    .replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  return ['jimmy.v1', `bearer.${encoded}`];
}

export function decodeAudio(packet: ArrayBuffer): JimmyAudioFrame {
  if (packet.byteLength < 8 || packet.byteLength % 2) throw new Error('Invalid Jimmy PCM frame');
  const data = new DataView(packet);
  const pcm = new Int16Array((packet.byteLength - 8) / 2);
  for (let i = 0; i < pcm.length; i++) pcm[i] = data.getInt16(8 + i * 2, true);
  return { turn: data.getUint32(0, true), seq: data.getUint32(4, true), pcm };
}

export class Jimmy {
  readonly ready: Promise<Ready>;
  #resolve!: (ready: Ready) => void;
  #reject!: (error: Error) => void;
  #worker?: Worker;
  #socket?: WebSocket;
  #closed = false;
  #warm = false;
  #controls: ClientEvent[] = [];
  #events = new Set<Listener>();
  #audio = new Set<AudioListener>();
  #speaking?: Turn;
  #lastSequence?: number;
  #context?: AudioContext;
  #worklet?: AudioWorkletNode;
  #mic?: MediaStream;
  #capture?: Ring;
  #playback?: Ring;
  #timer?: ReturnType<typeof setInterval>;
  #queued: Int16Array[] = [];
  #offset = 0;
  #dropping = false;
  #workletUrl: string | URL;

  constructor(options: JimmyOptions) {
    this.ready = new Promise<Ready>((resolve, reject) => {
      this.#resolve = resolve;
      this.#reject = reject;
    });
    this.#workletUrl = options.workletUrl ?? new URL('./worklet.js', import.meta.url);
    if (options.engine === 'local') {
      const worker = new Worker(options.workerUrl ?? new URL('./worker.js', import.meta.url), { type: 'module' });
      this.#worker = worker;
      worker.onmessage = ({ data }) => {
        if (data.event) this.#receive(data.event);
        if (data.frame) this.#receiveAudio(data.frame);
      };
      worker.onerror = event => this.#error(event.message || 'Jimmy Worker stopped', true);
      const modelBase = options.modelBase ? new URL(options.modelBase, location.href).href.replace(/\/?$/, '/') : undefined;
      worker.postMessage({ init: { modelBase, ttsDevice: options.ttsDevice } });
    } else {
      const address = new URL(options.engine);
      if (!['ws:', 'wss:'].includes(address.protocol)) throw new Error('Jimmy server URL must use ws or wss');
      if (address.pathname === '/') address.pathname = '/v1/stream';
      const socket = new WebSocket(address, socketProtocols(options.token));
      this.#socket = socket;
      socket.binaryType = 'arraybuffer';
      socket.onmessage = ({ data }) => {
        if (typeof data === 'string') {
          try { this.#receive(JSON.parse(data)); } catch (error) { this.#error(String(error)); }
        } else if (data instanceof ArrayBuffer) {
          this.#receiveAudio(data);
        }
      };
      socket.onerror = () => this.#error('Jimmy WebSocket failed', true);
      socket.onclose = () => {
        if (!this.#closed) this.#error('Jimmy WebSocket closed', true);
      };
    }
  }

  onEvent(listener: Listener): () => void {
    this.#events.add(listener);
    return () => { this.#events.delete(listener); };
  }

  onAudio(listener: AudioListener): () => void {
    this.#audio.add(listener);
    return () => { this.#audio.delete(listener); };
  }

  send(event: ClientEvent): void {
    if (this.#closed) throw new Error('Jimmy is closed');
    if (event.type === 'session.configure' && event.output_codec === 'opus') {
      throw new Error('Jimmy web playback expects PCM16');
    }
    if (event.type === 'cancel' && (event.turn == null || event.turn === this.#speaking)) this.#dropAudio();
    if (!this.#warm) {
      if (this.#controls.length >= 32) throw new Error('Jimmy is still warming and its queue is full');
      this.#controls.push(event);
      return;
    }
    if (this.#worker) this.#worker.postMessage({ event });
    else this.#socket?.send(JSON.stringify(event));
  }

  configure(options: Configure): void { this.send({ ...options, type: 'session.configure' }); }
  listenStart(): void { this.send({ type: 'listen.start' }); }
  listenStop(): void { this.send({ type: 'listen.stop' }); }
  speakAppend(text: string, turn: Turn | null = null): void { this.send({ type: 'speak.append', text, turn }); }
  speakFlush(turn: Turn | null = null): void { this.send({ type: 'speak.flush', turn }); }
  playCue(id: string, turn: Turn | null = null): void { this.send({ type: 'cue.play', id, turn }); }
  cancel(turn: Turn | null = null): void { this.send({ type: 'cancel', turn }); }

  pushAudio(pcm: Int16Array): void {
    if (this.#closed || !this.#warm || pcm.length === 0) return;
    for (let at = 0; at < pcm.length; at += 24_000) {
      const part = pcm.subarray(at, at + 24_000);
      if (this.#worker) {
        const audio = part.slice();
        this.#worker.postMessage({ audio }, [audio.buffer]);
      } else if (this.#socket?.readyState === WebSocket.OPEN) {
        if (this.#socket.bufferedAmount > 256_000) throw new Error('Jimmy WebSocket audio backlog is full');
        const packet = new DataView(new ArrayBuffer(part.length * 2));
        for (let index = 0; index < part.length; index++) packet.setInt16(index * 2, part[index], true);
        this.#socket.send(packet.buffer);
      }
    }
  }

  /** Call from a user gesture. Set microphone to false for playback only. */
  async openAudio({ microphone = true } = {}): Promise<void> {
    if (this.#closed || this.#context) throw new Error('Jimmy audio is already open or Jimmy is closed');
    const context = new AudioContext({ sampleRate: 24_000, latencyHint: 'interactive' });
    this.#context = context;
    try {
      await context.resume();
      if (context.sampleRate !== 24_000) throw new Error(`The browser opened an unexpected ${context.sampleRate} Hz audio context`);
      await context.audioWorklet.addModule(this.#workletUrl);
      if (microphone) {
        const mic = await navigator.mediaDevices.getUserMedia({
          audio: { echoCancellation: true, noiseSuppression: true, channelCount: 1 },
        });
        if (this.#closed) {
          for (const track of mic.getTracks()) track.stop();
          throw new Error('Jimmy closed during microphone setup');
        }
        this.#mic = mic;
      }
      const isolated = globalThis.crossOriginIsolated && typeof SharedArrayBuffer !== 'undefined';
      const capture = isolated && microphone ? makeRing(8192) : undefined;
      const playback = isolated ? makeRing(16_384) : undefined;
      const node = new AudioWorkletNode(context, 'jimmy-audio', {
        numberOfInputs: microphone ? 1 : 0, numberOfOutputs: 1, outputChannelCount: [1],
        channelCount: 1, processorOptions: { capture, playback, mic: microphone },
      });
      this.#worklet = node;
      const echo = this.#mic?.getAudioTracks()[0]?.getSettings().echoCancellation === true;
      this.configure({ input_rate: context.sampleRate, echo_cancelled: echo });
      if (this.#mic) context.createMediaStreamSource(this.#mic).connect(node);
      node.connect(context.destination);
      if (capture && this.#worker) {
        void this.ready.then(() => {
          if (this.#worklet === node) this.#worker?.postMessage({ ring: capture });
        }, () => {});
      }
      if (capture && !this.#worker) this.#capture = new Ring(capture);
      if (playback) this.#playback = new Ring(playback);
      node.port.onmessage = ({ data }) => {
        if (data.mic) this.#sendMic(data.mic);
        if (data.consumed) this.#ack();
        if (data.dropped) {
          this.#dropping = false;
          for (let i = 0; i < data.released; i++) this.#ack();
        }
      };
      if (isolated) {
        const mic = new Int16Array(960);
        this.#timer = setInterval(() => {
          const count = this.#capture?.read(mic) ?? 0;
          if (count) this.#sendMic(mic.subarray(0, count));
          this.#drainPlayback();
        }, 10);
      }
    } catch (error) {
      await this.#closeAudio();
      throw error;
    }
  }

  async #closeAudio(): Promise<void> {
    clearInterval(this.#timer);
    this.#worker?.postMessage({ ring: null });
    this.#worklet?.disconnect();
    this.#worklet?.port.close();
    for (const track of this.#mic?.getTracks() ?? []) track.stop();
    const closing = this.#context;
    this.#context = undefined;
    this.#worklet = undefined;
    this.#mic = undefined;
    this.#capture = undefined;
    this.#playback = undefined;
    this.#queued.length = 0;
    this.#offset = 0;
    this.#dropping = false;
    if (closing && closing.state !== 'closed') await closing.close();
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    if (!this.#warm) this.#reject(new Error('Jimmy closed before it was ready'));
    const closing = this.#closeAudio();
    this.#worker?.terminate();
    this.#socket?.close();
    await closing;
  }

  #sendMic(pcm: Int16Array): void {
    try { this.pushAudio(pcm); } catch (error) { this.#error(String(error)); }
  }

  #receive(event: ServerEvent): void {
    if (this.#closed) return;
    if (event.type === 'session.ready') {
      if (event.protocol !== 1) { this.#error(`Unsupported Jimmy protocol ${event.protocol}`, true); return; }
      this.#warm = true;
      this.#resolve(event);
      for (const control of this.#controls.splice(0)) this.send(control);
    }
    if (event.type === 'speak.start') {
      this.#speaking = event.turn;
      this.#lastSequence = undefined;
    }
    if (event.type === 'speak.end' && event.turn === this.#speaking) {
      if (event.reason !== 'done') this.#dropAudio();
      else this.#speaking = undefined;
    }
    if (event.type === 'error' && event.fatal && !this.#warm) this.#reject(new Error(event.message));
    for (const listener of this.#events) listener(event);
  }

  #receiveAudio(data: ArrayBuffer): void {
    if (this.#closed) return;
    let frame;
    try { frame = decodeAudio(data); } catch (error) { this.#error(String(error)); this.#ack(); return; }
    if (frame.turn !== this.#speaking || this.#lastSequence !== undefined && frame.seq <= this.#lastSequence) {
      this.#ack();
      return;
    }
    this.#lastSequence = frame.seq;
    for (const listener of this.#audio) listener(frame);
    if (!this.#worklet) { this.#ack(); return; }
    if (this.#playback) {
      if (this.#queued.length > 6_000) { this.#error('Playback backlog is full'); this.cancel(); this.#ack(); return; }
      this.#queued.push(frame.pcm);
      this.#drainPlayback();
    } else {
      this.#worklet.port.postMessage({ samples: frame.pcm });
    }
  }

  #dropAudio(): void {
    this.#speaking = undefined;
    for (let i = 0; i < this.#queued.length; i++) this.#ack();
    this.#queued.length = 0;
    this.#offset = 0;
    this.#dropping = Boolean(this.#playback);
    this.#worklet?.port.postMessage({ drop: true });
  }

  #drainPlayback(): void {
    const ring = this.#playback;
    if (!ring || this.#dropping) return;
    while (this.#queued.length) {
      const samples = this.#queued[0]!;
      this.#offset += ring.write(samples, this.#offset);
      if (this.#offset < samples.length) break;
      this.#queued.shift();
      this.#offset = 0;
      this.#ack();
    }
  }

  #ack(): void { this.#worker?.postMessage({ ack: true }); }

  #error(message: string, fatal = false): void {
    this.#receive({ type: 'error', code: 'transport', message, turn: this.#speaking ?? null, fatal });
  }
}
