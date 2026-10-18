import {
  DEFAULT_SETTINGS,
  pcmFrameBytes,
  type CaptureConfig,
  type CaptureStatus,
  type EdgeHello,
  type MediaAction,
  type MediaControlResult,
  type MediaState
} from "./protocol";

const CONNECT_TIMEOUT_MS = 8_000;
const AUTH_TIMEOUT_MS = 8_000;
const PCM_TIMEOUT_MS = 10_000;
const FILE_SEGMENT_MS = 10 * 60 * 1_000;
const FILE_BITRATE = 96_000;

type Operation = {
  tabId: number;
  streamKey: string;
  cancelled: boolean;
  disposed: boolean;
  stream?: MediaStream;
  socket?: WebSocket;
  context?: AudioContext;
  source?: MediaStreamAudioSourceNode;
  processor?: AudioWorkletNode;
  sink?: GainNode;
  recordingDestination?: MediaStreamAudioDestinationNode;
  heartbeatTimer?: number;
  mediaPollTimer?: number;
  firstPcmTimer?: number;
  connectionTimer?: number;
  authTimer?: number;
  cancelWaiters: Set<() => void>;
  sentPcm: boolean;
  pcmConfigured: boolean;
  pcmEnabled: boolean;
  paused: boolean;
  settingsKey: string;
  returnAudio: boolean;
  fileRecording: boolean;
  fileRecorder?: MediaRecorder;
  fileChunks: Blob[];
  fileSegmentTimer?: number;
  fileSegmentNumber: number;
  fileRotation?: Promise<void>;
  fileName?: string;
  fileError?: string;
  mediaEndArmed: boolean;
  receiverState?: "running" | "stopped";
  lastPeerSeen?: number;
  acknowledgedCaptureState?: "running" | "stopped";
  mediaPollInFlight?: boolean;
  mediaState?: MediaState;
  mediaError?: string;
};

const pending = new Map<number, Operation>();
const active = new Map<number, Operation>();
const statuses = new Map<number, CaptureStatus>();
const starts = new Map<number, Promise<void>>();
const stops = new Map<number, Promise<void>>();

function report(next: CaptureStatus): void {
  if (next.tabId === undefined) return;
  const tabId = next.tabId;
  if (next.state === "idle") statuses.delete(next.tabId); else {
    const operation = active.get(tabId) ?? pending.get(tabId);
    next = {
      ...next,
      returnAudio: next.returnAudio ?? operation?.returnAudio,
      fileRecording: next.fileRecording ?? operation?.fileRecording,
      fileName: next.fileName ?? operation?.fileName,
      fileError: next.fileError ?? operation?.fileError,
      receiverState: next.receiverState ?? operation?.receiverState,
      mediaState: next.mediaState ?? operation?.mediaState,
      mediaError: next.mediaError ?? operation?.mediaError
    };
    statuses.set(tabId, next);
  }
  void chrome.runtime.sendMessage({ target: "service-worker", type: "capture_status", status: next }).catch(() => {});
}

function allStatuses(): CaptureStatus[] { return [...statuses.values()].sort((a, b) => (a.tabId ?? 0) - (b.tabId ?? 0)); }

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function cancelled(): Error {
  return new Error("キャプチャを停止しました");
}

function requireOwner(operation: Operation): void {
  if (operation.cancelled || (pending.get(operation.tabId) !== operation && active.get(operation.tabId) !== operation)) throw cancelled();
}

function isCancelled(error: unknown): boolean {
  return error instanceof Error && error.message === "キャプチャを停止しました";
}

function isMediaState(value: unknown): value is MediaState {
  return value === "playing" || value === "paused" || value === "ended" || value === "unsupported" || value === "unknown";
}

function isCurrentSocket(operation: Operation, socket: WebSocket): boolean {
  return !operation.disposed
    && operation.socket === socket
    && active.get(operation.tabId) === operation;
}

function mediaResult(value: unknown): MediaControlResult {
  const result = value as { state?: unknown; error?: unknown; positionSeconds?: unknown; durationSeconds?: unknown } | undefined;
  return {
    state: isMediaState(result?.state) ? result.state : "unknown",
    ...(typeof result?.error === "string" && result.error ? { error: result.error } : {}),
    ...(typeof result?.positionSeconds === "number" && Number.isFinite(result.positionSeconds) ? { positionSeconds: result.positionSeconds } : {}),
    ...(typeof result?.durationSeconds === "number" && Number.isFinite(result.durationSeconds) ? { durationSeconds: result.durationSeconds } : {})
  };
}

