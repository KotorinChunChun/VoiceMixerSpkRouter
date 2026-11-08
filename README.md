# VoiceMixerSpkRouter

Windowsの再生デバイス・特定アプリ・ブラウザの音声を選び、2台の再生デバイスへ転送する常駐アプリです。

VoiceMixer本体から独立したリポジトリです。単独でビルドでき、VoiceMixer本体やMic版のチェックアウトは不要です。

- 取得元は再生デバイス・アプリ・ブラウザ・停止の4択。
- 出力1・2は個別にON/OFF。停止すると取得と転送を止めます。
- 既定キーはShift+F1〜F4。変更・無効化と、共存／上書きを各キーで選べます。
- オーバーレイ、トレイ、設定画面を備えます。
- Chrome/Edgeのタブ音声は付属の拡張から取得します。

## こんなときに使えます

- 特定のアプリやブラウザタブの音声を、仮想オーディオデバイスなどを通して配信・録音ソフトへ渡す。
- 普段は聞いていない仮想デバイスの音を、必要なときだけ自分のスピーカーやヘッドホンでモニターする。
- 配信・録音用と自分のモニター用に出力先を分け、必要な出力だけをONにする。

OBSなどの配信・録音ソフトにも音声を選ぶ機能はあります。このツールは、オーバーレイや割り当てたキーで取得元を切り替えたり、出力先を手元でON/OFFしたりしたいときに使います。

取得元は一度に一つです。配信・録音ソフト側でも転送先の音声を受け取る設定が必要です。また、出力をOFFにしても元アプリの音量や再生先は変わりません。自分には聞こえないようにしたい場合は、元の再生先も含めて設定してください。

## ダウンロードして使う

[GitHub Releases](https://github.com/KotorinChunChun/VoiceMixerSpkRouter/releases)からWindows x64用のsetup.exeを取得し、実行してください。利用者単位の導入で、Rust・Node.js・.NET・PowerShell 7の追加インストールは不要です。Windows 11 x64が対象です。

初回は設定画面が開き、取得停止・両出力OFFで起動します。同梱の「はじめに.html」に初期設定とブラウザ拡張の導入手順を記載しています。設定は導入先のspk-settings.jsonへ生成し、更新・削除では保持します。

KotorinToys向けの管理契約と配布台帳を同梱しています。本体側の修正・製品登録は今回の対象外です。[導入・更新・削除と本体連携](docs/kotorintoys-integration.md)に製品IDと配布仕様をまとめています。

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
