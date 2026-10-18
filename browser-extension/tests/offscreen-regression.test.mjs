import assert from "node:assert/strict";
import test from "node:test";
const offscreenUrl = new URL("../dist/offscreen.js", import.meta.url).href;

function deferred() {
  let resolve;
  const promise = new Promise(value => { resolve = value; });
  return { promise, resolve };
}

function trackStream() {
  const track = { stopped: false, stop() { this.stopped = true; } };
  return { track, stream: { getTracks: () => [track] } };
}

async function settle() {
  await new Promise(resolve => setImmediate(resolve));
}

async function setup(getUserMedia, workerMessage = async () => ({ ok: true })) {
  let listener;
  const sockets = [];
  const contexts = [];
  const destinations = [];
  const processors = [];
  const recorders = [];
  const intervals = [];
  const workerMessages = [];
  globalThis.AudioContext = class {
    constructor() { this.audioWorklet = { addModule: async () => {} }; contexts.push(this); }
    createMediaStreamSource() { return { connect() {}, disconnect() {} }; }
    createGain() { this.sink = { gain: {}, connect() {}, disconnect() {} }; return this.sink; }
    createMediaStreamDestination() { const destination = { stream: {}, disconnect() {} }; destinations.push(destination); return destination; }
    async resume() {}
    async close() { this.closed = true; await this.closeBarrier; }
  };
  globalThis.AudioWorkletNode = class {
    constructor() { this.port = { close() {} }; processors.push(this); }
    connect() {}
    disconnect() { this.disconnected = true; }
  };
  globalThis.MediaRecorder = class {
    static isTypeSupported(type) { return type === 'audio/webm;codecs=opus' || type === 'audio/webm'; }
    constructor(stream, options) { this.stream = stream; this.mimeType = options.mimeType; this.state = 'inactive'; recorders.push(this); }
    start() { this.state = 'recording'; }
    pause() { this.state = 'paused'; }
    resume() { this.state = 'recording'; }
    stop() { this.state = 'inactive'; this.ondataavailable?.({ data: new Blob(['webm'], { type: this.mimeType }) }); this.onstop?.(); }
  };
  class MockSocket {
    static OPEN = 1;
    constructor() { this.readyState = 0; this.sent = []; sockets.push(this); }
    send(value) { if (this.throwOnSend) throw new Error("send failed"); this.sent.push(value); }
    close() { this.readyState = 3; this.onclose?.(); }
    open() { this.readyState = 1; this.onopen?.(); }
    message(value) { this.onmessage?.({ data: value }); }
  }
  globalThis.window = globalThis;
  globalThis.setInterval = callback => {
    intervals.push(callback);
    return intervals.length;
  };
  globalThis.clearInterval = () => {};
  globalThis.document = { createElement: () => ({ relList: { supports: () => true } }) };
  globalThis.WebSocket = MockSocket;
  Object.defineProperty(globalThis, "navigator", {
    value: { mediaDevices: { getUserMedia } }, configurable: true
  });
  globalThis.chrome = {
    runtime: {
      getURL: path => path,
      sendMessage: async message => { workerMessages.push(message); return workerMessage(message); },
      onMessage: { addListener(callback) { listener = callback; } }
    }
  };
  await import(`${offscreenUrl}?case=${crypto.randomUUID()}`);
  const send = async message => new Promise(resolve => {
    const keepAlive = listener({ target: "offscreen", ...message }, {}, resolve);
    if (!keepAlive) resolve(undefined);
  });
  return { send, sockets, contexts, processors, destinations, recorders, intervals, workerMessages };
}

function startMessage() {
  return {
    type: "offscreen_start",
    tab: { id: 42, title: "test", url: "https://example.test" },
    config: { port: 38471, token: "token", returnAudio: false, streamKey: "" },
    streamId: "stream-id",
    streamKey: "tab-42"
  };
}

test("stop during websocket open cancels the pending start", async () => {
  const { stream } = trackStream();
  const app = await setup(async () => stream);
  const start = app.send(startMessage());
  await settle();
  assert.equal(app.sockets.length, 1);
  assert.deepEqual(await app.send({ type: "offscreen_stop", tabId: 42 }), { ok: true });
  assert.equal((await start).ok, false);
});