function reportMedia(operation: Operation, result: MediaControlResult): void {
  operation.mediaState = result.state;
  operation.mediaError = result.error;
  const current = statuses.get(operation.tabId);
  if (current) report({ ...current, mediaState: result.state, mediaError: result.error });
}

function filePart(value: string): string {
  return value.replace(/[<>:"/\\|?*\u0000-\u001f]/g, "_").trim().slice(0, 80) || "tab-audio";
}

function recorderError(event: Event, fallback: string): string {
  const error = (event as ErrorEvent).error;
  return error instanceof Error && error.message ? error.message : fallback;
}

function webmFileName(operation: Operation, partNumber: number): string {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  return `VoiceMixer-${stamp}-${filePart(operation.streamKey)}-part${String(partNumber).padStart(4, "0")}.webm`;
}

function supportedWebmMimeType(): string {
  const candidates = ["audio/webm;codecs=opus", "audio/webm"];
  const supported = candidates.find(type => MediaRecorder.isTypeSupported(type));
  if (!supported) throw new Error("このEdgeではWebM/Opus録音に対応していません");
  return supported;
}

function beginFileRecording(operation: Operation): void {
  if (operation.fileRecording || operation.fileRecorder) throw new Error("このタブはWebM保存中です");
  if (!operation.stream) throw new Error("録音対象の音声ストリームを取得できません");
  operation.fileRecording = true;
  operation.fileChunks = [];
  operation.fileSegmentNumber = 1;
  operation.fileRecorder = undefined;
  operation.fileSegmentTimer = undefined;
  operation.fileRotation = undefined;
  operation.fileName = undefined;
  operation.fileError = undefined;
  beginFileSegment(operation);
}

function beginFileSegment(operation: Operation): void {
  if (!operation.fileRecording || operation.disposed || !operation.stream) return;
  const recordingStream = operation.recordingDestination?.stream;
  if (!recordingStream) throw new Error("録音用の音声出力を初期化できません");
  const recorder = new MediaRecorder(recordingStream, {
    mimeType: supportedWebmMimeType(),
    audioBitsPerSecond: FILE_BITRATE
  });
  operation.fileRecorder = recorder;
  operation.fileChunks = [];
  recorder.ondataavailable = event => {
    if (event.data.size > 0 && operation.fileRecorder === recorder) operation.fileChunks.push(event.data);
  };
  recorder.onerror = event => {
    const detail = recorderError(event, "WebM録音中にエラーが発生しました");
    operation.fileError = detail;
    void fail(operation, new Error(detail));
  };
  recorder.start(1_000);
  operation.fileSegmentTimer = window.setTimeout(() => {
    void rotateFileSegment(operation).catch(error => { void fail(operation, error); });
  }, FILE_SEGMENT_MS);
  (operation.fileSegmentTimer as unknown as { unref?: () => void }).unref?.();
}

async function stopFileSegment(operation: Operation): Promise<void> {
  const recorder = operation.fileRecorder;
  if (!recorder) return;
  if (operation.fileSegmentTimer !== undefined) clearTimeout(operation.fileSegmentTimer);
  operation.fileSegmentTimer = undefined;
  const chunks = operation.fileChunks;
  await new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (error?: unknown) => {
      if (settled) return;
      settled = true;
      if (error) reject(error); else resolve();
    };
    recorder.onstop = () => finish();
    recorder.onerror = event => finish(new Error(recorderError(event, "WebM録音を確定できませんでした")));
    try {
      if (recorder.state === "inactive") finish(); else recorder.stop();
    } catch (error) {
      finish(error);
    }
  });
  operation.fileRecorder = undefined;
  operation.fileChunks = [];
  if (!chunks.length) return;
  const blob = new Blob(chunks, { type: recorder.mimeType || "audio/webm" });
  const partNumber = operation.fileSegmentNumber++;
  const filename = webmFileName(operation, partNumber);
  const url = URL.createObjectURL(blob);
  try {
    const result = await chrome.runtime.sendMessage({ target: "service-worker", type: "download_media", url, filename });
    if (!result?.ok) throw new Error(result?.error || "WebMファイルの保存を開始できませんでした");
    operation.fileName = filename;
    operation.fileError = undefined;
    // downloads.downloadがURLを消費するまで、Offscreen側のBlob URLを保持する。
    const revokeTimer = window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
    (revokeTimer as unknown as { unref?: () => void }).unref?.();
  } catch (error) {
    URL.revokeObjectURL(url);
    operation.fileError = errorMessage(error);
    throw error;
  }
}

