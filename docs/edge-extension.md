# ブラウザ音声キャプチャ拡張

ChromeとEdgeでVoiceMixer Edge PCM拡張を読み込み、対象タブの音声をSpkRouterへ送れます。

[インストール案内](spk-browser-capture.html#install)と[接続設定・開始と停止](spk-browser-capture.html#setup)を参照してください。SpkRouterの基本設定からも同じ案内を開けます。

## ソースから作る

```powershell
npm --prefix .\browser-extension ci
npm --prefix .\browser-extension run build
```

browser-extension/distをChromeのchrome://extensions、Edgeのedge://extensionsから読み込みます。SpkRouterは初期ポート18766を使用するので、拡張の共通接続設定をSpkRouterに合わせます。ポート・認証トークンはブラウザの全タブで共通、ストリームキーは対象タブごとです。

タブの「同時再生」はブラウザ側の音声を鳴らす設定です。SpkRouterの入力切替とブラウザ側のキャプチャ開始は別操作です。更新時の拡張の再読み込みは実行中の全キャプチャへ影響します。
