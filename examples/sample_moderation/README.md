# モデレーションのサンプル

標準 FORTRAN77 のサンプルは，模擬チャンネルで人間が投稿した
`moderate demo` に反応します．`D7CDP` と `D7CTP` で権限を確認してから，
対象メッセージの削除と投稿者への 60 秒のタイムアウトをそれぞれ試みます．
いずれかの操作が成功または dry-run になった場合は，処理結果を
`D7LOG` と模擬記録チャンネルに送ります．チャンネル ID は同梱の
模擬 Gateway イベントに対応する架空の値です．

リポジトリのルートからランタイムとサンプルをビルドします．

```sh
cargo build
make fortran-sample
```

模擬イベントを再生して監査ログに `D7DEL`，`D7TMO`，`D7SND` が
記録されることを確認します．確認が終わったら Ctrl-C で停止します．

```sh
target/debug/dfor77 \
  --mock-gateway examples/sample_moderation/mock_gateway.jsonl \
  --audit-log ./dfor77-audit.jsonl \
  --dry-run \
  --bot target/fortran/sample_moderation
```

`sample_moderation_extension.f` は，`INCLUDE 'dfor77.inc'` で定数を
読み込む短いサンプルです．`make fortran-extension-sample` でビルドできます．
