import {
  isBusy,
  loadLegacyTabSettings,
  loadSettings,
  type CaptureConfig,
  type CaptureStatus,
  type MediaAction,
  type MediaControlResult,
  type TabSettings
} from "./protocol";
import { controlTabMedia, readTabMedia } from "./media-control";

const statuses = new Map<number, CaptureStatus>();
const captureLocks = new Set<number>();
const stopLocks = new Set<number>();
const wavLocks = new Set<number>();
const settingsLocks = new Set<number>();
const generations = new Map<number, number>();
const tabSettings = new Map<number, TabSettings>();
const mediaStatuses = new Map<number, MediaControlResult>();
const mediaOperations = new Map<number, Promise<MediaControlResult>>();
const TAB_SETTINGS_STORAGE_KEY = "voiceMixerTabSettingsV047";
const BROWSER_SESSION_KEY = "voiceMixerBrowserSessionV047";
let tabSettingsRestore: Promise<void> | undefined;
let tabSettingsWrite: Promise<void> = Promise.resolve();
let browserSessionId: string | undefined;
let tabRegistration: Promise<void> = Promise.resolve();
// 旧版が保存したアイコン動作も明示的に解除する。
void chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: false });

async function settings(): Promise<Awaited<ReturnType<typeof loadSettings>>> {
  return loadSettings();
}

async function restoreTabSettings(): Promise<void> {
  if (tabSettingsRestore) return tabSettingsRestore;
  tabSettingsRestore = (async () => {
    const stored = await chrome.storage.session.get(TAB_SETTINGS_STORAGE_KEY);
    const records = stored[TAB_SETTINGS_STORAGE_KEY] as Record<string, TabSettings> | undefined;
    for (const [tabId, value] of Object.entries(records ?? {})) {
      if (Number.isInteger(Number(tabId)) && value && typeof value === "object") {
        tabSettings.set(Number(tabId), value);
      }
    }
  })();
  return tabSettingsRestore;
}

async function persistTabSettings(): Promise<void> {
  const write = tabSettingsWrite.then(async () => {
    const records: Record<string, TabSettings> = {};
    for (const [tabId, value] of tabSettings) records[String(tabId)] = value;
    await chrome.storage.session.set({ [TAB_SETTINGS_STORAGE_KEY]: records });
  });
  tabSettingsWrite = write.catch(() => {});
  await write;
}

async function automaticStreamKey(tabId: number): Promise<string> {
  if (!browserSessionId) {
    const stored = await chrome.storage.session.get(BROWSER_SESSION_KEY);
    browserSessionId = typeof stored[BROWSER_SESSION_KEY] === "string"
      ? stored[BROWSER_SESSION_KEY]
      : crypto.randomUUID();
    if (stored[BROWSER_SESSION_KEY] !== browserSessionId) {
      await chrome.storage.session.set({ [BROWSER_SESSION_KEY]: browserSessionId });
    }
  }
  return `tab-${browserSessionId}-${tabId}`;
}

async function tabSettingsFor(tab: chrome.tabs.Tab): Promise<TabSettings> {
  if (tab.id === undefined) throw new Error("タブIDを取得できません");
  await restoreTabSettings();
  const existing = tabSettings.get(tab.id);
  if (existing) return existing;
  await loadSettings();
  // 旧global設定は最初に登録したタブだけへ移行する。以後のタブへ
  // 同じ固定キーを複製して衝突させない。
  const legacy = tabSettings.size === 0 ? await loadLegacyTabSettings() : {};
  const value: TabSettings = {
    streamKey: legacy.streamKey || await automaticStreamKey(tab.id),
    streamKeyExplicit: legacy.streamKeyExplicit ?? false,
    returnAudio: legacy.returnAudio ?? false,
    title: tab.title ?? "",
    url: tab.url ?? ""
  };
  tabSettings.set(tab.id, value);
  await persistTabSettings();
  return value;
}

function decorateStatus(status: CaptureStatus): CaptureStatus {
  if (status.tabId === undefined) return status;
  const saved = tabSettings.get(status.tabId);
  const media = mediaStatuses.get(status.tabId);
  const decorated = saved ? {
    ...status,
    streamKey: status.streamKey ?? saved.streamKey,
    returnAudio: saved.returnAudio,
    title: saved.title,
    url: saved.url
  } : status;
  return media ? {
    ...decorated,
    mediaState: media.state,
    mediaError: media.error
  } : decorated;
}

