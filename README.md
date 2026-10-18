# VoiceMixerSpkRouter

Windowsの再生デバイス・特定アプリ・ブラウザの音声を選び、2台の再生デバイスへ転送する常駐アプリです。

VoiceMixer本体から独立したリポジトリです。単独でビルドでき、VoiceMixer本体やMic版のチェックアウトは不要です。

- 取得元は再生デバイス・アプリ・ブラウザ・停止の4択。
- 出力1・2は個別にON/OFF。停止すると取得と転送を止めます。
- 既定キーはShift+F1〜F4。変更・無効化と、共存／上書きを各キーで選べます。
- オーバーレイ、トレイ、設定画面を備えます。
- Chrome/Edgeのタブ音声は付属の拡張から取得します。

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

[操作と仕様](docs/spk-router.md)、[ブラウザ拡張の導入・設定](docs/spk-browser-capture.html)、[開発者ガイド](docs/DEVELOPERS_GUIDE.md)を参照してください。

MSTSC前景のキー検知に対応しますが、OSが保護する操作や権限の異なるウィンドウまで抑止する保証はありません。実接続・物理キーと自動試験の範囲は[検証記録](dev/testing/spk-router-results.md)で区別します。
