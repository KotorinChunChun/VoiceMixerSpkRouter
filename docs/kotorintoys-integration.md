# 導入・更新・削除とKotorinToys連携

Windows 11 x64向けの利用者単位Inno Setupです。GitHub ReleasesのVoiceMixerSpkRouter-1.0.10-setup.exeを実行します。既定の導入先は%LOCALAPPDATA%/Programs/VoiceMixerSpkRouter。管理者権限、Rust、Node.js、.NETの追加インストールは不要です。ブラウザ拡張は同梱しますが、登録とキャプチャ許可は同梱HTMLで案内します。

設定・認証トークンは導入先のspk-settings.jsonへ初回生成します。配布物に個人設定は含めません。同じ導入先への更新・アンインストールでは設定を保持し、再導入時に再利用します。ポータブル版と開発版の設定は自動移動しません。旧ポータブル版・開発版が動作中なら終了してから導入版を起動してください。セッション内の二重常駐は行いません。

設定画面が開いている間や適用中は管理側からの終了を拒否します。適用するか編集を破棄して設定画面を閉じ、更新・削除をやり直してください。プロセスの強制終了は行いません。既存の更新保守用CLI --stopは従来互換です。

## 製品情報

|項目|値|
|---|---|
|id|voice-mixer-spk-router|
|name|VoiceMixerSpkRouter|
|appId|KotorinToys.VoiceMixerSpkRouter|
|導入EXE|VoiceMixerSpkRouter.exe|
|登録|HKCU/Software/Microsoft/Windows/CurrentVersion/Uninstall/KotorinToys.VoiceMixerSpkRouter_is1|
|導入定義|ルートのtool.json|
|setup|VoiceMixerSpkRouter-1.0.10-setup.exe|
|公開台帳|VoiceMixerSpkRouter-packages.json|
|ハッシュ|VoiceMixerSpkRouter-SHA256SUMS.txt|

管理契約2のパイプはKotorinToys.<現在ユーザーSID>.voice-mixer-spk-router。現在ユーザーだけのACL、リモート接続拒否、同じ対話セッション、操作対象PID、要求期限を検証します。status/show/stopに対応し、showは既存設定画面を表示、stopは終了受理を返した後で正常終了します。旧文字列形式はstatusだけに対応し、showとstopはexpectedPidを含むJSON要求が必要です。管理操作のstopは音声取得停止モードとは異なります。

--startup --launch-origin host_manual|direct_task|via_kotorintoysで設定画面を開かず起動し、showを待ちます。単独起動の経路はstandaloneです。単独の初回起動は従来どおり設定を開きます。

## 本体側への受け渡し

本リリースはSpkRouter側だけを対応させています。KotorinToys本体の修正・製品登録・一覧公開は行いません。本体側で固定の製品識別と配布対象に追加し、tool.jsonの情報とオンライン一覧を登録した版が必要です。未対応の既存本体で直ちに導入できることは意味しません。

公開する製品台帳はschemaVersion=1、distributionStatus=published、packagesはこの製品1件。版・AppId・setup名・容量・SHA-256をGitHub assetと照合します。未署名の開発版として公開します。

更新・削除補助は.NETランタイム同梱。本人のKotorinToys所有予約だけを既存の所有マーカー・XML・権限の指紋で照合し、対象製品の予約を整理します。他製品の設定・外部予約・SpkRouterの設定は削除しません。削除取消・失敗時は所有記録から復旧します。