async function rotateFileSegment(operation: Operation): Promise<void> {
  if (operation.fileRotation) return operation.fileRotation;
  const rotation = (async () => {
    await stopFileSegment(operation);
    if (operation.fileRecording && !operation.disposed) beginFileSegment(operation);
  })();
  operation.fileRotation = rotation;
  try {
    await rotation;
  } finally {
    if (operation.fileRotation === rotation) operation.fileRotation = undefined;
  }
}

async function finishFileRecording(operation: Operation): Promise<void> {
  if (!operation.fileRecording && !operation.fileRecorder && !operation.fileRotation) return;
  operation.fileRecording = false;
  try {
    if (operation.fileRotation) await operation.fileRotation;
    await stopFileSegment(operation);
  } catch (error) {
    operation.fileError = errorMessage(error);
    throw error;
  } finally {
    const state = operation.pcmEnabled ? (operation.sentPcm ? "streaming" : "authenticated") : "stopped";
    report({ state, tabId: operation.tabId, streamKey: operation.streamKey, fileRecording: false, fileName: operation.fileName, fileError: operation.fileError });
    if (!operation.pcmEnabled && active.get(operation.tabId) === operation) {
      await releaseAudio(operation);
      active.delete(operation.tabId);
    }
  }
}

async function handleTrackEnded(operation: Operation): Promise<void> {
  if (!operation.fileRecording) {
    await stop(operation.tabId).catch(() => {});
    return;
  }
  operation.pcmEnabled = false;
  operation.paused = true;
  await finishFileRecording(operation).catch(() => {});
  await dispose(operation);
  active.delete(operation.tabId);
  report({ state: "stopped", tabId: operation.tabId, streamKey: operation.streamKey, fileName: operation.fileName, fileError: operation.fileError });
}

async function pollMediaState(operation: Operation, socket?: WebSocket): Promise<void> {
  if (operation.mediaPollInFlight || operation.disposed || active.get(operation.tabId) !== operation) return;
  if (socket && !isCurrentSocket(operation, socket)) return;
  operation.mediaPollInFlight = true;
  try {
    const reply = await chrome.runtime.sendMessage({
      target: "service-worker",
      type: "read_tab_media",
      tabId: operation.tabId
    });
    if (operation.disposed || active.get(operation.tabId) !== operation || (socket && !isCurrentSocket(operation, socket))) return;
    const result = mediaResult(reply);
    reportMedia(operation, result);
    if (result.state === "ended" && operation.mediaEndArmed) void stopForMediaEnd(operation);
    if (socket) socket.send(JSON.stringify({
        type: "media_state",
        stream_key: operation.streamKey,
        state: result.state,
        ...(result.error ? { error: result.error } : {})
      }));
  } catch (error) {
    if (operation.disposed || active.get(operation.tabId) !== operation || (socket && !isCurrentSocket(operation, socket))) return;
    const result: MediaControlResult = { state: "unknown", error: errorMessage(error) };
    reportMedia(operation, result);
    try {
      if (!socket) return;
      socket.send(JSON.stringify({ type: "media_state", stream_key: operation.streamKey, state: result.state, error: result.error }));
    } catch {
      // メディア状態読取の失敗はPCM接続を停止しない。
    }
  } finally {
    operation.mediaPollInFlight = false;
  }
}

async function handleMediaCommand(operation: Operation, socket: WebSocket, control: Record<string, unknown>): Promise<void> {
  if (control.stream_key !== operation.streamKey
    || (control.action !== "play" && control.action !== "pause")
    || typeof control.request_id !== "string"
    || !control.request_id
    || !isCurrentSocket(operation, socket)) return;
  const action = control.action as MediaAction;
  let result: MediaControlResult;
  try {
    const reply = await chrome.runtime.sendMessage({
      target: "service-worker",
      type: "control_tab_media",
      tabId: operation.tabId,
      action
    });
    result = mediaResult(reply);
  } catch (error) {
    result = { state: "unknown", error: errorMessage(error) };
  }
  // 要求の待機中に再接続・停止済みなら、古い操作結果を新socketへ流さない。
  if (!isCurrentSocket(operation, socket)) return;
  reportMedia(operation, result);
  try {
    socket.send(JSON.stringify({
      type: "media_result",
      stream_key: operation.streamKey,
      request_id: control.request_id,
      state: result.state,
      ...(result.error ? { error: result.error } : {})
    }));
  } catch {
    // メディア制御失敗をPCM接続失敗へ昇格させない。
  }
}

