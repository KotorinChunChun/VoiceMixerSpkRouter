export type EdgeHello = {
  type: "hello";
  stream_key: string;
  token: string;
  protocol_version: 1;
  tab_id: number;
  title: string;
  url: string;
  sample_rate: number;
  channels: 1 | 2;
};

export type CommonSettings = {
  port: number;
  token: string;
};

export type TabSettings = {
  returnAudio: boolean;
  streamKey: string;
  title: string;
  url: string;
  /** session内だけで使う自動キー判定。UI契約には露出しない。 */
  streamKeyExplicit?: boolean;
};

/** offscreen開始時だけ、共通設定とタブ設定を合成する。 */
export type CaptureConfig = CommonSettings & Pick<TabSettings, "streamKey" | "returnAudio">;

/** 旧offscreen利用者向けの別名。新規コードはCaptureConfigを使う。 */
export type EdgeSettings = CaptureConfig;

export const DEFAULT_SETTINGS: CommonSettings = {
  port: 38471,
  token: "runner-scenario-local-test"
};

const SETTINGS_VERSION = 3;
const SETTINGS_VERSION_KEY = "settingsVersion";
const LEGACY_TAB_SETTINGS_KEY = "legacyTabSettingsV047";

/**
 * v0.4.4より前はトークンと固定ストリームキーが空欄だった。
 * 明示保存された値は保護し、旧来の空欄だけをRunnerの既定値へ移行する。
 */
export async function loadSettings(): Promise<CommonSettings> {
  const stored = await chrome.storage.local.get([
    "port", "token", "returnAudio", "streamKey", SETTINGS_VERSION_KEY, LEGACY_TAB_SETTINGS_KEY
  ]);
  const isLegacy = stored[SETTINGS_VERSION_KEY] !== SETTINGS_VERSION;
  const storedToken = typeof stored.token === "string" ? stored.token : DEFAULT_SETTINGS.token;
  const result: CommonSettings = {
    port: typeof stored.port === "number" ? stored.port : DEFAULT_SETTINGS.port,
    token: stored[SETTINGS_VERSION_KEY] !== 2 && isLegacy && !storedToken.trim() ? DEFAULT_SETTINGS.token : storedToken
  };
  if (isLegacy) {
    const legacyKey = String(stored.streamKey ?? "").trim();
    const legacy: Partial<TabSettings> = {};
    if (legacyKey) {
      legacy.streamKey = legacyKey;
      legacy.streamKeyExplicit = true;
    }
    if (typeof stored.returnAudio === "boolean") legacy.returnAudio = stored.returnAudio;
    await chrome.storage.local.set({
      ...result,
      [LEGACY_TAB_SETTINGS_KEY]: legacy,
      [SETTINGS_VERSION_KEY]: SETTINGS_VERSION
    });
  }
  return result;
}

/** localのport/tokenだけを差分保存し、タブ別の旧設定を上書きしない。 */
export async function saveSettings(value: CommonSettings): Promise<void> {
  await chrome.storage.local.set({
    port: value.port,
    token: value.token,
    [SETTINGS_VERSION_KEY]: SETTINGS_VERSION
  });
}

export async function loadLegacyTabSettings(): Promise<Partial<TabSettings>> {
  const value = await chrome.storage.local.get(LEGACY_TAB_SETTINGS_KEY);
  return (value[LEGACY_TAB_SETTINGS_KEY] ?? {}) as Partial<TabSettings>;
}

export type CaptureState = "idle" | "starting" | "connecting" | "authenticated" | "streaming" | "stopping" | "stopped" | "failed";

export type MediaAction = "play" | "pause";
export type MediaState = "playing" | "paused" | "ended" | "unsupported" | "unknown";

export type MediaControlResult = {
  state: MediaState;
  error?: string;
  positionSeconds?: number;
  durationSeconds?: number;
};

export type CaptureStatus = {
  state: CaptureState;
  tabId?: number;
  streamKey?: string;
  returnAudio?: boolean;
  fileRecording?: boolean;
  fileName?: string;
  fileError?: string;
  title?: string;
  url?: string;
  message?: string;
  receiverState?: "running" | "stopped";
  /** ページ内 media の実状態。PCM capture の状態とは独立する。 */
  mediaState?: MediaState;
  mediaError?: string;
};

export type CaptureStatuses = {
  statuses: CaptureStatus[];
};

export function isBusy(status: CaptureStatus): boolean {
  return ["starting", "connecting", "authenticated", "streaming", "stopping"].includes(status.state);
}

export function pcmFrameBytes(samples: Float32Array): ArrayBuffer {
  return samples.slice().buffer;
}