function rememberMediaStatus(tabId: number, result: MediaControlResult): MediaControlResult {
  mediaStatuses.set(tabId, result);
  const status = statuses.get(tabId);
  if (status) statuses.set(tabId, decorateStatus(status));
  return result;
}

function runMediaOperation(tabId: number, operation: () => Promise<MediaControlResult>): Promise<MediaControlResult> {
  const existing = mediaOperations.get(tabId);
  // heartbeatの読取は実行中の操作へ参加し、後続のscript注入を増やさない。
  if (existing) return existing;
  const task = operation();
  mediaOperations.set(tabId, task);
  void task.finally(() => {
    if (mediaOperations.get(tabId) === task) mediaOperations.delete(tabId);
  }).catch(() => {});
  return task;
}

async function requireRegisteredTab(tabId: number): Promise<void> {
  await restoreTabSettings();
  if (!tabSettings.has(tabId)) {
    throw new Error("VoiceMixerアイコンで登録したタブだけを操作できます");
  }
}

async function readRegisteredTabMedia(tabId: number): Promise<MediaControlResult> {
  try {
    await requireRegisteredTab(tabId);
    return rememberMediaStatus(tabId, await runMediaOperation(tabId, () => readTabMedia(tabId)));
  } catch (error) {
    return rememberMediaStatus(tabId, { state: "unknown", error: error instanceof Error ? error.message : String(error) });
  }
}

async function controlRegisteredTabMedia(tabId: number, action: MediaAction, positionSeconds?: number): Promise<MediaControlResult> {
  try {
    await requireRegisteredTab(tabId);
    // 二重clickや本体からの連打を待ち行列化すると、期限切れ後に古い play が
    // 実行される。明示的に拒否して操作の順序を保つ。
    if (mediaOperations.has(tabId)) {
      return rememberMediaStatus(tabId, { state: "unknown", error: "このタブのメディア操作を処理中です" });
    }
    return rememberMediaStatus(tabId, await runMediaOperation(tabId, () => controlTabMedia(tabId, action, positionSeconds)));
  } catch (error) {
    return rememberMediaStatus(tabId, { state: "unknown", error: error instanceof Error ? error.message : String(error) });
  }
}

let offscreenCreation: Promise<void> | undefined;
async function ensureOffscreen(): Promise<void> {
  if (chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) return;
  if (offscreenCreation) return offscreenCreation;
  offscreenCreation = chrome.offscreen.createDocument({
    url: "src/offscreen.html",
    reasons: [chrome.offscreen.Reason.USER_MEDIA, chrome.offscreen.Reason.BLOBS],
    justification: "タブ音声をAudioWorkletからPCMへ変換し、WebM/Opus音声を分割保存するため"
  }).finally(() => { offscreenCreation = undefined; });
  return offscreenCreation;
}

function setStatus(next: CaptureStatus): void {
  if (next.tabId === undefined) return;
  if (next.state === "idle") statuses.delete(next.tabId);
  else statuses.set(next.tabId, decorateStatus(next));
}

function allStatuses(): CaptureStatus[] {
  const merged = new Map(statuses);
  for (const [tabId, saved] of tabSettings) {
    if (!merged.has(tabId)) {
      merged.set(tabId, {
        state: "idle",
        tabId,
        streamKey: saved.streamKey,
        returnAudio: saved.returnAudio,
        title: saved.title,
        url: saved.url
      });
    }
  }
  return [...merged.values()].map(decorateStatus).sort((a, b) => (a.tabId ?? 0) - (b.tabId ?? 0));
}

function statusFor(tabId: number): CaptureStatus {
  return decorateStatus(statuses.get(tabId) ?? { state: "idle", tabId });
}

function nextGeneration(tabId: number): number {
  const value = (generations.get(tabId) ?? 0) + 1;
  generations.set(tabId, value);
  return value;
}

function mediaStreamId(tabId: number): Promise<string> {
  return new Promise((resolve, reject) => {
    chrome.tabCapture.getMediaStreamId({ targetTabId: tabId }, streamId => {
      const error = chrome.runtime.lastError;
      if (error || !streamId) reject(new Error(error?.message || "タブ音声の取得が拒否されました"));
      else resolve(streamId);
    });
  });
}

