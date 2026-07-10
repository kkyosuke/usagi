---
number: 179
title: ci: Rust gate を段階化し重複 build を除去する
status: todo
priority: high
labels: [ci, test]
dependson: []
related: []
parent: 177
created_at: 2026-07-10T23:35:22.675670+00:00
updated_at: 2026-07-10T23:35:22.675670+00:00
---

## 目的

PR の信頼性を落とさず failure feedback を早める。現行 `test.yml` は fmt→clippy→build→test を 1 job で直列実行し、`cargo build` は後続 `cargo test` と成果物が重複する。coverage workflow も別に全 test を実行する。

## 変更案

- fmt/lint と full test を独立 job にして同時開始する。full `cargo test --quiet` と coverage 100% は required のまま維持する。
- 明示的 `cargo build --verbose` を削除する（`cargo test` が lib/bin/test target を build、clippy は all-targets）。bin の非-test build 保証が必要なら `cargo check --bins` の安価な明示に置換する。
- workflow に PR/branch 単位の `concurrency` + `cancel-in-progress: true` を追加し、古い commit の実行を止める。
- job ごとの cache key/target sharing、cold/warm wall time と billed minutes を変更前後で記録する。並列化で総 compute が増える場合は failure latency とのトレードオフを明記する。
- branch protection の required check 名を変更前に棚卸しし、名前変更で保護が外れないよう移行する。

## 完了条件

PR で fmt/clippy/full test/coverage がすべて gate され、故意の各 failure が対応 job を失敗させる。現行比の wall time/compute を PR に記録する。
