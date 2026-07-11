---
number: 201
title: CI に sccache 実験を導入する
status: done
priority: medium
labels: [ci, perf]
dependson: []
related: []
created_at: 2026-07-11T05:34:18.373316+00:00
updated_at: 2026-07-11T05:37:13.777332+00:00
---

## 背景

#191 で local sccache opt-in helper とベンチ手順が入った。次段階として required Rust gate の ubuntu jobs に絞り、CI 上で sccache の効果を観測できるようにする。

## やること

- `.github/workflows/test.yml` の ubuntu Rust jobs で sccache を有効化する。
- `.github/workflows/coverage.yml` の ubuntu coverage job で sccache を有効化する。
- 既存の `swatinem/rust-cache@v2` は残し、sccache cache dir は separate key で `actions/cache` 管理にする。
- cache key は OS、Rust toolchain、Cargo.lock、workflow/job を含め、過剰共有しない。
- `sccache --show-stats` を `if: always()` で CI log に残す。
- CI 実験の観測方法、採否基準、no-go 範囲を proposal/docs に反映する。

## 検証

- YAML 構文の軽い確認
- `scripts/tests/cargo-sccache.sh`
- `git diff --check`

## 完了条件

PR に目的、変更内容、CI 観測項目、検証結果、release/test-metrics 等へ広げない範囲を明記する。
