---
number: 222
title: docs(tui): v2 TUI の v1 UI/UX parity 契約を正本化する
status: done
priority: high
labels: [tui, design, docs]
dependson: []
related: []
created_at: 2026-07-12T11:57:51.403993+00:00
updated_at: 2026-07-12T12:32:13.243958+00:00
---

## 目的

v2 を v1 と同様の UI/UX で利用可能にするため、実装前の受け入れ契約を作る。設計の正本は document/proposals/06-tui-v1-parity.md とする。

## スコープ

- v1 の現行コードとテストを優先し、画面・操作・非同期 UX を調査する。
- v2 の現状と gap、parity 方針、優先度、acceptance test、daemon 依存を表にする。
- Home の Switch / Closeup と modal scope、MVP、後回し項目、外部 checkpoint を定義する。
- proposal 目次を更新し Markdown link check を通す。

## 対象外

- TUI / daemon / core の Rust 実装
- daemon wire contract の変更
- 退避版 v1/document の変更

## 完了条件

- proposal と目次が同じ PR でレビュー可能になっている。
- コード参照付きで v1/v2 の差分と受け入れ条件が追跡できる。
- issue が同じブランチ上で done になり、docs-only の検証結果が PR に記載されている。