async function stopForMediaEnd(operation: Operation): Promise<void> {
  if (!operation.mediaEndArmed || operation.disposed || active.get(operation.tabId) !== operation) return;
  operation.mediaEndArmed = false;
  operation.pcmEnabled = false;
  operation.paused = true;
  try {
    if (operation.socket?.readyState === WebSocket.OPEN) {
      operation.socket.send(JSON.stringify({ type: "capture_state", stream_key: operation.streamKey, state: "stopped" }));
    }
    if (operation.fileRecording) await finishFileRecording(operation);
    await dispose(operation);
    active.delete(operation.tabId);
    report({ state: "stopped", tabId: operation.tabId, streamKey: operation.streamKey, fileName: operation.fileName, fileError: operation.fileError });
  } catch (error) {
    await fail(operation, error);
  }
}

async function pauseOperation(tabId: number): Promise<void> {
  const operation = active.get(tabId);
  if (!operation || operation.disposed) return;
  operation.paused = true;
  operation.pcmEnabled = false;
  if (operation.firstPcmTimer !== undefined) clearTimeout(operation.firstPcmTimer);
  operation.firstPcmTimer = undefined;
  if (operation.fileRecorder?.state === "recording") operation.fileRecorder.pause();
  if (operation.socket?.readyState === WebSocket.OPEN) {
    operation.socket.send(JSON.stringify({ type: "capture_state", stream_key: operation.streamKey, state: "stopped" }));
  }
  report({ state: "stopped", tabId, streamKey: operation.streamKey, fileRecording: operation.fileRecording });
}

async function resumeOperation(tabId: number): Promise<void> {
  const operation = active.get(tabId);
  if (!operation || operation.disposed) throw new Error("再開するキャプチャがありません");
  operation.paused = false;
  operation.pcmEnabled = operation.pcmConfigured;
  if (operation.fileRecorder?.state === "paused") operation.fileRecorder.resume();
  if (operation.pcmEnabled && operation.socket?.readyState === WebSocket.OPEN) {
    operation.socket.send(JSON.stringify({ type: "capture_state", stream_key: operation.streamKey, state: "running" }));
    if (!operation.sentPcm) operation.firstPcmTimer = window.setTimeout(() => void fail(operation, new Error("PCM送信開始がタイムアウトしました。再生中のタブを選択してください")), PCM_TIMEOUT_MS);
  }
  report({ state: operation.pcmEnabled ? (operation.sentPcm ? "streaming" : "authenticated") : "stopped", tabId, streamKey: operation.streamKey, fileRecording: operation.fileRecording });
}

function waitForOpen(operation: Operation, socket: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (callback: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(operation.connectionTimer);
      operation.connectionTimer = undefined;
      operation.cancelWaiters.delete(cancel);
      callback();
    };
    const cancel = () => finish(() => reject(cancelled()));
    operation.cancelWaiters.add(cancel);
    operation.connectionTimer = window.setTimeout(() => finish(() => reject(new Error("VoiceMixerへの接続がタイムアウトしました"))), CONNECT_TIMEOUT_MS);
    socket.onopen = () => finish(resolve);
    socket.onerror = () => finish(() => reject(new Error("VoiceMixerへ接続できません")));
    socket.onclose = () => finish(() => reject(new Error("VoiceMixerとの接続が切断されました")));
    if (operation.cancelled) cancel();
  });
}

function waitForAccepted(operation: Operation, socket: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (callback: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(operation.authTimer);
      operation.authTimer = undefined;
      operation.cancelWaiters.delete(cancel);
      callback();
    };
    const cancel = () => finish(() => reject(cancelled()));
    operation.cancelWaiters.add(cancel);
    operation.authTimer = window.setTimeout(() => finish(() => reject(new Error("VoiceMixerの認証応答がタイムアウトしました"))), AUTH_TIMEOUT_MS);
    socket.onmessage = event => {
      try {
        const control = JSON.parse(String(event.data));
        if (control.type === "accepted") finish(resolve);
        if (control.type === "rejected") {
          const detail = control.reason === "stream_in_use" ? "このstream_keyは別の接続で使用中です" : `VoiceMixerに拒否されました: ${control.reason || "unknown"}`;
          finish(() => reject(new Error(detail)));
        }
      } catch {
        // 制御メッセージ以外は受理待ちに影響させない。
      }
    };
    socket.onerror = () => finish(() => reject(new Error("VoiceMixerとの通信中にエラーが発生しました")));
    socket.onclose = () => finish(() => reject(operation.cancelled ? cancelled() : new Error("VoiceMixerとの接続が切断されました")));
    if (operation.cancelled) cancel();
  });
}

