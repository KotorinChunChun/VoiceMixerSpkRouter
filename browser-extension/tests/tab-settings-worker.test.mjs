import test from 'node:test';
import assert from 'node:assert/strict';

let workerSequence = 0;
async function workerFixture({ session, remote = [], onOffscreenMessage = () => ({ ok: true }), onExecuteScript = async () => [{ result: { state: 'paused' } }] }) {
  let receive, removed;
  const calls = [];
  globalThis.chrome = {
    sidePanel: { setPanelBehavior: async () => {} },
    action: { onClicked: { addListener() {} } },
    storage: {
      session: { get: async () => session, set: async value => Object.assign(session, value) },
      local: { get: async () => ({ settingsVersion: 3, port: 38471, token: 'token' }), set: async () => {} }
    },
    tabs: {
      get: async id => ({ id, title: `タブ${id}`, url: `https://tab-${id}.test` }),
      onUpdated: { addListener() {} }, onRemoved: { addListener: callback => { removed = callback; } }
    },
    scripting: { executeScript: async details => onExecuteScript(details) },
    tabCapture: { getMediaStreamId: (_options, callback) => callback('stream-id'), getCapturedTabs: callback => callback([]) },
    offscreen: { hasDocument: async () => true },
    downloads: { download: async () => 1 },
    runtime: {
      id: 'browser-test-id',
      onMessage: { addListener: callback => { receive = callback; } },
      sendMessage: async message => {
        calls.push(message);
        if (message.type === 'offscreen_get_statuses') return { statuses: remote };
        return onOffscreenMessage(message);
      }
    }
  };
  await import(`../dist/service_worker.js?tab-settings-case-${workerSequence++}`);
  return {
    calls,
    removed,
    send: message => new Promise(resolve => receive({ target: 'service-worker', ...message }, {}, resolve))
  };
}

test('タブ別設定をsessionから復元し、停止中のキーと音声戻しを更新できる', async () => {
  const session = {
    captureTargetTabIds: [9],
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '9': { streamKey: 'saved-key', returnAudio: false, title: '保存済み', url: 'https://saved.test', streamKeyExplicit: true }
      , '10': { streamKey: 'taken-key', returnAudio: false, title: '別タブ', url: 'https://other.test', streamKeyExplicit: true }
    }
  };
  let receive;
  const offscreenCalls = [];
  globalThis.chrome = {
    sidePanel: { setPanelBehavior: async () => {} },
    action: { onClicked: { addListener() {} } },
    storage: {
      session: { get: async () => session, set: async value => Object.assign(session, value) },
      local: { get: async () => ({ settingsVersion: 3, port: 38471, token: 'token' }), set: async () => {} }
    },
    tabs: {
      get: async id => ({ id, title: '現在の題名', url: 'https://current.test' }),
      onUpdated: { addListener() {} }, onRemoved: { addListener() {} }
    },
    tabCapture: { getMediaStreamId: (_options, callback) => callback('stream-id'), getCapturedTabs: callback => callback([]) },
    offscreen: { hasDocument: async () => true },
    runtime: {
      id: 'browser-test-id',
      onMessage: { addListener: callback => { receive = callback; } },
      sendMessage: async message => {
        offscreenCalls.push(message);
        if (message.type === 'offscreen_get_statuses') return { statuses: [] };
        return { ok: true };
      }
    }
  };
  await import('../dist/service_worker.js?tab-settings-first');
  const send = message => new Promise(resolve => receive({ target: 'service-worker', ...message }, {}, resolve));

  const initial = await send({ type: 'get_capture_status' });
  assert.deepEqual(initial.statuses[0], {
    state: 'idle', tabId: 9, streamKey: 'saved-key', returnAudio: false,
    title: '保存済み', url: 'https://saved.test'
  });
  const duplicate = await send({ type: 'update_tab_settings', tabId: 9, streamKey: 'taken-key' });
  assert.equal(duplicate.ok, false);
  assert.match(duplicate.error, /別のタブ/);
  const updated = await send({ type: 'update_tab_settings', tabId: 9, streamKey: 'changed-key', returnAudio: true });
  assert.equal(updated.ok, true);
  assert.equal(updated.status.streamKey, 'changed-key');
  assert.equal(updated.status.returnAudio, true);
  assert.ok(offscreenCalls.some(call => call.type === 'offscreen_forget' && call.tabId === 9));
  assert.ok(offscreenCalls.some(call => call.type === 'offscreen_set_return_audio' && call.returnAudio === true));

  // 新しいworker module instanceでもsessionの値だけから一覧を復元する。
  await import('../dist/service_worker.js?tab-settings-restart');
  const restored = await new Promise(resolve => receive({ target: 'service-worker', type: 'get_capture_status' }, {}, resolve));
  assert.equal(restored.statuses[0].streamKey, 'changed-key');
  assert.equal(restored.statuses[0].returnAudio, true);
});

