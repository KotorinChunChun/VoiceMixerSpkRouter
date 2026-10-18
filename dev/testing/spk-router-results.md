# SpkRouter 検証記録

## 独立化前の基準

v1.0.8は通常57件、GUI・監視6件、実EXE UI12ケース、ライフサイクル13ケースが成功。2出力・出力キーなし・停止で取得停止、MSTSC前景監視、共存／上書き、設定中のキー操作を維持する。

## 独立化以降

今回の測定結果を追記する。実MSTSC接続・物理キー・聴感は自動試験と区別する。

## v1.0.9 独立化と設定画面（2026-10-19）

- 独立したCargoワークスペースで通常57件、実GUI・監視6件、実EXE UI12ケース、ライフサイクル13ケース成功。Debug/Releaseビルドとブラウザ拡張ビルド成功。
- 基本設定だけに公開GitHubリンクを表示。キー設定に共存／上書きの説明を表示し、クリック通知、タブ別表示、配置と編集保持を確認した。840×580・16pxを維持する。
- UI証跡: tmp/spk-router-v109/run-039ecb3fd4204178bf7486c1e2d4a7a6/。ライフサイクル: tmp/spk-router-refactoring/lifecycle/run 86f4926ba57943ebae433a5507a2e31c/。専用設定で対象窓だけ撮影した。
- 旧・新設定のハッシュ一致、未保存編集とページ復元、独立先EXEの単一起動、旧起動入口からの呼出しを確認した。Startupは元から未登録だったため、新規登録していない。
- VoiceMixer本体のGitとは別の履歴・originで管理。音声エンジンをローカル同梱し、元プロジェクトを参照しないことをmetadataとビルドで確認。元の履歴・個人設定は公開対象に含めない。
- OSが保護するキー・権限境界の制約は従来どおり。実MSTSC接続・物理キー・新しい聴感確認を今回測定済みとは扱わない。

公開確認: 専用リポジトリのvisibility=PUBLIC、認証なしHTTP 200、mainとREADME、ローカルとリモートのSHA一致を確認。GitHub ActionsのWindows検証も開始している。

GitHub Actions Windows検証: [実行39144771665](https://github.com/KotorinChunChun/VoiceMixerSpkRouter/actions/runs/39144771665)がsuccess。公開ソースのクリーンチェックアウトで通常試験、Releaseビルド、npm ci、ブラウザ拡張ビルドが全工程成功した。測定対象は3dbe968（後続は完了資料のみ）。

## v1.0.10 管理契約とインストーラ（2026-10-19）

通常60件（Debugと静的CRT Release）、実GUI・キー監視6件、Release管理IPC15ケース、予約整理の模擬11件、実setup28ケースが成功。実setupは日本語・空白入り導入先、正式登録、設定なしの初回起動、無表示起動、設定編集中の更新・削除拒否、旧版相当fixtureからの版上げ、設定・トークンの全バイト保持、削除と再導入を確認した。fixtureは現行の管理対応ソースの版だけを下げた試験物で、過去の正式製品からの更新測定ではない。

公開する最終setupでの証跡はtmp/installer-tests/検証 3f12e87680144fb7b971e527cde6c13c/。利用中だったv1.0.9と未保存編集を復元し、検証用の正式導入登録は解除した。他製品の並行操作に由来する本体設定の差分は操作記録の製品IDで区別し、上書きしていない。

[GitHub Releases v1.0.10](https://github.com/KotorinChunChun/VoiceMixerSpkRouter/releases/tag/v1.0.10)へsetup・製品別台帳・SHA-256を公開。3 asset再取得と認証なしsetup取得でハッシュ一致。KotorinToys本体の変更・登録は対象外。実本体UIからの導入、実MSTSC接続、物理キー、聴感は今回の測定に含めない。[詳細](../history/v1.0.10/v1.0.10-imp.md)を参照。
