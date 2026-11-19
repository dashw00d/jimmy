import { Ring } from './ring.js';

class JimmyAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { capture, playback, mic } = options.processorOptions ?? {};
    this.mic = Boolean(mic);
    this.capture = capture && new Ring(capture);
    this.playback = playback && new Ring(playback);
    this.recording = new Int16Array(960);
    this.recorded = 0;
    this.output = new Int16Array(128);
    this.queue = [];
    this.offset = 0;
    this.port.onmessage = ({ data }) => {
      if (data.drop) {
        this.playback?.discard();
        const released = this.queue.length;
        this.queue.length = 0;
        this.offset = 0;
        this.port.postMessage({ dropped: true, released });
      } else if (data.samples) {
        this.queue.push(data.samples);
      }
    };
  }

  process(inputs, outputs) {
    const input = inputs[0]?.[0];
    if (this.mic && input) {
      for (let i = 0; i < input.length; i++) {
        this.recording[this.recorded++] = Math.max(-32768, Math.min(32767, Math.round(input[i] * 32768)));
        if (this.recorded === this.recording.length) {
          if (this.capture) this.capture.write(this.recording);
          else this.port.postMessage({ mic: this.recording });
          this.recorded = 0;
        }
      }
    }
    const out = outputs[0]?.[0];
    if (!out) return true;
    if (this.playback) {
      if (this.output.length !== out.length) this.output = new Int16Array(out.length);
      const count = this.playback.read(this.output);
      for (let i = 0; i < out.length; i++) out[i] = i < count ? this.output[i] / 32768 : 0;
      return true;
    }
    for (let i = 0; i < out.length; i++) {
      const samples = this.queue[0];
      out[i] = samples ? samples[this.offset++] / 32768 : 0;
      if (samples && this.offset === samples.length) {
        this.queue.shift();
        this.offset = 0;
        this.port.postMessage({ consumed: true });
      }
    }
    return true;
  }
}
registerProcessor('jimmy-audio', JimmyAudio);
