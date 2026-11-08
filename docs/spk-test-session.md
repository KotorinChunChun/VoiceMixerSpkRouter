# 隔離テストCLI（開発ビルド）

`voicemixer-spk.exe --test-session` を単独で指定します。通常の設定・音声デバイス・常駐IPC・ブラウザ待受・グローバルキーには接続しません。通常の常駐や別の隔離セッションを動かしたまま使えます。配布用Releaseビルドでは使用できません。

動作中の通常版を置き換えないよう、別の出力先にビルドします。以下はPowerShell 7での例です。

```powershell
cd C:\develop\KotorinToys\tools\VoiceMixerSpkRouter
$env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
$env:CARGO_BUILD_BUILD_DIR = Join-Path (Get-Location) 'target'
$build = 'dev/history/v1.0.16/logs/cli-example/build'
cargo build --locked --bin voicemixer-spk --target-dir $build
@(
    '{"schemaVersion":1,"requestId":"validate","command":"validate-route","mode":"device","patch":{"outputs":[{"deviceId":"speaker-alias","enabled":true},null]}}'
    '{"schemaVersion":1,"requestId":"apply","command":"apply","expectedRevision":0,"patch":{"mode":"device","outputs":[{"deviceId":"headphones"},null]}}'
    '{"schemaVersion":1,"requestId":"enable","command":"set-output","expectedRevision":1,"output":1,"enabled":true}'
    '{"schemaVersion":1,"requestId":"status","command":"status"}'
    '{"schemaVersion":1,"requestId":"bye","command":"shutdown"}'
) | & "$build/debug/voicemixer-spk.exe" --test-session
```

標準入力・標準出力はUTF-8の1行1JSONです。起動時に `event: "ready"`、要求ごとに `requestId` と `command` が対応する応答を返します。要求IDは英数字・ハイフン・アンダースコアの1〜64字です。要求は1行64 KiBまで。正常終了・EOFでは試験用一時領域を回収します。OSによる強制終了では残ることがあります。

| 操作 | 指定 | 結果 |
|---|---|---|
| `status` | 追加指定なし | `ok`。当該セッションの確定状態 |
| `validate-route` | `mode` 必須、`patch` は任意 | `valid` または `rejected`。候補・解決ID・警告を返す。保存しない |
| `apply` | `expectedRevision`、`patch` | `applied`、`unchanged`、`rejected`、`failed` |
| `set-output` | `expectedRevision`、`output`（1/2）、`enabled` | `apply` と同じ。指定した出力だけを変更 |
| `shutdown` | 追加指定なし | `ok` を返して対象セッションだけ終了 |

`patch` は `mode`、`inputEnabled`（デバイス・アプリ・ブラウザの真偽値3つ）、`inputDeviceId`、`outputs` を指定できます。出力は2要素で、各要素は `null`（変更なし）または `deviceId`・`enabled` の変更分です。経路検証時のモードはトップレベルに指定します。ファイルパス・認証情報は受け付けません。

試験用デバイスIDは `default`、`communications`、`speaker`、`speaker-alias`、`headphones`、`cable`、`disconnected`、`unselected`。初期状態は停止、出力1が `default`、出力2は未選択、どちらもOFF。 `default`・`speaker`・`speaker-alias` は同じ試験デバイス、`communications`・`headphones` も同一です。

応答の `state.outputsEnabled` は設定上のON/OFF、`state.effectiveOutputs` は重複抑止後に採用された試験経路です。 `state.simulated` は常に `true` です。物理音声の出力成功を示すものではありません。 `issues` は `code`・`field`・`severity` を返します。 `revision` は設定が確定したときだけ進みます。 `apply`・`set-output` の `fault` に `audio`／`save`／`rollback` を指定すると失敗を注入できます。復旧も失敗したときは `rollback_failed` と `effectiveOutputs: null` を返します。

個別の拒否や注入失敗はJSONで判定し、プロセスは継続します。起動失敗や入出力障害は非0で終了します。並行試験は [Test-SpkRouterTestSession.ps1](../dev/scripts/tests/Test-SpkRouterTestSession.ps1) を参照してください。
