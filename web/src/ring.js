// Single producer and single consumer. The indices are monotonic u32 counters.
export function makeRing(capacity) {
  if (capacity < 2 || (capacity & (capacity - 1)) !== 0) throw new RangeError('Audio ring capacity must be a power of two');
  return new SharedArrayBuffer(8 + capacity * 2);
}

export class Ring {
  constructor(buffer) {
    this.indices = new Int32Array(buffer, 0, 2);
    this.samples = new Int16Array(buffer, 8);
    this.mask = this.samples.length - 1;
  }
  get readable() {
    return (Atomics.load(this.indices, 0) - Atomics.load(this.indices, 1)) >>> 0;
  }
  write(samples, offset = 0) {
    const at = Atomics.load(this.indices, 0);
    const length = Math.min(samples.length - offset, this.samples.length - this.readable);
    for (let i = 0; i < length; i++) this.samples[(at + i) & this.mask] = samples[offset + i];
    Atomics.store(this.indices, 0, (at + length) | 0);
    return length;
  }
  read(out) {
    const at = Atomics.load(this.indices, 1);
    const length = Math.min(out.length, this.readable);
    for (let i = 0; i < length; i++) out[i] = this.samples[(at + i) & this.mask];
    Atomics.store(this.indices, 1, (at + length) | 0);
    return length;
  }
  discard() {
    Atomics.store(this.indices, 1, Atomics.load(this.indices, 0));
  }
}
