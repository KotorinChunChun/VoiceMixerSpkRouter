# 開発者ガイド

Rustの単独ワークスペースです。src/main.rsとsrc/spk_routerがアプリ、crates/audio-engineが取り込み時点の音声エンジンです。VoiceMixer本体へのパス依存・Git依存はありません。取り込み元の識別情報はdev/import-origin.jsonに記録し、元のGit履歴は公開していません。

ブラウザ拡張はbrowser-extensionに置き、package-lock.jsonで依存を固定します。

```powershell
cargo test --locked --bin voicemixer-spk -- --test-threads=1
cargo build --locked --bin voicemixer-spk
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
```

GUI・実フック試験は他のSpkRouterを通常終了して実施します。実フック試験だけ専用キーの合成入力を許可し、製品では注入入力を無視します。

```powershell
cargo test --locked --bin voicemixer-spk -- --ignored --test-threads=1
pwsh -NoProfile -File .\dev\scripts\tests\Test-SpkRouterUi.ps1 -Executable .\target\debug\voicemixer-spk.exe
pwsh -NoProfile -File .\dev\scripts\tests\Test-SpkRouterLifecycle.ps1 -Executable .\target\debug\voicemixer-spk.exe
```

ブラウザの詳細操作は[拡張ガイド](edge-extension.md)、アプリの仕様は[SpkRouter](spk-router.md)を参照してください。ブラウザ拡張のデフォルト接続先を本体向けから変更せず、SpkRouterの18766と認証トークンを案内に従って設定します。
