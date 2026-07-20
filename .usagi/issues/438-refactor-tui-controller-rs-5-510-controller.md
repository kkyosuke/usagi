---
number: 438
title: refactor(tui): controller.rs（5,510 行）を controller/ ディレクトリへ分割する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: [410]
related: []
created_at: 2026-07-20T12:01:48.607896+00:00
updated_at: 2026-07-20T12:01:48.607896+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/tui/src/usecase/application/controller.rs` は **5,510 行**。home reducer・overlay 群・create session・sidebar ジオメトリ・Entry/New reducer（本番未使用、#410）・テスト用 fake が 1 ファイルに同居。
- fake は同型のものが 3 コピー存在する（テストモジュール内）。

## 問題

reducer 中枢の見通しが悪く、変更のたびに 5,000 行ファイルを往復する。未使用 reducer の削除（#410）後も 4,000 行超が残る見込みで、分割の適期。

## 改善案（要検討）

- `controller/` ディレクトリへ分割: home / overlays / create_session / sidebar_geometry /（Entry・New を残す判断なら entry, new）/ fakes（generic 化して 3 コピー統合）。
- #410（未使用 reducer の配線/削除決定）の**後**に実施すると移動量が減る（本 issue は #410 に依存）。

## 受け入れ条件

- [ ] controller.rs が責務ごとのサブモジュールに分割されている。
- [ ] テスト fake が 1 実装に統合されている。
- [ ] 既存 reducer の挙動が回帰しない（既存テスト維持）。coverage 100% を維持。