async function releaseAudio(operation: Operation): Promise<void> {
  if (operation.firstPcmTimer !== undefined) clearTimeout(operation.firstPcmTimer);
  operation.firstPcmTimer = undefined;
  const processor = operation.processor;
  const source = operation.source;
  const sink = operation.sink;
  const recordingDestination = operation.recordingDestination;
  const stream = operation.stream;
  const context = operation.context;
  operation.processor = undefined;
  operation.source = undefined;
  operation.sink = undefined;
  operation.recordingDestination = undefined;
  operation.stream = undefined;
  operation.context = undefined;
  if (processor) {
    processor.port.onmessage = null;
    processor.port.close?.();
  }
  try { processor?.disconnect(); } catch { /* cleanup continues */ }
  try { source?.disconnect(); } catch { /* cleanup continues */ }
  try { sink?.disconnect(); } catch { /* cleanup continues */ }
  try { recordingDestination?.disconnect(); } catch { /* cleanup continues */ }
  stream?.getTracks().forEach(track => { track.onended = null; track.stop(); });
  try { await context?.close(); } catch { /* cleanup continues */ }
}

async function dispose(operation: Operation): Promise<void> {
  operation.disposed = true;
  for (const cancel of operation.cancelWaiters) cancel();
  if (operation.heartbeatTimer !== undefined) clearInterval(operation.heartbeatTimer);
  if (operation.mediaPollTimer !== undefined) clearInterval(operation.mediaPollTimer);
  if (operation.connectionTimer !== undefined) clearTimeout(operation.connectionTimer);
  if (operation.authTimer !== undefined) clearTimeout(operation.authTimer);
  const socket = operation.socket;
  operation.socket = undefined;
  if (socket) {
    socket.onopen = null;
    socket.onerror = null;
    socket.onmessage = null;
    socket.onclose = null;
    if (socket.readyState === WebSocket.OPEN) {
      try { socket.send(JSON.stringify({ type: "closing", stream_key: operation.streamKey })); } catch { /* cleanup continues */ }
    }
  }
  await releaseAudio(operation);
  try { socket?.close(); } catch { /* cleanup is best-effort */ }
}

function installControlChannel(operation: Operation): void {
  const socket = operation.socket!;
  socket.onmessage = event => {
    try {
      const control = JSON.parse(String(event.data)) as Record<string, unknown>;
      if (control.type === "media_command") {
        void handleMediaCommand(operation, socket, control);
        return;
      }
      if (control.stream_key !== operation.streamKey || (control.state !== "running" && control.state !== "stopped")) return;
      if (control.type === "capture_state_changed") {
        operation.lastPeerSeen = Date.now();
        operation.acknowledgedCaptureState = control.state;
      }
      if (control.type === "receiver_state") {
        operation.lastPeerSeen = Date.now();
        operation.receiverState = control.state;
        const current = statuses.get(operation.tabId);
        if (current) report({ ...current, receiverState: control.state });
      }
    } catch { /* 未知の制御メッセージは無視する */ }
  };
  socket.onclose = () => { if (!operation.disposed) void fail(operation, new Error("VoiceMixerとの接続が切断されました")); };
  socket.onerror = () => { if (!operation.disposed) void fail(operation, new Error("VoiceMixerとの通信中にエラーが発生しました")); };
  operation.heartbeatTimer = window.setInterval(() => {
    if (operation.disposed) return;
    if (operation.lastPeerSeen !== undefined && Date.now() - operation.lastPeerSeen > 5_000) {
      void fail(operation, new Error("VoiceMixerからの応答が途絶えました"));
      return;
    }
    try {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "heartbeat", stream_key: operation.streamKey }));
        void pollMediaState(operation, socket);
      }
    } catch (error) { void fail(operation, error); }
  }, 1_000);
}

