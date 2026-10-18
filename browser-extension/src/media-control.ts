import type { MediaAction, MediaControlResult, MediaState } from "./protocol";

/**
 * `chrome.scripting.executeScript` で対象タブへ送る関数。
 *
 * この関数はシリアライズされるため、モジュール側の値や補助関数を参照してはならない。
 * 同一オリジン iframe と open shadow root は文書から辿り、別オリジン iframe は
 * SecurityError を握りつぶして対象外とする。
 */
export async function inspectOrControlMedia(action?: MediaAction, positionSeconds?: number): Promise<MediaControlResult> {
  const elements: HTMLMediaElement[] = [];
  const seenRoots = new Set<Document | ShadowRoot>();
  const seenElements = new Set<HTMLMediaElement>();

  const visit = (root: Document | ShadowRoot): void => {
    if (seenRoots.has(root)) return;
    seenRoots.add(root);
    for (const media of root.querySelectorAll<HTMLMediaElement>("audio,video")) {
      if (!seenElements.has(media)) {
        seenElements.add(media);
        elements.push(media);
      }
    }
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) visit(element.shadowRoot);
      if (element instanceof HTMLIFrameElement) {
        try {
          if (element.contentDocument) visit(element.contentDocument);
        } catch {
          // 別オリジン iframe は activeTab でも読み取れない。権限を広げない。
        }
      }
    }
  };

  visit(document);
  if (elements.length === 0) return { state: "unsupported", error: "このページには操作可能な audio/video 要素がありません" };

  if (action === "pause") {
    for (const media of elements) media.pause();
    if (elements.some(media => !media.paused && !media.ended)) {
      return { state: "playing", error: "ページが一時停止を受け付けませんでした" };
    }
  } else if (action === "play") {
    const failures = (await Promise.all(elements.map(async media => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        if (Number.isFinite(positionSeconds) && positionSeconds !== undefined && positionSeconds >= 0
          && Number.isFinite(media.duration)) {
          media.currentTime = Math.min(positionSeconds, media.duration);
        }
        await Promise.race([
          media.play(),
          new Promise<void>((_resolve, reject) => {
            timer = setTimeout(() => reject(new Error("再生開始がタイムアウトしました")), 1_500);
          })
        ]);
        return undefined;
      } catch (error) {
        // timeout後に遅れて解決した play() が再生を開始しないよう明示的に止める。
        media.pause();
        return error instanceof Error ? error.message : String(error);
      } finally {
        if (timer !== undefined) clearTimeout(timer);
      }
    }))).filter((error): error is string => error !== undefined);
    if (failures.length !== 0) {
      const playing = elements.some(media => !media.paused && !media.ended);
      return {
        state: playing ? "playing" : "paused",
        error: `再生を開始できませんでした: ${failures.join(" / ")}`
      };
    }
  }

  const state = elements.some(media => !media.paused && !media.ended)
    ? "playing"
    : elements.some(media => media.ended) && elements.every(media => media.paused || media.ended)
      ? "ended"
      : "paused";
  const current = elements.find(media => !media.ended) ?? elements[0];
  const durationSeconds = Number.isFinite(current.duration) ? current.duration : undefined;
  return {
    state,
    positionSeconds: Number.isFinite(current.currentTime) ? current.currentTime : undefined,
    durationSeconds
  };
}

export async function readTabMedia(tabId: number): Promise<MediaControlResult> {
  const results = await chrome.scripting.executeScript({
    target: { tabId },
    func: inspectOrControlMedia
  });
  return results[0]?.result ?? { state: "unknown", error: "メディア状態を読み取れませんでした" };
}

export async function controlTabMedia(tabId: number, action: MediaAction, positionSeconds?: number): Promise<MediaControlResult> {
  const results = await chrome.scripting.executeScript({
    target: { tabId },
    func: inspectOrControlMedia,
    args: [action, positionSeconds]
  });
  return results[0]?.result ?? { state: "unknown", error: "メディア制御結果を読み取れませんでした" };
}

export function isMediaState(value: unknown): value is MediaState {
  return value === "playing" || value === "paused" || value === "ended" || value === "unsupported" || value === "unknown";
}
