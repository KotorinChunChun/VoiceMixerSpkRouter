# ブラウザ音声キャプチャ拡張

新規設定でも旧版のトークンが空欄の場合でも、試験用の固定トークンは自動で入力されません。拡張は認証トークンが未設定の間、送信を開始しません。SpkRouterの設定に表示される利用者固有の値を、拡張の設定で保存してください。保存済みのポート・トークン・タブ別設定は保持します。

ChromeとEdgeでVoiceMixer Edge PCM拡張を読み込み、対象タブの音声をSpkRouterへ送れます。

[インストール案内](spk-browser-capture.html#install)と[接続設定・開始と停止](spk-browser-capture.html#setup)を参照してください。SpkRouterの基本設定からも同じ案内を開けます。

## ソースから作る

```powershell
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
```

browser-extension/distをChromeのchrome://extensions、Edgeのedge://extensionsから読み込みます。SpkRouterは初期ポート18766を使用するので、拡張の共通接続設定をSpkRouterに合わせます。ポート・認証トークンはブラウザの全タブで共通、ストリームキーは対象タブごとです。

タブの「同時再生」はブラウザ側で音声を鳴らす設定です。入力の切替はSpkRouterで、キャプチャの開始はブラウザ側で操作します。更新時に拡張を再読み込みすると、実行中のすべてのキャプチャに影響します。