async function start(tab: chrome.tabs.Tab, config: CaptureConfig, streamId: string, streamKey: string, mode: "pcm" | "file" = "pcm"): Promise<void> {
  if (!tab.id) throw new Error("タブIDを取得できません");
  const previous = active.get(tab.id);
  if (pending.has(tab.id) || (previous && !previous.paused) || stops.has(tab.id)) throw new Error("このタブは実行中または停止処理中です");
  if (mode === "pcm" && previous?.fileRecording) throw new Error("WebM保存中はPCM送信を開始できません");
  const settingsKey = JSON.stringify([config.port, config.token, streamKey]);
  const reuse = previous?.paused && previous.socket?.readyState === WebSocket.OPEN && !previous.fileRecording && previous.settingsKey === settingsKey;
  if (reuse && previous?.processor && previous.stream) {
    previous.returnAudio = config.returnAudio;
    if (previous.sink) previous.sink.gain.value = config.returnAudio ? 1 : 0;
    await resumeOperation(tab.id);
    return;
  }
  if (previous && !reuse) {
    await dispose(previous);
    active.delete(tab.id);
  }
  const operation: Operation = reuse ? previous! : { tabId: tab.id, streamKey, cancelled: false, disposed: false, cancelWaiters: new Set(), sentPcm: false, pcmConfigured: mode === "pcm", pcmEnabled: mode === "pcm", paused: mode !== "pcm", settingsKey, returnAudio: config.returnAudio, fileRecording: false, fileChunks: [], fileSegmentNumber: 1, mediaEndArmed: false };
  operation.returnAudio = config.returnAudio;
  operation.pcmEnabled = mode === "pcm";
  operation.paused = mode !== "pcm";
  operation.cancelled = false;
  operation.sentPcm = false;
  active.delete(operation.tabId);
  pending.set(operation.tabId, operation);
  report({ state: "starting", tabId: operation.tabId, streamKey });
  try {
    operation.stream = await navigator.mediaDevices.getUserMedia({
      audio: { mandatory: { chromeMediaSource: "tab", chromeMediaSourceId: streamId } } as MediaTrackConstraints,
      video: false
    });
    requireOwner(operation);
    if (!reuse && mode === "pcm") {
    operation.socket = new WebSocket(`ws://127.0.0.1:${config.port}/edge`);
    operation.socket.binaryType = "arraybuffer";
    report({ state: "connecting", tabId: operation.tabId, streamKey });
    await waitForOpen(operation, operation.socket);
    requireOwner(operation);
    const hello: EdgeHello = {
      type: "hello", stream_key: streamKey, token: config.token, protocol_version: 1,
      tab_id: operation.tabId, title: tab.title ?? "", url: tab.url ?? "", sample_rate: 48000, channels: 2
    };
    operation.socket.send(JSON.stringify(hello));
    await waitForAccepted(operation, operation.socket);
    requireOwner(operation);
    installControlChannel(operation);
    }
    if (mode === "file") operation.pcmEnabled = false;
    report({ state: mode === "pcm" ? "authenticated" : "stopped", tabId: operation.tabId, streamKey, fileRecording: operation.fileRecording });
    operation.context = new AudioContext({ sampleRate: 48000 });
    await operation.context.audioWorklet.addModule(chrome.runtime.getURL("audio-worklet.js"));
    requireOwner(operation);
    operation.source = operation.context.createMediaStreamSource(operation.stream);
    operation.processor = new AudioWorkletNode(operation.context, "voicemixer-pcm", { numberOfInputs: 1, numberOfOutputs: 1, channelCount: 2, channelCountMode: "explicit" });
    operation.sink = operation.context.createGain();
    operation.recordingDestination = operation.context.createMediaStreamDestination();
    operation.sink.gain.value = config.returnAudio ? 1 : 0;
    operation.source.connect(operation.processor);
    operation.processor.connect(operation.sink);
    operation.processor.connect(operation.recordingDestination);
    operation.sink.connect(operation.context.destination);
    await operation.context.resume();
    requireOwner(operation);
    pending.delete(operation.tabId);
    active.set(operation.tabId, operation);
    if (mode === "file") {
      beginFileRecording(operation);
      report({ state: "stopped", tabId: operation.tabId, streamKey, fileRecording: true });
      if (!operation.socket) operation.mediaPollTimer = window.setInterval(() => void pollMediaState(operation), 1_000);
    }
    if (operation.pcmEnabled) operation.socket!.send(JSON.stringify({ type: "capture_state", stream_key: streamKey, state: "running" }));
    operation.stream.getTracks().forEach(track => { track.onended = () => { void handleTrackEnded(operation); }; });
    if (operation.pcmEnabled) operation.firstPcmTimer = window.setTimeout(() => void fail(operation, new Error("PCM送信開始がタイムアウトしました。再生中のタブを選択してください")), PCM_TIMEOUT_MS);
    operation.processor.port.onmessage = event => {
      if (operation.disposed || (!operation.fileRecording && !operation.pcmEnabled)) return;
      const samples = new Float32Array(event.data);
      try {
        if (operation.pcmEnabled && operation.socket?.readyState === WebSocket.OPEN) operation.socket.send(pcmFrameBytes(samples));
        if (operation.pcmEnabled && !operation.sentPcm) {
          operation.sentPcm = true;
          clearTimeout(operation.firstPcmTimer);
          report({ state: "streaming", tabId: operation.tabId, streamKey: operation.streamKey });
        }
      } catch (error) {
        void fail(operation, error);
      }
    };
  } catch (error) {
    const owned = pending.get(operation.tabId) === operation || active.get(operation.tabId) === operation;
    await dispose(operation);
    if (pending.get(operation.tabId) === operation) pending.delete(operation.tabId);
    if (active.get(operation.tabId) === operation) active.delete(operation.tabId);
    if (owned) report(isCancelled(error) ? { state: "stopped", tabId: operation.tabId, streamKey } : { state: "failed", tabId: operation.tabId, streamKey, message: errorMessage(error) });
    throw error;
  }
}