async function begin(app, tabId = 42) {
  const start = app.send({ ...startMessage(), tab: { id: tabId }, streamKey: `tab-${tabId}` });
  await settle();
  const socket = app.sockets.at(-1);
  if (socket.readyState === 0) {
    socket.open();
    await settle();
    socket.message(JSON.stringify({ type: "accepted" }));
  }
  assert.equal((await start).ok, true);
  await settle();
  app.processors.at(-1).port.onmessage({ data: new Float32Array(960).buffer });
  return socket;
}

test("停止は音声資源を解放し、同じ通信接続で再開して受信側の状態も表示する", async () => {
  const media = [];
  const app = await setup(async () => { const item = trackStream(); media.push(item); return item.stream; });
  const socket = await begin(app);
  socket.message(JSON.stringify({ type: "receiver_state", stream_key: "tab-42", state: "running" }));
  assert.equal((await app.send({ type: "offscreen_stop", tabId: 42 })).ok, true);
  assert.equal(media[0].track.stopped, true);
  assert.equal(app.contexts[0].closed, true);
  assert.equal(app.processors[0].port.onmessage, null);
  assert.equal(socket.readyState, 1);
  assert.ok(socket.sent.some(value => typeof value === "string" && value.includes('"state":"stopped"')));
  let status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "stopped");
  assert.equal(status.receiverState, "running");
  socket.message(JSON.stringify({ type: "receiver_state", stream_key: "tab-42", state: "stopped" }));
  await begin(app);
  assert.equal(app.sockets.length, 1);
  assert.equal(media.length, 2);
  assert.equal(media[1].track.stopped, false);
  status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "streaming");
  assert.equal(status.receiverState, "stopped");
  socket.close();
  await settle();
  assert.equal(media[1].track.stopped, true);
  assert.equal((await app.send({ type: "offscreen_get_statuses" })).statuses[0].state, "failed");
});

test("PCM送信を使わずにタブ音声をWebMへ分割保存できる", async () => {
  const media = trackStream();
  const app = await setup(async () => media.stream);
  const started = app.send({
    type: "offscreen_start_file",
    tab: { id: 42, title: "test", url: "https://example.test" },
    config: { port: 38471, token: "token", returnAudio: false, streamKey: "tab-42" },
    streamId: "stream-id",
    streamKey: "tab-42"
  });
  await settle();
  assert.equal((await started).ok, true);
  assert.equal(app.sockets.length, 0);
  const status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "stopped");
  assert.equal(status.fileRecording, true);
  assert.equal((await app.send({ type: "offscreen_stop_file", tabId: 42 })).ok, true);
  const download = app.workerMessages.find(message => message.type === "download_media");
  assert.ok(download);
  assert.match(download.filename, /\.webm$/);
  assert.match(download.url, /^blob:/);
  assert.equal(app.recorders[0].stream, app.destinations[0].stream);
  assert.equal(app.recorders[0].state, "inactive");
  assert.equal(media.track.stopped, true);
});

test("PCM送信中のWebM保存は同じタブ音声を保存し、送信を継続する", async () => {
  const app = await setup(async () => trackStream().stream);
  const socket = await begin(app);
  assert.equal((await app.send({ type: "offscreen_start_file", tab: { id: 42 }, config: { streamKey: "tab-42" }, streamKey: "tab-42" })).ok, true);
  app.processors.at(-1).port.onmessage({ data: new Float32Array([0.25, -0.25]).buffer });
  assert.equal((await app.send({ type: "offscreen_stop_file", tabId: 42 })).ok, true);
  assert.equal(socket.readyState, 1);
  assert.equal((await app.send({ type: "offscreen_get_statuses" })).statuses[0].state, "streaming");
  assert.ok(app.workerMessages.some(message => message.type === "download_media"));
  socket.close();
});

test("再生の一時停止と再開はWebM録音とPCM送信を同時に切り替える", async () => {
  const app = await setup(async () => trackStream().stream);
  const socket = await begin(app);
  assert.equal((await app.send({ type: "offscreen_start_file", tab: { id: 42 }, config: { streamKey: "tab-42" }, streamKey: "tab-42" })).ok, true);
  assert.equal(app.recorders[0].state, "recording");
  assert.equal((await app.send({ type: "offscreen_pause", tabId: 42 })).ok, true);
  assert.equal(app.recorders[0].state, "paused");
  assert.equal((await app.send({ type: "offscreen_get_statuses" })).statuses[0].state, "stopped");
  assert.equal((await app.send({ type: "offscreen_resume", tabId: 42 })).ok, true);
  assert.equal(app.recorders[0].state, "recording");
  app.processors.at(-1).port.onmessage({ data: new Float32Array([0.2, -0.2]).buffer });
  assert.equal((await app.send({ type: "offscreen_get_statuses" })).statuses[0].state, "streaming");
  assert.ok(socket.sent.some(value => typeof value === "string" && value.includes('"state":"stopped"')));
  assert.ok(socket.sent.some(value => typeof value === "string" && value.includes('"state":"running"')));
  socket.close();
});

