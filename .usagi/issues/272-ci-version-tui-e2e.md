---
number: 272
title: ci: 配布 version 変更時だけ重い TUI E2E を実行する
status: done
priority: medium
labels: [ci, test, tui, release]
dependson: []
related: []
created_at: 2026-07-13T01:34:09.728133+00:00
updated_at: 2026-07-13T01:37:30.502286+00:00
---

## 目的

通常の PR を軽く保ちながら、出荷 version を変更する PR とリリース候補の検証で v1 の実 PTY TUI E2E を必ず実行する。

## 受け入れ条件

- 配布 version の正本である `v1/Cargo.toml` の `[package].version` を、PR の base commit と正確に比較する。
- version が変わらない通常 PR では重い `cargo test --manifest-path v1/Cargo.toml --test tui_e2e --quiet` を実行しない。
- version を変更する PR、merge queue の検証、および明示的な手動検証では同 target を実行できる。
- fork PR でも secrets や write 権限に依存せず安全に判定・実行する。
- 既存の `auto-release.yml` / `release-build-check.yml` の v1 version 起点と整合させ、CI 正本ドキュメントと検証を更新する。

## 範囲

GitHub Actions の gate、version 比較ロジックとそのテスト、CI 運用ドキュメント。
