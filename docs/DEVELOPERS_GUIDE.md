# 開発者ガイド

VoiceMixerSpkRouterは独立したRustワークスペースです。アプリのコードはsrc/main.rsとsrc/spk_routerにあり、取り込み時点の音声エンジンはcrates/audio-engineにあります。VoiceMixer本体へのパス依存・Git依存はありません。取り込み元の識別情報はdev/import-origin.jsonに記録し、元のGit履歴は公開していません。

ブラウザ拡張はbrowser-extensionに置き、package-lock.jsonで依存を固定します。

```powershell
cargo test --locked --bin voicemixer-spk -- --test-threads=1
cargo build --locked --bin voicemixer-spk
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
```

GUI試験と実フック試験は、他のSpkRouterを通常終了してから行います。専用キーの合成入力を許可するのは実フック試験だけです。製品では注入入力を無視します。

```powershell
cargo test --locked --bin voicemixer-spk -- --ignored --test-threads=1
pwsh -NoProfile -File .\dev\scripts\tests\Test-SpkRouterUi.ps1 -Executable .\target\debug\voicemixer-spk.exe
pwsh -NoProfile -File .\dev\scripts\tests\Test-SpkRouterLifecycle.ps1 -Executable .\target\debug\voicemixer-spk.exe
```

ブラウザの詳細操作は[拡張ガイド](edge-extension.md)、アプリの仕様は[SpkRouter](spk-router.md)を参照してください。ブラウザ拡張のデフォルト接続先を本体向けから変更せず、SpkRouterの18766と認証トークンを案内に従って設定します。

## インストーラの発行

pwsh -NoProfile -File ./dev/scripts/Build-Installer.ps1で、利用者単位のInnoインストーラを作成します。CRTを静的リンクした配布EXE、拡張、自己完結型の.NET補助プログラムを同梱します。追加で必要なのは.NET 10 SDKと、Prepare-Inno.ps1で検証・準備する固定版コンパイラです。既に検証したアプリ成果物を再梱包するだけなら-SkipBuildを指定できます。

製品IDと本体への受け渡しは[連携ガイド](kotorintoys-integration.md)、公開条件は[配布規約](../dev/RELEASE_RULE.md)を参照してください。予約整理の模擬試験はdotnet run --project maintenance/tests/Maintenance.Tests.csproj --artifacts-path target/maintenance-tests -- tmp/reservation-testsです。実setup試験はdev/scripts/tests/Test-Installer.ps1を使い、既存の正式導入があれば中止します。

## ビルドと起動

Windows、RustのMSVCツールチェーン、Visual Studio C++ビルドツール、Node.jsとnpmが必要です。

```powershell
git clone https://github.com/KotorinChunChun/VoiceMixerSpkRouter.git
cd VoiceMixerSpkRouter
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
pwsh -NoProfile -File .\dev\scripts\Install-SpkRouter.ps1
pwsh -NoProfile -File .\dev\scripts\Start-SpkRouter.ps1 -Mode Settings
```

設定はrelease/SpkRouter/spk-settings.jsonへ保存します。認証トークンを含むため共有しないでください。初回は取得停止・両出力OFFで起動します。


## README撮影用の隔離画面

通常版が起動中でも、次の別名EXEで設定画面を確認できます。出力先は存在しない新しいフォルダーを指定してください。Release構成には撮影入口を含めません。

```powershell
cargo test --offline --bin voicemixer-spk
cargo build --offline --example manual-preview
.\target\debug\examples\manual-preview.exe --manual-preview C:\path\to\new-screenshot-output
```

実際のSettingsWindowを使い、適用時は指定先のsettings.jsonだけに保存します。音声エンジン・管理IPC・グローバルキー・OS予約は作りません。デバイス一覧は読み取りのみです。認証トークンはダミー固定です。実機の音声・キー・拡張接続は別途検証してください。