test("再生終端を検知するとPCM送信とWebM録音を自動停止する", async () => {
  const media = trackStream();
  const app = await setup(async () => media.stream, async message => message.type === "read_tab_media"
    ? { ok: true, state: "ended" } : { ok: true });
  const started = app.send({
    type: "offscreen_start_file", tab: { id: 42, title: "test", url: "https://example.test" },
    config: { streamKey: "tab-42" }, streamId: "stream-id", streamKey: "tab-42"
  });
  await settle();
  assert.equal((await started).ok, true);
  assert.equal((await app.send({ type: "offscreen_arm_media_end", tabId: 42 })).ok, true);
  app.intervals[0]();
  await settle();
  await settle();
  const status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "stopped");
  assert.ok(app.workerMessages.some(message => message.type === "download_media"));
  assert.equal(app.recorders[0].state, "inactive");
  assert.equal(media.track.stopped, true);
});

test("PCM停止後にWebM保存を開始すると音声処理を再構築する", async () => {
  const media = [];
  const app = await setup(async () => { const item = trackStream(); media.push(item); return item.stream; });
  const socket = await begin(app);
  assert.equal((await app.send({ type: "offscreen_stop", tabId: 42 })).ok, true);
  assert.equal((await app.send({
    type: "offscreen_start_file", tab: { id: 42 }, config: { port: 38471, token: "token", returnAudio: false, streamKey: "tab-42" }, streamId: "stream-id", streamKey: "tab-42"
  })).ok, true);
  assert.equal(app.processors.length, 2);
  app.processors.at(-1).port.onmessage({ data: new Float32Array([0.1, -0.1]).buffer });
  assert.equal((await app.send({ type: "offscreen_stop_file", tabId: 42 })).ok, true);
  assert.equal(media[1].track.stopped, true);
  assert.equal(socket.readyState, 1);
  socket.close();
});

test("停止完了前の再開を拒否し、他のタブの配信には影響しない", async () => {
  const app = await setup(async () => trackStream().stream);
  const first = await begin(app);
  const other = await begin(app, 43);
  const barrier = deferred();
  app.contexts[0].closeBarrier = barrier.promise;
  const stopping = app.send({ type: "offscreen_stop", tabId: 42 });
  await settle();
  assert.equal((await app.send(startMessage())).ok, false);
  assert.equal(other.readyState, 1);
  barrier.resolve();
  assert.equal((await stopping).ok, true);
  const statuses = (await app.send({ type: "offscreen_get_statuses" })).statuses;
  assert.deepEqual(statuses.map(item => item.state), ["stopped", "streaming"]);
  first.close(); other.close();
  await settle();
});

test("音声戻しは対象タブのGainだけを即時変更しPCMと接続を維持する", async () => {
  const app = await setup(async () => trackStream().stream);
  const first = await begin(app);
  const other = await begin(app, 43);
  const before = first.sent.length;
  assert.equal((await app.send({ type: "offscreen_set_return_audio", tabId: 42, returnAudio: true })).ok, true);
  assert.equal(app.contexts[0].sink.gain.value, 1);
  assert.equal(app.contexts[1].sink.gain.value, 0);
  app.processors[0].port.onmessage({ data: new Float32Array(960).buffer });
  assert.equal(first.sent.length, before + 1);
  assert.equal(first.readyState, 1);
  assert.equal((await app.send({ type: "offscreen_get_statuses" })).statuses[0].returnAudio, true);
  await app.send({ type: "offscreen_set_return_audio", tabId: 42, returnAudio: false });
  assert.equal(app.contexts[0].sink.gain.value, 0);
  first.close(); other.close(); await settle();
});

