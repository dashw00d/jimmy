import init, { BrowserSession } from './core/jimmy_web.js';
import { loadEngines } from './model.js';
import { Ring } from './ring.js';

let session;
let capture;
let timer;
let playbackAhead = 0;
const mic = new Int16Array(960);

const events = () => {
  for (const json of session.events()) self.postMessage({ event: JSON.parse(json) });
};
const error = (message, fatal = false) => self.postMessage({
  event: { type: 'error', turn: session?.turn(), code: fatal ? 'engine' : 'worker', message: String(message), fatal },
});

function tick() {
  try {
    if (capture) {
      const count = capture.read(mic);
      if (count) session.audio(mic.subarray(0, count));
    }
    events();
    const previous = performance.now();
    for (let i = 0; i < 6 && performance.now() - previous < 8 && playbackAhead < 20; i++) {
      const frame = session.pull_audio();
      events();
      if (!frame) break;
      self.postMessage({ frame: frame.buffer }, [frame.buffer]);
      playbackAhead += 1;
    }
  } catch (e) { error(e); }
}

self.onmessage = async ({ data }) => {
  try {
    if (data.init && !session) {
      const [, engines] = await Promise.all([init(), loadEngines(data.init)]);
      const { stt, start, pull, cancel, voices } = engines;
      session = new BrowserSession(stt, start, pull, cancel, voices);
      events();
      timer = setInterval(tick, 5);
    } else if (data.close) {
      clearInterval(timer);
      session?.free();
      self.close();
    } else if (data.ring !== undefined) {
      capture = data.ring ? new Ring(data.ring) : undefined;
    } else if (data.ack) {
      playbackAhead = Math.max(0, playbackAhead - 1);
    } else if (session) {
      if (data.event) session.event(JSON.stringify(data.event));
      if (data.audio) session.audio(data.audio);
      events();
    }
  } catch (e) { error(e, !session); }
};
