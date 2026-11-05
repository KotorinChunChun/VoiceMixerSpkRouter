# 日次セキュリティ調査

## 2026-11-05 PyTorch公告の別名照合

音声分離プラグインのrequirements.txtにはtorch2.5.1/torchaudio2.5.1/demucs4.0.1が固定されている。[GHSA-63cw-57p8-fm3p](https://github.com/pytorch/pytorch/security/advisories/GHSA-63cw-57p8-fm3p)が本日のOSV照合で新規IDとして現れたが、CVE-2026-24747/PYSEC-2026-2286は前回から追跡済み。同じ問題の別名であり新しい実攻撃件数へ加算しない。

公式原文公開2026年1月26日（OSV登録1月27日）、今回OSV更新11月4日。影響<=2.9.1、修正>=2.10.0。細工されたcheckpointをtorch.load(..., weights_only=True)で読み込む場合にもメモリー破壊と任意実行につながる。weights_only=Trueだけで回避できるとは扱わない。2.5.1の固定版は影響範囲内。

engines.pyではDemucsのget_model(descriptor(method)["model_entry"], repo=folder)を使う。自作ソースに直接torch.loadの呼出しは検出しないが、Demucsの推移読込み経路があるため安全と断定しない。コードには選択methodや管理モデルの制約があるが、通常Python環境/実モデルの導入状態/実読込み/攻撃者制御の成立までは未確認。モデル・checkpoint・音声・個人設定を読まず、取得・実行・GPU/実機試験をしない。

[公式修正版配布](https://pypi.org/project/torch/2.10.0/)のmetadataと配布情報を確認した。torch/torchaudio/CUDA/DeepFilterNet/Demucsの世代・互換性評価を伴うため既存のB（ML依存移行設計待ち）を継続する。2.5.1→2.10.0を無条件置換せず、未回答を承認とせず新規の重複質問もしない。信頼できないcheckpointを読み込まないことが暫定回避策。

製品ソース/依存pin/通常環境/モデル/サービスは未変更、未配布。文書のみ差分・リンクを検査し、形式的な製品build/音声/GUI試験は実施しない。[横断の日次報告](C:/develop/default/dev/research/security-maintenance/2026-11-05.md)で既存保留と確認範囲を管理する。
