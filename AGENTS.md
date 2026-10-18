# VoiceMixerSpkRouter の作業ルールと目録

このリポジトリをSpkRouterの正本とする。VoiceMixer本体とは別のGit履歴・origin・ビルドを持ち、兄弟フォルダーへの依存を追加しない。

- 質問には回答だけを行い、明確な変更指示がない限りデータを変更しない。
- PowerShellは常にpwsh。ドキュメント・コメント・コミットは日本語。既存変更を保持し、無関係な差分を含めない。
- 必要な資料だけ読む。dev/privateとdev/tempはユーザーが対象を指定した場合だけ参照し、Git・公開物・通常検査から除外する。
- 開発資料は作業中dev直下、完了後dev/history/vX.Y.Zへ移す。要求はユーザーが作成し、AIはimpと必要時だけtaskを作る。過去の未チェックより後続の完了記録を優先する。
- 多項目は工程を分け、適切なビルド・試験後に日本語コミットする。版は更新ごとにpatchだけ1増やし、Cargo.tomlとsrc/spk_router/config.rsを揃える。上位桁は明示指示時だけ変更。
- 取得3候補＋停止、2出力・出力キーなし・停止で取得も止める。設定中のキー操作、MSTSC前景の検出、共存／上書き、未保存編集の保持を守る。
- UIに不要な解説や英語タイトルを追加しない。今回依頼されたキーの説明は表示する。
- 公開前に追跡ファイルを確認し、個人設定・トークン・録音・モデル・キャッシュ・取り込み元の非公開履歴を含めない。公開の明示指示の範囲でpushする。
- 更新は通常終了・旧EXEと設定の退避・設定保持・既存Startupの追従・単一起動確認を行う。署名は依頼がある場合のみ既定CN=KotorinChunChunを使い、秘密鍵をエクスポートしない。
- 対象窓の自動操作・撮影はwindows-ui-automationの手順に従い、画面全体を撮影しない。実接続・物理入力・聴感と自動試験を区別する。
- まとまった完了報告には、起動中でも絶対パスのcdから始まる起動コマンドを載せる。実機確認を求める場合は-Debug。
- 新規・移行時はDevLauncherの原本と共通台帳の同じIdを更新する。個人上書きを変更しない。

|目的|入口|
|---|---|
|概要・利用|README.md、docs/spk-router.md|
|ビルド・改造|docs/DEVELOPERS_GUIDE.md|
|ブラウザ導入・設定|docs/spk-browser-capture.html|
|構造・境界|docs/architecture/spk-router.md|
|検証|dev/testing/spk-router-results.md、dev/scripts/tests/|
|公開・配備|dev/RELEASE_RULE.md|
|今回の実装|対応するdev/vX.Y.Z-imp.md、完了後dev/history/|