async function fail(operation: Operation, error: unknown): Promise<void> {
  if (active.get(operation.tabId) !== operation && pending.get(operation.tabId) !== operation) return;
  operation.cancelled = true;
  if (operation.fileRecording) await finishFileRecording(operation).catch(() => {});
  await dispose(operation);
  if (pending.get(operation.tabId) === operation) pending.delete(operation.tabId);
  if (active.get(operation.tabId) === operation) active.delete(operation.tabId);
  report({ state: "failed", tabId: operation.tabId, streamKey: operation.streamKey, message: errorMessage(error) });
}

async function stop(tabId: number): Promise<void> {
  const existing = stops.get(tabId);
  if (existing) return existing;
  const task = stopOperation(tabId);
  stops.set(tabId, task);
  try { await task; } finally { if (stops.get(tabId) === task) stops.delete(tabId); }
}

async function stopOperation(tabId: number): Promise<void> {
  const operation = active.get(tabId) || pending.get(tabId);
  if (!operation) {
    report({ state: "stopped", tabId, streamKey: statuses.get(tabId)?.streamKey });
    return;
  }
  if (operation.fileRecording) throw new Error("WebM保存を停止してからPCM送信を停止してください");
  operation.paused = true;
  operation.pcmEnabled = false;
  const wasPending = pending.get(tabId) === operation;
  report({ state: "stopping", tabId: operation.tabId, streamKey: operation.streamKey });
  if (wasPending) {
    operation.cancelled = true;
    await dispose(operation);
    // 遅れて返るgetUserMediaもstart側が解放するまで、再取得を許可しない。
    await starts.get(tabId)?.catch(() => {});
  } else {
    try {
      operation.socket?.send(JSON.stringify({ type: "capture_state", stream_key: operation.streamKey, state: "stopped" }));
      await releaseAudio(operation);
    } catch (error) {
      await fail(operation, error);
      throw error;
    }
  }
  if (!wasPending && (operation.disposed || operation.socket?.readyState !== WebSocket.OPEN)) {
    await fail(operation, new Error("VoiceMixerとの接続が切断されました"));
    throw new Error("VoiceMixerとの接続が切断されました");
  }
  report({ state: "stopped", tabId, streamKey: operation.streamKey, receiverState: operation.receiverState });
}