async function waitForCaptureRelease(tabId: number): Promise<void> {
  const deadline = Date.now() + 3_000;
  while (Date.now() < deadline) {
    const tabs = await new Promise<chrome.tabCapture.CaptureInfo[]>((resolve, reject) => chrome.tabCapture.getCapturedTabs(items => {
      const error = chrome.runtime.lastError;
      if (error) reject(new Error(error.message)); else resolve(items);
    }));
    const entry = tabs.find(item => item.tabId === tabId);
    if (!entry || entry.status === "stopped" || entry.status === "error") return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error("前回のタブ音声キャプチャの解放を待機中です");
}

async function restoreStatuses(): Promise<void> {
  await restoreTabSettings();
  if (!chrome.offscreen?.hasDocument || !await chrome.offscreen.hasDocument()) return;
  const result = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_get_statuses" });
  if (!Array.isArray(result?.statuses)) return;
  const reported = new Map<number, CaptureStatus>();
  for (const item of result.statuses as CaptureStatus[]) if (item.tabId !== undefined) reported.set(item.tabId, item);
  for (const [tabId, old] of statuses) {
    if (!captureLocks.has(tabId) && !stopLocks.has(tabId) && !settingsLocks.has(tabId) && old.state !== "failed" && old.state !== "stopped" && !reported.has(tabId)) {
      statuses.delete(tabId);
    }
  }
  for (const item of reported.values()) if (!captureLocks.has(item.tabId!) && !stopLocks.has(item.tabId!) && !settingsLocks.has(item.tabId!)) setStatus(item);
}

function requireGeneration(tabId: number, expected: number): void {
  if (generations.get(tabId) !== expected) throw new Error("キャプチャを停止しました");
}

async function downloadMedia(url: unknown, filename: unknown): Promise<number> {
  if (typeof url !== "string" || (!url.startsWith("blob:") && !url.startsWith("data:audio/"))
    || typeof filename !== "string" || !/\.(webm|wav)$/i.test(filename)) {
    throw new Error("音声保存データが不正です");
  }
  const downloadId = await chrome.downloads.download({ url, filename, saveAs: false, conflictAction: "uniquify" });
  if (downloadId === undefined) throw new Error("音声ファイルの保存を開始できませんでした");
  return downloadId;
}

function startPositionSeconds(value: unknown): number | undefined {
  if (value === undefined || value === null || value === "") return undefined;
  const seconds = Number(value);
  if (!Number.isFinite(seconds) || seconds < 0) throw new Error("開始位置は0以上で指定してください");
  return seconds;
}

async function playForCapture(tabId: number, positionSeconds?: number): Promise<void> {
  const result = await controlRegisteredTabMedia(tabId, "play", positionSeconds);
  if (result.state !== "playing" || result.error) return;
  if (chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) {
    await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_arm_media_end", tabId });
  }
}

async function pausePageMedia(tabId: number): Promise<void> {
  if (!chrome.offscreen?.hasDocument || !await chrome.offscreen.hasDocument()) return;
  await controlRegisteredTabMedia(tabId, "pause");
}

function streamKeyInUse(streamKey: string, tabId: number): boolean {
  return [...tabSettings.entries()].some(([otherId, value]) => otherId !== tabId && value.streamKey === streamKey)
    || allStatuses().some(status => status.tabId !== tabId && status.streamKey === streamKey && isBusy(status));
}

// IDは数秒で失効するため、ここでは対象だけ保存する。
chrome.action.onClicked.addListener(tab => {
  if (tab.id === undefined || tab.windowId === undefined) return;
  // ユーザー操作の同期区間で開く。音声取得の成否とパネル表示を分離する。
  void chrome.sidePanel.open({ windowId: tab.windowId }).catch(console.error);
  tabRegistration = tabRegistration.then(async () => {
    const value = await chrome.storage.session.get("captureTargetTabIds");
    const ids = new Set<number>(Array.isArray(value.captureTargetTabIds) ? value.captureTargetTabIds : []);
    ids.add(tab.id!);
    await chrome.storage.session.set({ captureTargetTabIds: [...ids], captureTargetTabId: tab.id });
    // 同時に複数タブを登録しても、旧固定キーを複数へ移行しない。
    await tabSettingsFor(tab);
  }).catch(console.error);
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.target !== "service-worker") return false;
  void (async () => {
    if (message.type === "capture_status") {
      setStatus(message.status as CaptureStatus);
      sendResponse({ ok: true });
      return;
    }
    if (message.type === "download_media") {
      const downloadId = await downloadMedia(message.url, message.filename);
      sendResponse({ ok: true, downloadId });
      return;
    }
    if (message.type === "get_capture_status") {
      await restoreStatuses();
      sendResponse({ ok: true, statuses: allStatuses(), status: allStatuses()[0] ?? { state: "idle" } });
      return;
    }
    if (message.type === "read_tab_media") {
      const tabId = Number(message.tabId);
      if (!Number.isInteger(tabId)) throw new Error("対象タブを選択してください");
      const result = await readRegisteredTabMedia(tabId);
      sendResponse({ ok: true, ...result, status: statusFor(tabId), statuses: allStatuses() });
      return;
    }
    if (message.type === "control_tab_media") {
      const tabId = Number(message.tabId);
      const action = message.action as MediaAction;
      if (!Number.isInteger(tabId)) throw new Error("対象タブを選択してください");
      if (action !== "play" && action !== "pause") throw new Error("再生操作が不正です");
      const positionSeconds = action === "play" ? startPositionSeconds(message.positionSeconds) : undefined;
      const result = await controlRegisteredTabMedia(tabId, action, positionSeconds);
      if (message.syncCapture === true) {
        if (action === "play" && result.state === "playing" && !result.error) {
          const resumed = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_resume", tabId });
          if (!resumed?.ok) throw new Error(resumed?.error || "録音・転送の再開に失敗しました");
          await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_arm_media_end", tabId });
        } else if (action === "pause" && !result.error) {
          const paused = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_pause", tabId });
          if (!paused?.ok) throw new Error(paused?.error || "録音・転送の一時停止に失敗しました");
        }
      }
      const ok = !result.error && (result.state === "playing" || result.state === "paused" || result.state === "ended");
      sendResponse({ ok, ...result, status: statusFor(tabId), statuses: allStatuses() });
      return;
    }
    if (message.type === "update_tab_settings") {
      const tabId = Number(message.tabId);
      if (!Number.isInteger(tabId)) throw new Error("対象タブを選択してください");
      await restoreStatuses();
      if (captureLocks.has(tabId) || stopLocks.has(tabId) || settingsLocks.has(tabId)) {
        sendResponse({ ok: false, error: "このタブは実行中です", statuses: allStatuses() });
        return;
      }
      settingsLocks.add(tabId);
      try {
        const tab = await chrome.tabs.get(tabId);
        const current = await tabSettingsFor(tab);
        const requestedKey = message.streamKey === undefined ? current.streamKey : String(message.streamKey).trim();
        if (requestedKey !== current.streamKey && isBusy(statusFor(tabId))) {
          throw new Error("Streamキーは停止してから変更してください");
        }
        if (!requestedKey) throw new Error("Streamキーを入力してください");
        if (requestedKey !== current.streamKey && streamKeyInUse(requestedKey, tabId)) {
          throw new Error("このStreamキーは別のタブで使用中です");
        }
        if (requestedKey !== current.streamKey && chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) {
          const forgotten = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_forget", tabId });
          if (!forgotten?.ok) throw new Error(forgotten?.error || "以前の接続を解除できませんでした");
        }
        const updated: TabSettings = {
          ...current,
          streamKey: requestedKey,
          streamKeyExplicit: message.streamKey === undefined ? current.streamKeyExplicit : true,
          returnAudio: message.returnAudio === undefined ? current.returnAudio : Boolean(message.returnAudio),
          title: tab.title ?? current.title,
          url: tab.url ?? current.url
        };
        if (message.returnAudio !== undefined && chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) {
          const changed = await chrome.runtime.sendMessage({
            target: "offscreen", type: "offscreen_set_return_audio", tabId, returnAudio: updated.returnAudio
          });
          if (!changed?.ok) throw new Error(changed?.error || "タブ音声戻しの変更に失敗しました");
        }
        tabSettings.set(tabId, updated);
        try { await persistTabSettings(); } catch (error) {
          tabSettings.set(tabId, current);
          if (message.returnAudio !== undefined && chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) {
            await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_set_return_audio", tabId, returnAudio: current.returnAudio });
          }
          throw error;
        }
        setStatus({
          ...statusFor(tabId), tabId, streamKey: updated.streamKey,
          returnAudio: updated.returnAudio, title: updated.title, url: updated.url
        });
        sendResponse({ ok: true, status: statusFor(tabId), statuses: allStatuses() });
      } finally {
        settingsLocks.delete(tabId);
      }
      return;
    }
    if (message.type === "capture_tab") {
      const selectedTabId = Number(message.tabId);
      if (!Number.isInteger(selectedTabId)) {
        sendResponse({ ok: false, error: "対象タブを選択してください" });
        return;
      }
      await restoreStatuses();
      if (captureLocks.has(selectedTabId) || stopLocks.has(selectedTabId) || wavLocks.has(selectedTabId) || settingsLocks.has(selectedTabId) || isBusy(statusFor(selectedTabId)) || statusFor(selectedTabId).fileRecording) {
        sendResponse({ ok: false, error: "このタブは実行中です", status: statusFor(selectedTabId), statuses: allStatuses() });
        return;
      }
      captureLocks.add(selectedTabId);
      try {
        const requestGeneration = nextGeneration(selectedTabId);
        const target = await chrome.storage.session.get("captureTargetTabIds");
        const targets = Array.isArray(target.captureTargetTabIds) ? target.captureTargetTabIds : [];
        if (!targets.includes(selectedTabId)) {
          throw new Error("対象タブを前面にしてVoiceMixerアイコンを押し、サイドパネルを開き直してください。");
        }
        const tab = await chrome.tabs.get(selectedTabId);
        requireGeneration(selectedTabId, requestGeneration);
        if (!tab.id) throw new Error("タブIDを取得できません");
        const common = await settings();
        const tabConfig = await tabSettingsFor(tab);
        requireGeneration(selectedTabId, requestGeneration);
        if (!Number.isInteger(common.port) || common.port < 1 || common.port > 65535) {
          throw new Error("Backendポートは1から65535で設定してください。設定画面を開いて確認してください。");
        }
        if (!common.token.trim()) {
          throw new Error("認証トークンが未設定です。設定画面でRunnerと同じ値を保存してください。");
        }
        if (streamKeyInUse(tabConfig.streamKey, tab.id)) {
          throw new Error("このStreamキーは別のタブで使用中です");
        }
        const streamKey = tabConfig.streamKey;
        const config: CaptureConfig = { ...common, streamKey, returnAudio: tabConfig.returnAudio };
        setStatus({ state: "starting", tabId: tab.id, streamKey });
        await ensureOffscreen();
        requireGeneration(selectedTabId, requestGeneration);
        await waitForCaptureRelease(tab.id);
        requireGeneration(selectedTabId, requestGeneration);
        const streamId = await mediaStreamId(tab.id);
        requireGeneration(selectedTabId, requestGeneration);
        const result = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_start", tab, config, streamId, streamKey });
        requireGeneration(selectedTabId, requestGeneration);
        if (!result?.ok) throw new Error(result?.error || "タブ音声の開始に失敗しました");
        await playForCapture(tab.id, startPositionSeconds(message.startPositionSeconds));
        sendResponse({ ok: true, status: statusFor(selectedTabId), statuses: allStatuses() });
      } finally {
        captureLocks.delete(selectedTabId);
      }
      return;
    }
    if (message.type === "save_wav") {
      const tabId = Number(message.tabId);
      if (!Number.isInteger(tabId)) {
        sendResponse({ ok: false, error: "保存対象タブを選択してください" });
        return;
      }
      await restoreStatuses();
      const currentStatus = statusFor(tabId);
      if (captureLocks.has(tabId) || stopLocks.has(tabId) || wavLocks.has(tabId) || settingsLocks.has(tabId) || ["starting", "connecting", "authenticated", "stopping"].includes(currentStatus.state)) {
        sendResponse({ ok: false, error: "このタブは開始・停止処理中です", status: currentStatus, statuses: allStatuses() });
        return;
      }
      wavLocks.add(tabId);
      try {
        if (currentStatus.fileRecording) {
          if (!chrome.offscreen?.hasDocument || !await chrome.offscreen.hasDocument()) throw new Error("WebM保存の状態を取得できません");
          const result = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_stop_file", tabId });
          if (!result?.ok) throw new Error(result?.error || "WebM保存の停止に失敗しました");
          await pausePageMedia(tabId);
          sendResponse({ ok: true, status: statusFor(tabId), statuses: allStatuses() });
          return;
        }
        const tab = await chrome.tabs.get(tabId);
        const common = await settings();
        const tabConfig = await tabSettingsFor(tab);
        if (!Number.isInteger(common.port) || common.port < 1 || common.port > 65535) throw new Error("Backendポートは1から65535で設定してください。設定画面を開いて確認してください。");
        const livePcm = isBusy(currentStatus);
        const streamId = livePcm ? "" : await mediaStreamId(tabId);
        await ensureOffscreen();
        const result = await chrome.runtime.sendMessage({
          target: "offscreen", type: "offscreen_start_file", tab, config: { ...common, streamKey: tabConfig.streamKey, returnAudio: tabConfig.returnAudio }, streamId, streamKey: tabConfig.streamKey
        });
        if (!result?.ok) throw new Error(result?.error || "WebM保存の開始に失敗しました");
        await playForCapture(tabId, startPositionSeconds(message.startPositionSeconds));
        sendResponse({ ok: true, status: statusFor(tabId), statuses: allStatuses() });
      } finally {
        wavLocks.delete(tabId);
      }
      return;
    }
    if (message.type === "stop_capture") {
      const tabId = Number(message.tabId);
      if (!Number.isInteger(tabId)) throw new Error("停止対象タブを選択してください");
      if (stopLocks.has(tabId) || wavLocks.has(tabId) || settingsLocks.has(tabId)) {
        sendResponse({ ok: false, error: "このタブは停止処理中です", statuses: allStatuses() });
        return;
      }
      stopLocks.add(tabId);
      try {
      const previous = statusFor(tabId);
      nextGeneration(tabId);
      setStatus({ ...previous, state: "stopping" });
      if (chrome.offscreen?.hasDocument && await chrome.offscreen.hasDocument()) {
        const result = await chrome.runtime.sendMessage({ target: "offscreen", type: "offscreen_stop", tabId });
        if (!result?.ok) throw new Error(result?.error || "停止に失敗しました");
        await pausePageMedia(tabId);
        await waitForCaptureRelease(tabId);
        setStatus({ ...statusFor(tabId), state: "stopped", tabId, streamKey: statusFor(tabId).streamKey ?? previous.streamKey });
      } else {
        setStatus({ ...previous, state: "stopped", tabId });
      }
      sendResponse({ ok: true, statuses: allStatuses() });
      } finally { stopLocks.delete(tabId); }
      return;
    }
    sendResponse({ ok: false, error: "unknown_message" });
  })().catch(error => {
    const detail = error instanceof Error ? error.message : String(error);
    const tabId = Number(message.tabId);
    if (detail !== "キャプチャを停止しました" && message.type !== "update_tab_settings" && Number.isInteger(tabId)) setStatus({ ...statusFor(tabId), state: "failed", tabId, message: detail });
    sendResponse({ ok: false, error: detail, status: Number.isInteger(tabId) ? statusFor(tabId) : { state: "failed", message: detail }, statuses: allStatuses() });
  });
  return true;
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (tabSettings.has(tabId) && (changeInfo.title || changeInfo.url)) {
    const current = tabSettings.get(tabId)!;
    tabSettings.set(tabId, { ...current, title: tab.title ?? current.title, url: tab.url ?? current.url });
    void persistTabSettings();
  }
  if (statuses.has(tabId) && (changeInfo.title || changeInfo.url)) {
    void chrome.runtime.sendMessage({ target: "offscreen", type: "tab_updated", tabId, title: tab.title ?? "", url: tab.url ?? "" });
  }
});

chrome.tabs.onRemoved.addListener(tabId => {
  void (async () => {
    await restoreTabSettings();
    statuses.delete(tabId);
    mediaStatuses.delete(tabId);
    tabSettings.delete(tabId);
    await persistTabSettings();
    await chrome.runtime.sendMessage({ target: "offscreen", type: "tab_closed", tabId });
  })().catch(console.error);
});
