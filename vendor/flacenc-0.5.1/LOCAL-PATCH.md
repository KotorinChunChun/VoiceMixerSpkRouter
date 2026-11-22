# flacenc 0.5.1 の同梱元と変更

[crates.ioのflacenc 0.5.1](https://crates.io/crates/flacenc/0.5.1)を基にしています。配布アーカイブのSHA-256は `74c892c2b5fa08f967e8b5ad29121570b4762202f2402033ce08479ec65eccd0` です。上流コミットは `4e9e73ae9a5072bd3de904c4a516401f425fde07` です。ライセンスはApache-2.0で、本文は同梱のLICENSEにあります。

heaplessの依存を0.9.4へ変更し、`src/component/datatype.rs` の2か所で `Vec::from_slice` が返す容量エラーの受け方を変更しました。上流のテスト用音声は製品のビルドで使わないため同梱していません。heapless 0.9.4以降を要求するflacencの公式版が公開されたら、FLACの保存と読み戻しを再検証してCargoのpath patchを外します。

`src/source.rs` のフレームバッファーは、最終フレーム向けに1サンプルから確保できるようにしました。符号化設定のブロックサイズ制約は従来のままです。末尾3フレームを含む録音で、停止後の保存とサンプル数を検証します。[FLAC仕様 RFC 9639 §4.1](https://www.rfc-editor.org/rfc/rfc9639.html#section-4.1)