async function startFile(tab: chrome.tabs.Tab, config: CaptureConfig, streamId: string, streamKey: string): Promise<void> {
  if (!tab.id) throw new Error("タブIDを取得できません");
  const previous = active.get(tab.id);
  if (pending.has(tab.id) || stops.has(tab.id)) throw new Error("このタブは開始・停止処理中です");
  if (previous) {
    if (previous.fileRecording) throw new Error("このタブはWebM保存中です");
    if (previous.socket?.readyState === WebSocket.OPEN && previous.processor) {
      beginFileRecording(previous);
      report({ state: previous.pcmEnabled ? (previous.sentPcm ? "streaming" : "authenticated") : "stopped", tabId: previous.tabId, streamKey: previous.streamKey, fileRecording: true });
      return;
    }
  }
  await start(tab, config, streamId, streamKey, "file");
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.target !== "offscreen") return false;
  if (message.type === "offscreen_start") {
    const tabId = Number(message.tab?.id);
    if (starts.has(tabId)) { sendResponse({ ok: false, error: "このタブは実行中です" }); return false; }
    const task = start(message.tab, { ...DEFAULT_SETTINGS, ...message.config }, String(message.streamId), String(message.streamKey));
    starts.set(tabId, task);
    void task
      .then(() => sendResponse({ ok: true }))
      .catch(error => sendResponse({ ok: false, error: errorMessage(error) }))
      .finally(() => { if (starts.get(tabId) === task) starts.delete(tabId); });
    return true;
  }
  if (message.type === "offscreen_start_file") {
    const tabId = Number(message.tab?.id);
    if (starts.has(tabId)) { sendResponse({ ok: false, error: "このタブは開始処理中です" }); return false; }
    const task = startFile(message.tab, { ...DEFAULT_SETTINGS, ...message.config }, String(message.streamId ?? ""), String(message.streamKey));
    starts.set(tabId, task);
    void task
      .then(() => sendResponse({ ok: true }))
      .catch(error => sendResponse({ ok: false, error: errorMessage(error) }))
      .finally(() => { if (starts.get(tabId) === task) starts.delete(tabId); });
    return true;
  }
  if (message.type === "offscreen_stop") {
    void stop(Number(message.tabId)).then(() => sendResponse({ ok: true })).catch(error => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  if (message.type === "offscreen_stop_file") {
    const tabId = Number(message.tabId);
    const operation = active.get(tabId);
    if (!operation?.fileRecording) { sendResponse({ ok: false, error: "WebM保存は開始されていません" }); return false; }
    void finishFileRecording(operation).then(() => sendResponse({ ok: true })).catch(error => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  if (message.type === "offscreen_pause") {
    void pauseOperation(Number(message.tabId)).then(() => sendResponse({ ok: true })).catch(error => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  if (message.type === "offscreen_resume") {
    void resumeOperation(Number(message.tabId)).then(() => sendResponse({ ok: true })).catch(error => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  if (message.type === "offscreen_arm_media_end") {
    const operation = active.get(Number(message.tabId));
    if (!operation || operation.disposed) { sendResponse({ ok: false, error: "キャプチャが開始されていません" }); return false; }
    operation.mediaEndArmed = true;
    sendResponse({ ok: true });
    return false;
  }
  if (message.type === "offscreen_get_statuses") {
    sendResponse({ ok: true, statuses: allStatuses() });
    return false;
  }
  if (message.type === "offscreen_set_return_audio") {
    const tabId = Number(message.tabId);
    if (pending.has(tabId) || stops.has(tabId)) {
      sendResponse({ ok: false, error: "開始・停止処理の完了を待ってください" });
      return false;
    }
    const operation = active.get(tabId);
    if (operation && !operation.disposed) {
      operation.returnAudio = Boolean(message.returnAudio);
      if (operation.sink) operation.sink.gain.value = operation.returnAudio ? 1 : 0;
    }
    const status = statuses.get(tabId);
    if (status) report({ ...status, returnAudio: Boolean(message.returnAudio) });
    sendResponse({ ok: true });
    return false;
  }
  if (message.type === "offscreen_forget") {
    const tabId = Number(message.tabId);
    const operation = active.get(tabId);
    if (starts.has(tabId) || stops.has(tabId) || pending.has(tabId) || operation?.fileRecording || (operation && !operation.paused)) {
      sendResponse({ ok: false, error: "停止完了後にストリームキーを変更してください" });
      return false;
    }
    void (async () => {
      if (operation) await dispose(operation);
      if (active.get(tabId) === operation) active.delete(tabId);
      statuses.delete(tabId);
      sendResponse({ ok: true });
    })().catch(error => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  const operation = active.get(Number(message.tabId));
  if (message.type === "tab_updated" && operation?.socket?.readyState === WebSocket.OPEN) {
    operation.socket.send(JSON.stringify({ type: "tab_updated", stream_key: operation.streamKey, title: message.title ?? "", url: message.url ?? "" }));
  }
  if (message.type === "tab_closed") {
    const tabId = Number(message.tabId);
    void (async () => {
      const current = active.get(tabId);
      if (current?.fileRecording) {
        current.pcmEnabled = false;
        current.paused = true;
        await finishFileRecording(current).catch(() => {});
        await dispose(current);
      } else {
        await stop(tabId).catch(() => {});
      }
      const remaining = active.get(tabId);
      if (remaining) await dispose(remaining);
      active.delete(tabId);
      report({ state: "idle", tabId });
    })().catch(() => {});
  }
  return false;
});
