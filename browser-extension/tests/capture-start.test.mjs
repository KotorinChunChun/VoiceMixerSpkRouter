import test from 'node:test';
import assert from 'node:assert/strict';

test('タブごとにIDを発行し、停止後の同じタブだけを再開できる', async () => {
  let click, receive, issued = 0, consumed = [], panelOpened = 0;
  let remote = [];
  let releaseStop;
  let stopBarrier;
  const session = { voiceMixerBrowserSessionV047: 'browser' };
  globalThis.chrome = {
    sidePanel: {
      setPanelBehavior: async value => assert.equal(value.openPanelOnActionClick, false),
      open: async () => { panelOpened++; }
    },
    action: { onClicked: { addListener: f => { click = f; } } },
    storage: {
      session: { set: async v => Object.assign(session, v), get: async () => session },
      local: { get: async () => ({ settingsVersion: 2, token: 'test', streamKey: 'test', port: 38471 }), set: async () => {} }
    },
    tabs: { get: async id => ({ id, windowId: 1 }), onUpdated: { addListener() {} }, onRemoved: { addListener() {} } },
    tabCapture: { getMediaStreamId: (options, callback) => callback(`id-${++issued}`), getCapturedTabs: callback => callback([]) },
    offscreen: { hasDocument: async () => true },
    runtime: {
      onMessage: { addListener: f => { receive = f; } },
      sendMessage: async message => {
        if (message.type === 'offscreen_get_statuses') return { statuses: remote };
        if (message.type === 'offscreen_start') { consumed.push(message.streamId); remote = [...remote.filter(item => item.tabId !== message.tab.id), { state: 'streaming', tabId: message.tab.id, streamKey: message.streamKey, receiverState: 'running' }]; }
        if (message.type === 'offscreen_stop') {
          await stopBarrier;
          remote = remote.map(item => item.tabId === message.tabId ? { ...item, state: 'stopped' } : item);
        }
        return { ok: true };
      }
    }
  };
  await import('../dist/service_worker.js');
  click({ id: 42, windowId: 1 });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(panelOpened, 1);
  assert.equal(issued, 0);
  const send = (type, tabId = 42) => new Promise(resolve => receive({ target: 'service-worker', type, tabId }, {}, resolve));
  assert.equal((await send('capture_tab')).ok, true);
  assert.deepEqual(consumed, ['id-1']);
  click({ id: 43, windowId: 1 });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal((await send('capture_tab', 43)).ok, true);
  assert.deepEqual(consumed, ['id-1', 'id-2']);
  assert.deepEqual(remote.map(item => item.streamKey), ['tab-browser-42', 'tab-browser-43']);
  assert.equal((await send('capture_tab')).ok, false);
  assert.equal(issued, 2);
  stopBarrier = new Promise(resolve => { releaseStop = resolve; });
  const stopping = send('stop_capture');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal((await send('capture_tab')).ok, false);
  assert.equal(issued, 2);
  releaseStop();
  assert.equal((await stopping).ok, true);
  assert.equal((await send('get_capture_status')).statuses.find(item => item.tabId === 42).receiverState, 'running');
  assert.deepEqual(remote.map(item => item.state), ['stopped', 'streaming']);
  assert.equal((await send('capture_tab')).ok, true);
  assert.deepEqual(consumed, ['id-1', 'id-2', 'id-3']);
});
