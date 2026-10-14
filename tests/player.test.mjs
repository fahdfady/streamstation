// Red/green test for the player bookkeeping bug: refreshing /api/status while
// the station is on the same movie must NOT tear down and rebuild the HLS
// player (that resets playback to the live edge behind the current position,
// which looks like the stream rewinding a couple of seconds every poll).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function extractInlineScript(html) {
  const match = html.match(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/i);
  if (!match) throw new Error("no inline <script> found in web/index.html");
  return match[1];
}

const code = extractInlineScript(readFileSync(join(root, "web", "index.html"), "utf8"));

let status = {
  playing: true,
  seq: 0,
  viewers: 1,
  count: 2,
  index: 0,
  title: "Alpha",
  next_title: "Beta",
  offset_secs: 1,
  duration_secs: 12,
  station_total_secs: 24,
  uptime_secs: 5,
};

let hlsInstances = 0;
let playCalls = 0;
const intervals = [];

class FakeHls {
  static isSupported() {
    return true;
  }
  static Events = { ERROR: "hlsError" };
  constructor() {
    hlsInstances++;
  }
  loadSource() {}
  attachMedia() {}
  on() {}
  destroy() {}
}

const video = {
  paused: false,
  currentTime: 0,
  addEventListener() {},
  play() {
    playCalls++;
    return Promise.resolve();
  },
};

const elements = new Map();
function getElement(id) {
  if (id === "video") return video;
  if (!elements.has(id)) {
    elements.set(id, { textContent: "", className: "", hidden: false, style: {}, addEventListener() {} });
  }
  return elements.get(id);
}

globalThis.document = {
  getElementById: getElement,
  addEventListener() {},
  hidden: false,
  visibilityState: "visible",
};
globalThis.window = { addEventListener() {}, crypto: undefined };
globalThis.Hls = FakeHls;
globalThis.window.Hls = FakeHls;
globalThis.window.crypto = globalThis.crypto;
globalThis.Worker = class {};
globalThis.URL.createObjectURL = () => "blob:test";
globalThis.setInterval = (fn) => {
  intervals.push(fn);
  return intervals.length;
};
globalThis.setTimeout = () => 0;
globalThis.fetch = async (path) => {
  if (String(path).includes("/api/status")) {
    return { ok: true, json: async () => status };
  }
  return { ok: true, json: async () => ({ viewers: 1 }) };
};

(0, eval)(code);

const flush = () => new Promise((resolve) => setImmediate(resolve));

async function tick() {
  for (const fn of intervals) fn();
  await flush();
  await flush();
}

let failures = 0;
function check(cond, message) {
  if (cond) {
    console.log(`ok - ${message}`);
  } else {
    failures++;
    console.error(`FAIL - ${message}`);
  }
}

async function main() {
  await flush();
  await flush();
  check(hlsInstances === 1, `tune-in attaches the player once (got ${hlsInstances})`);

  for (let i = 0; i < 4; i++) await tick();
  check(hlsInstances === 1, `polling on the same movie does not re-attach (got ${hlsInstances})`);
  check(playCalls === 1, `polling does not restart video.play() (got ${playCalls})`);

  status = { ...status, index: 1, title: "Beta", next_title: "Alpha" };
  await tick();
  check(hlsInstances === 2, `movie boundary attaches the next stream (got ${hlsInstances})`);
  await tick();
  check(hlsInstances === 2, `boundary attach happens exactly once (got ${hlsInstances})`);

  status = { ...status, seq: 1, offset_secs: 3 };
  await tick();
  check(hlsInstances === 3, `seek (same movie, new seq) re-attaches (got ${hlsInstances})`);
  await tick();
  check(hlsInstances === 3, `seek re-attaches exactly once (got ${hlsInstances})`);

  process.exitCode = failures > 0 ? 1 : 0;
}

main();
