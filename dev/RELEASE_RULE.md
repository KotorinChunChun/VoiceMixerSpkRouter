# 公開・配備

版はCargo.tomlとsrc/spk_router/config.rsで管理し、patchだけ増やす。文書は作業中dev直下、完成後historyへ移す。

通常回帰・変更箇所のGUI試験・Debug/Releaseビルド後に配備する。既存常駐は通常終了し、EXEと設定、未保存編集を退避して復元する。個人設定と既存Startupを保持し、移動時だけ起動先を追従させる。新規自動起動の登録は-Startupを明示した場合のみ。

```powershell
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
pwsh -NoProfile -File .\dev\scripts\Install-SpkRouter.ps1
```

ソースの公開前に追跡対象を検査する。release、target、tmp、dev/private、dev/temp、個人設定、録音、モデルは公開しない。元のVoiceMixerリポジトリの履歴は取り込まない。今回はユーザーの明示指示により専用Publicリポジトリを作成して公開する。署名・インストーラーは別の明示依頼として扱う。GitHub ReleasesへのZIP公開は2026-10-19の指示により実施する。

## 配布ZIP

```powershell
cd C:/develop/VoiceMixerSpkRouter
pwsh -NoProfile -File ./dev/scripts/Build-Portable.ps1
pwsh -NoProfile -File ./dev/scripts/tests/Test-PortablePackage.ps1 -Zip ./release/Publish/VoiceMixerSpkRouter-v1.0.9-windows-x64.zip -SmokeTest
```

配布専用の`target/portable`へx64 MSVC・CRT静的リンクでビルドする。ブラウザ拡張、利用手順、版とソースコミット、内部SHA-256一覧を同梱する。個人設定がある通常の配備先を梱包しない。Buildの`-SkipBuild`は既に検証した成果物の再梱包に限る。ソースコミットを確定してから最終ZIPを作る。

検証スクリプトはVisual Studioのdumpbinで外部CRT依存を拒否し、AMD64・内部と外部ハッシュ・拡張参照を確認する。`-SmokeTest`は利用中の常駐を通常終了・必要な未保存編集を退避してから実施し、別の空の日本語・空白入りフォルダーで初回起動、停止・2出力OFF、設定生成、再起動、正常終了を確認する。利用中の常駐と編集状態は検証後に戻す。これは新規Windows環境での音声実測やブラウザへの拡張登録を代替しない。

ZIPと`.zip.sha256`を同じ版のGitHub Releaseに添付する。タグのコミットとZIP内VERSION.txtを揃え、公開後にGitHubから再ダウンロードしてハッシュを照合する。Release配布物だけを添付し、設定・バックアップ・秘密情報・ビルドキャッシュは添付しない。
