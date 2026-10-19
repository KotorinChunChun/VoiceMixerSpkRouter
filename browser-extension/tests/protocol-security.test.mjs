import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import ts from 'typescript';

const source = readFileSync(new URL('../src/protocol.ts', import.meta.url), 'utf8');
const output = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2022 } }).outputText;
const { DEFAULT_SETTINGS, loadSettings } = await import('data:text/javascript;base64,' + Buffer.from(output).toString('base64'));

function storage(value) {
  globalThis.chrome = { storage: { local: {
    get: async () => value,
    set: async update => Object.assign(value, update)
  } } };
}

test('新規設定に固定の試験トークンを補完しない', async () => {
  storage({});
  assert.equal(DEFAULT_SETTINGS.token, '');
  assert.equal((await loadSettings()).token, '');
});

test('旧版の空欄も未設定のままにする', async () => {
  storage({ settingsVersion: 1, token: '' });
  assert.equal((await loadSettings()).token, '');
});

test('保存済みの利用者トークンと他の設定を保持する', async () => {
  const saved = { settingsVersion: 3, token: 'synthetic-saved-token', port: 18766, returnAudio: false };
  storage(saved);
  assert.deepEqual(await loadSettings(), { token: 'synthetic-saved-token', port: 18766 });
  assert.equal(saved.returnAudio, false);
});
