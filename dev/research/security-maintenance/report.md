# 日次セキュリティ調査

## 2026-11-22 heapless公告9件の適用範囲と移行検証

編集元は `C:/develop/KotorinToys/tools/VoiceMixerSpkRouter`、HEADは `837d71c`。既存のREADME.md編集と未追跡LICENSEがある。今回の確認時点の製品依存はheapless 0.8.0で、修正は未適用。

### Windows通常構成

`cargo tree --locked --offline --target x86_64-pc-windows-msvc -e normal,build -i heapless` と `-e features -i heapless` を実行した。両方とも次の経路が選択される。

`voicemixer-spk-router 1.0.16 → voicemixer-audio-engine 0.4.8 → oxiaudio-encode 0.2.1 → flacenc 0.5.1 → heapless 0.8.0`

heaplessの選択featureはdefaultとserde。flacencの制約は `heapless = "0.8.0"`、oxiaudio-encodeの制約は `flacenc = "0.5.1"`。crates.io公式APIを同日に照合した最新安定版もflacenc 0.5.1、oxiaudio-encode 0.2.1、heapless 0.9.4だった。0.9.4は0.8系の互換制約外で、lockだけの更新では解決しない。

取得元はcrates.io registry。flacencの公式リポジトリは [yotarok/flacenc-rs](https://github.com/yotarok/flacenc-rs/)、heaplessは [rust-embedded/heapless](https://github.com/rust-embedded/heapless)。[公開済みflacencの依存定義](https://docs.rs/crate/flacenc/0.5.1/source/Cargo.toml)をローカルのregistryソースと照合した。

### 公告対象APIと実利用

2026年11月19日登録の9件を公式RustSecで確認した。修正版は0.9.4。バージョン一致と、製品から問題の成立条件へ到達することは別に確認した。

| 公告 | 対象API・条件 | 今回確認した実利用 |
|---|---|---|
| [0574](https://rustsec.org/advisories/RUSTSEC-2026-0574.html) | SortedLinkedListのFindMutView::pop | heaplessのこの型の参照なし |
| [0575](https://rustsec.org/advisories/RUSTSEC-2026-0575.html) | Spsc::peekのatomic ordering | heaplessのSpscの参照なし |
| [0576](https://rustsec.org/advisories/RUSTSEC-2026-0576.html) | IndexMap::truncate | heaplessのIndexMapの参照なし |
| [0577](https://rustsec.org/advisories/RUSTSEC-2026-0577.html) | Deque、HistoryBuf、IndexMap、Vec::IntoIterのDrop中のpanic | flacencはheapless::Vecを使用する。確認した要素はu8、i32、f32、f64で、panicする独自Dropの利用は検出しなかった |
| [0578](https://rustsec.org/advisories/RUSTSEC-2026-0578.html) | SortedLinkedList::zeroize | 該当型の参照なし、zeroize featureも未選択 |
| [0579](https://rustsec.org/advisories/RUSTSEC-2026-0579.html) | Deque::try_from、影響APIの範囲は0.9.2以上0.9.4未満 | 0.8.0はこのAPI範囲外 |
| [0580](https://rustsec.org/advisories/RUSTSEC-2026-0580.html) | arc_pool、box_pool、object_pool、旧pool | heaplessのpoolの参照なし。旧poolの対象は0.8.0未満 |
| [0581](https://rustsec.org/advisories/RUSTSEC-2026-0581.html) | Deque::make_contiguous | heaplessのDequeの参照なし |
| [0582](https://rustsec.org/advisories/RUSTSEC-2026-0582.html) | 大きなIndexMap::insert | heaplessのIndexMapの参照なし |

確認範囲は製品のsrc・crates、registryのflacenc 0.5.1とoxiaudio-encode 0.2.1。flacencの固定長VecはFLACヘッダとLPC係数・初期サンプルに使用される。製品エンジンの `recording/codecs.rs` と `recording/writer.rs` からFLAC符号化に到達する。通常音声経路のキューとしてheapless::Spscを使用している形跡はない。静的調査の結果であり、全実行経路の安全性の証明ではない。

### 隔離移行案の検証

- [x] HEADをgit archiveで `dev/history/v1.0.16/logs/security-heapless-20261122/candidate/` へ展開した。registryのflacenc 0.5.1をそのコピー内のvendor/flacencへ複製し、heapless制約だけ0.9.4へ変更した。隔離コピーのCargo.tomlにflacencのpath patchを設定した。
- [x] 隔離コピーで `cargo update -p heapless --precise 0.9.4` が成功。lockの変更はflacencのpath参照とheapless 0.9.4への移行で、serde依存がserde_coreへ変わった。
- [x] `cargo check --locked --bin voicemixer-spk --target x86_64-pc-windows-msvc --target-dir ../build` でflacencにE0308を2件検出した。場所は `src/component/datatype.rs:2007` と `:2100`。Vec::from_sliceのエラー型を `map_err(|()| ...)` で受けているが、0.9系はCapacityErrorを返す。
- [x] heapless 0.9.4のrust-versionは1.88。製品Cargo.tomlの宣言1.85との整合も検討が必要。
- [ ] 音声・接続・停止の回帰と製品ビルド。隔離案にコンパイルエラーがあるため未実施。

[上流変更履歴](https://github.com/rust-embedded/heapless/blob/main/CHANGELOG.md)にもエラー型変更と必要Rustバージョンの引き上げが記載されている。移行案にはflacenc側の少なくとも2か所の修正と上流ソースの保守が必要になるため、製品への採用は承認待ちとする。承認対象はflacencの限定的なローカル修正を含む0.9.4移行、Rust対応条件の見直し、合成PCMによるFLAC保存・読み戻しと通常回帰による検証。

### 復旧と継続

製品Cargo.toml/Cargo.lockは現行のまま。再開は隔離candidateから行い、採用前に通常音声・接続・停止とFLAC回帰に合格させる。移行案を破棄する場合はこのcandidateを採用対象から外す。将来採用した変更の復旧は、その依存変更コミットだけをrevertして旧lock・依存定義・上流参照を戻し、旧構成を再ビルドする。既存README編集とLICENSEを復旧対象へ含めない。

前回のQUIC未選択の判断は継続する。今回の隔離lock差分でquinn関連の変更はない。

## 2026-11-05 PyTorch公告の別名照合

音声分離プラグインのrequirements.txtにはtorch2.5.1/torchaudio2.5.1/demucs4.0.1が固定されている。[GHSA-63cw-57p8-fm3p](https://github.com/pytorch/pytorch/security/advisories/GHSA-63cw-57p8-fm3p)が本日のOSV照合で新規IDとして現れたが、CVE-2026-24747/PYSEC-2026-2286は前回から追跡済み。同じ問題の別名であり新しい実攻撃件数へ加算しない。

公式原文公開2026年1月26日（OSV登録1月27日）、今回OSV更新11月4日。影響<=2.9.1、修正>=2.10.0。細工されたcheckpointをtorch.load(..., weights_only=True)で読み込む場合にもメモリー破壊と任意実行につながる。weights_only=Trueだけで回避できるとは扱わない。2.5.1の固定版は影響範囲内。

engines.pyではDemucsのget_model(descriptor(method)["model_entry"], repo=folder)を使う。自作ソースに直接torch.loadの呼出しは検出しないが、Demucsの推移読込み経路があるため安全と断定しない。コードには選択methodや管理モデルの制約があるが、通常Python環境/実モデルの導入状態/実読込み/攻撃者制御の成立までは未確認。モデル・checkpoint・音声・個人設定を読まず、取得・実行・GPU/実機試験をしない。

[公式修正版配布](https://pypi.org/project/torch/2.10.0/)のmetadataと配布情報を確認した。torch/torchaudio/CUDA/DeepFilterNet/Demucsの世代・互換性評価を伴うため既存のB（ML依存移行設計待ち）を継続する。2.5.1→2.10.0を無条件置換せず、未回答を承認とせず新規の重複質問もしない。信頼できないcheckpointを読み込まないことが暫定回避策。

製品ソース/依存pin/通常環境/モデル/サービスは未変更、未配布。文書のみ差分・リンクを検査し、形式的な製品build/音声/GUI試験は実施しない。[横断の日次報告](C:/develop/default/dev/research/security-maintenance/2026-11-05.md)で既存保留と確認範囲を管理する。