test("本体からのmedia_commandは対象tabだけへ転送し、失敗してもPCM状態をfailedにしない", async () => {
  const workerMessages = [];
  const app = await setup(async () => trackStream().stream, async message => {
    workerMessages.push(message);
    return { ok: true, state: "paused", error: "ページが再生を拒否しました" };
  });
  const socket = await begin(app);
  socket.message(JSON.stringify({
    type: "media_command", stream_key: "tab-42", request_id: "request-1", action: "play"
  }));
  await settle();
  await settle();
  assert.ok(workerMessages.some(message => message.target === "service-worker"
    && message.type === "control_tab_media" && message.tabId === 42 && message.action === "play"));
  assert.ok(socket.sent.some(value => typeof value === "string"
    && value.includes('"type":"media_result"')
    && value.includes('"request_id":"request-1"')
    && value.includes('"state":"paused"')));
  const status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "streaming");
  assert.equal(status.mediaState, "paused");
  assert.match(status.mediaError, /拒否/);
  socket.close();
});

test("heartbeatのmedia状態読取はページ側の停止をWebSocketとstatusへ同期する", async () => {
  const app = await setup(async () => trackStream().stream, async message => {
    if (message.type === "read_tab_media") return { ok: true, state: "paused" };
    return { ok: true };
  });
  const socket = await begin(app);
  assert.equal(app.intervals.length, 1);
  app.intervals[0]();
  await settle();
  await settle();
  assert.ok(socket.sent.some(value => typeof value === "string"
    && value.includes('"type":"media_state"')
    && value.includes('"state":"paused"')));
  const status = (await app.send({ type: "offscreen_get_statuses" })).statuses[0];
  assert.equal(status.state, "streaming");
  assert.equal(status.mediaState, "paused");
  socket.close();
});

test("キー変更用の接続解放は停止後だけ許可し他タブを保持する", async () => {
  const app = await setup(async () => trackStream().stream);
  const first = await begin(app);
  const other = await begin(app, 43);
  assert.equal((await app.send({ type: "offscreen_forget", tabId: 42 })).ok, false);
  await app.send({ type: "offscreen_stop", tabId: 42 });
  assert.equal((await app.send({ type: "offscreen_forget", tabId: 42 })).ok, true);
  assert.equal(first.readyState, 3);
  assert.equal(other.readyState, 1);
  assert.deepEqual((await app.send({ type: "offscreen_get_statuses" })).statuses.map(item => item.tabId), [43]);
  other.close(); await settle();
});

test("stop during authentication cancels the pending start", async () => {
  const { stream } = trackStream();
  const app = await setup(async () => stream);
  const start = app.send(startMessage());
  await settle();
  app.sockets[0].open();
  await settle();
  assert.match(app.sockets[0].sent[0], /"hello"/);
  assert.deepEqual(await app.send({ type: "offscreen_stop", tabId: 42 }), { ok: true });
  assert.equal((await start).ok, false);
});

test("late getUserMedia result is stopped after cancellation", async () => {
  const media = deferred();
  const { stream, track } = trackStream();
  const app = await setup(() => media.promise);
  const start = app.send(startMessage());
  await settle();
  const stop = app.send({ type: "offscreen_stop", tabId: 42 });
  media.resolve(stream);
  assert.equal((await start).ok, false);
  await stop;
  assert.equal(track.stopped, true);
});

test("同じタブの並行開始だけを拒否し、別タブの開始準備は独立する", async () => {
  const media = deferred();
  const app = await setup(() => media.promise);
  const first = app.send(startMessage());
  await settle();
  const second = await app.send(startMessage());
  assert.equal(second.ok, false);
  assert.match(second.error, /実行中/);
  const other = app.send({ ...startMessage(), tab: { id: 43, title: "other", url: "https://other.test" }, streamKey: "tab-43" });
  await settle();
  assert.equal(app.sockets.length, 0);
  const stop = app.send({ type: "offscreen_stop", tabId: 42 });
  const stopOther = app.send({ type: "offscreen_stop", tabId: 43 });
  const { stream } = trackStream();
  media.resolve(stream);
  assert.equal((await first).ok, false);
  assert.equal((await other).ok, false);
  await Promise.all([stop, stopOther]);
});

test("closing a pending tab cancels capture and releases a late stream", async () => {
  const media = deferred();
  const { stream, track } = trackStream();
  const app = await setup(() => media.promise);
  const start = app.send(startMessage());
  await settle();
  await app.send({ type: "tab_closed", tabId: 42 });
  media.resolve(stream);
  assert.equal((await start).ok, false);
  assert.equal(track.stopped, true);
  await settle();
  assert.deepEqual(await app.send({ type: "offscreen_get_statuses" }), { ok: true, statuses: [] });
});
