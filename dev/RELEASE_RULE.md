# 公開・配備

版はCargo.tomlとsrc/spk_router/config.rsで管理し、patchだけ増やす。文書は作業中dev直下、完成後historyへ移す。

通常回帰・変更箇所のGUI試験・Debug/Releaseビルド後に配備する。既存常駐は通常終了し、EXEと設定、未保存編集を退避して復元する。個人設定と既存Startupを保持し、移動時だけ起動先を追従させる。新規自動起動の登録は-Startupを明示した場合のみ。

```powershell
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
pwsh -NoProfile -File .\dev\scripts\Install-SpkRouter.ps1
```

ソースの公開前に追跡対象を検査する。release、target、tmp、dev/private、dev/temp、個人設定、録音、モデルは公開しない。元のVoiceMixerリポジトリの履歴は取り込まない。今回はユーザーの明示指示により専用Publicリポジトリを作成して公開する。署名・インストーラー・GitHub Releasesの配布物作成は別の依頼として扱う。