test('streaming中でも音声戻しだけは対象tabへ即時反映できる', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '21': { streamKey: 'streaming-key', returnAudio: false, title: '保存済み', url: 'https://saved.test', streamKeyExplicit: true }
    }
  };
  const remote = [{ state: 'streaming', tabId: 21, streamKey: 'streaming-key' }];
  const worker = await workerFixture({ session, remote });
  const result = await worker.send({ type: 'update_tab_settings', tabId: 21, returnAudio: true });
  assert.equal(result.ok, true);
  assert.equal(result.status.state, 'streaming');
  assert.equal(result.status.returnAudio, true);
  assert.equal(session.voiceMixerTabSettingsV047['21'].returnAudio, true);
  assert.ok(worker.calls.some(call => call.type === 'offscreen_set_return_audio' && call.tabId === 21 && call.returnAudio === true));
});

test('音声戻しのoffscreen更新失敗では保存値もstatusも変更しない', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '22': { streamKey: 'unchanged-key', returnAudio: false, title: '保存済み', url: 'https://saved.test', streamKeyExplicit: true }
    }
  };
  const remote = [{ state: 'streaming', tabId: 22, streamKey: 'unchanged-key' }];
  const worker = await workerFixture({
    session,
    remote,
    onOffscreenMessage: message => message.type === 'offscreen_set_return_audio'
      ? { ok: false, error: 'Gain変更失敗' } : { ok: true }
  });
  const result = await worker.send({ type: 'update_tab_settings', tabId: 22, returnAudio: true });
  assert.equal(result.ok, false);
  assert.match(result.error, /Gain変更失敗/);
  assert.equal(session.voiceMixerTabSettingsV047['22'].returnAudio, false);
  const status = await worker.send({ type: 'get_capture_status' });
  assert.equal(status.statuses.find(item => item.tabId === 22).state, 'streaming');
  assert.equal(status.statuses.find(item => item.tabId === 22).returnAudio, false);
});

test('worker再起動直後に復元したrunning tabのキー変更を拒否する', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '23': { streamKey: 'running-key', returnAudio: false, title: '保存済み', url: 'https://saved.test', streamKeyExplicit: true }
    }
  };
  const worker = await workerFixture({
    session,
    remote: [{ state: 'streaming', tabId: 23, streamKey: 'running-key' }]
  });
  const result = await worker.send({ type: 'update_tab_settings', tabId: 23, streamKey: 'new-key' });
  assert.equal(result.ok, false);
  assert.match(result.error, /停止してから/);
  assert.equal(session.voiceMixerTabSettingsV047['23'].streamKey, 'running-key');
  assert.ok(!worker.calls.some(call => call.type === 'offscreen_forget'));
});

test('tabClosedは対象のsession設定だけを削除する', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '24': { streamKey: 'closed-key', returnAudio: false, title: '閉じる', url: 'https://closed.test', streamKeyExplicit: true },
      '25': { streamKey: 'kept-key', returnAudio: true, title: '残す', url: 'https://kept.test', streamKeyExplicit: true }
    }
  };
  const worker = await workerFixture({ session });
  worker.removed(24);
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(session.voiceMixerTabSettingsV047['24'], undefined);
  assert.equal(session.voiceMixerTabSettingsV047['25'].streamKey, 'kept-key');
  assert.ok(worker.calls.some(call => call.type === 'tab_closed' && call.tabId === 24));
});

