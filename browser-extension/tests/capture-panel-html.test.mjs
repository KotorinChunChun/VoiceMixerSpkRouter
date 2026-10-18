import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const sourceRoot = new URL("../src/", import.meta.url);

async function html(name) {
  return readFile(new URL(name, sourceRoot), "utf8");
}

test("capture panel pages share the stylesheet while preserving the UI contract", async () => {
  const [sidepanel, popup, stylesheet] = await Promise.all([
    html("sidepanel.html"),
    html("popup.html"),
    html("capture-panel.css")
  ]);

  for (const page of [sidepanel, popup]) {
    assert.match(page, /<link rel="stylesheet" href="\.\/capture-panel\.css">/);
    assert.doesNotMatch(page, /<style>/);
    assert.match(page, /<div id="capture-target" hidden>/);
    assert.match(page, /<strong data-target-title><\/strong>/);
    assert.match(page, /<button id="start" type="button">キャプチャ開始<\/button>/);
    assert.match(page, /<output aria-live="polite"><\/output>/);
    assert.match(page, /<p id="empty">キャプチャ対象のタブはありません<\/p>/);
    assert.match(page, /<div id="tab-cards"><\/div>/);
    assert.match(page, /<button id="open-options" type="button">/);
  }

  assert.match(sidepanel, /<script type="module" src="\.\/sidepanel\.ts"><\/script>/);
  assert.match(popup, /<body class="capture-popup">/);
  assert.match(popup, /<script type="module" src="\.\/popup\.ts"><\/script>/);
  assert.match(stylesheet, /\[hidden\]\s*\{\s*display: none !important;/);
  assert.match(stylesheet, /body\.capture-popup\s*\{\s*min-width: 300px;/);
  assert.doesNotMatch(stylesheet, /\nbody\s*\{[^}]*min-width/);
});