test('登録済みtabのメディア操作は実状態を保持し、PCM状態を変更しない', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '26': { streamKey: 'media-key', returnAudio: false, title: '音楽', url: 'https://media.test', streamKeyExplicit: true }
    }
  };
  const scripts = [];
  const worker = await workerFixture({
    session,
    remote: [{ state: 'streaming', tabId: 26, streamKey: 'media-key' }],
    onExecuteScript: async details => {
      scripts.push(details);
      return [{ result: { state: 'paused', error: '自動再生が拒否されました' } }];
    }
  });
  const result = await worker.send({ type: 'control_tab_media', tabId: 26, action: 'play' });
  assert.equal(result.ok, false);
  assert.equal(result.state, 'paused');
  assert.match(result.error, /拒否/);
  assert.equal(scripts.length, 1);
  assert.equal(scripts[0].target.tabId, 26);
  assert.equal(scripts[0].args[0], 'play');
  const status = await worker.send({ type: 'get_capture_status' });
  const media = status.statuses.find(item => item.tabId === 26);
  assert.equal(media.state, 'streaming');
  assert.equal(media.mediaState, 'paused');
  assert.match(media.mediaError, /拒否/);
});

test('メディア操作の再生位置と録音同期をoffscreenへ伝える', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '29': { streamKey: 'sync-key', returnAudio: false, title: '同期', url: 'https://sync.test', streamKeyExplicit: true }
    }
  };
  const scripts = [];
  const worker = await workerFixture({
    session,
    remote: [{ state: 'stopped', tabId: 29, streamKey: 'sync-key' }],
    onExecuteScript: async details => { scripts.push(details); return [{ result: { state: 'playing' } }]; }
  });
  const result = await worker.send({ type: 'control_tab_media', tabId: 29, action: 'play', positionSeconds: 600, syncCapture: true });
  assert.equal(result.ok, true);
  assert.equal(scripts[0].args[0], 'play');
  assert.equal(scripts[0].args[1], 600);
  assert.ok(worker.calls.some(call => call.type === 'offscreen_resume' && call.tabId === 29));
  assert.ok(worker.calls.some(call => call.type === 'offscreen_arm_media_end' && call.tabId === 29));
});

test('PCM送信中のファイル保存は再キャプチャせずoffscreenへ依頼する', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '27': { streamKey: 'wav-key', returnAudio: false, title: '保存対象', url: 'https://saved.test', streamKeyExplicit: true }
    }
  };
  const remote = [{ state: 'streaming', tabId: 27, streamKey: 'wav-key' }];
  const worker = await workerFixture({ session, remote });
  const result = await worker.send({ type: 'save_wav', tabId: 27 });
  assert.equal(result.ok, true);
  assert.ok(worker.calls.some(call => call.type === 'offscreen_start_file' && call.tabId === undefined && call.streamKey === 'wav-key'));
  assert.ok(!worker.calls.some(call => call.type === 'offscreen_start_file' && call.streamId === 'stream-id'));
});

test('WebMデータのダウンロードはService Workerから実行する', async () => {
  const worker = await workerFixture({ session: {} });
  const result = await worker.send({ type: 'download_media', url: 'blob:test', filename: 'test.webm' });
  assert.equal(result.ok, true);
  assert.equal(result.downloadId, 1);
});

test('WAV保存中のファイル保存操作はoffscreenへ停止を依頼する', async () => {
  const session = {
    voiceMixerBrowserSessionV047: 'browser',
    voiceMixerTabSettingsV047: {
      '28': { streamKey: 'wav-key-2', returnAudio: false, title: '保存中', url: 'https://saved.test', streamKeyExplicit: true }
    }
  };
  const worker = await workerFixture({ session, remote: [{ state: 'stopped', tabId: 28, streamKey: 'wav-key-2', fileRecording: true }] });
  const result = await worker.send({ type: 'save_wav', tabId: 28 });
  assert.equal(result.ok, true);
  assert.ok(worker.calls.some(call => call.type === 'offscreen_stop_file' && call.tabId === 28));
});
