---
number: 693
title: build: release.yml に manifest / prerelease input を足して v2 beta channel を出す
status: todo
priority: high
labels: [build, ci]
dependson: []
related: [690]
created_at: 2026-08-17T23:19:48.622418+00:00
updated_at: 2026-08-17T23:19:48.622418+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P1: v2 prerelease channel」が正本。

## 背景

`release.yml` は reusable（`workflow_call`）だが build 対象が `--manifest-path v1/Cargo.toml` に固定され、
**prerelease を作る手段が無い**。v2 を試せる artifact が存在しないため、試用 channel の起点になる。

## やること

### 1. input を 2 つ足す

| input | 既定 | 用途 |
|---|---|---|
| `manifest` | `v1/Cargo.toml` | build 対象の Cargo manifest。v2 は root の `Cargo.toml` |
| `prerelease` | `false` | `softprops/action-gh-release` の `prerelease` へ渡す |

- `Build binary` step の `--manifest-path`、`Prepare artifact` の binary コピー元 path、
  `swatinem/rust-cache` の `workspaces` を manifest から導く。
- root manifest は nightly pin（`rust-toolchain.toml`）で `coverage_attribute` を要求する。現在の
  release build は `dtolnay/rust-toolchain@stable` + `cargo +stable` なので、**v2 の build は
  `rust-toolchain.toml` の pin を使う**経路にする（`+stable` を manifest 依存で外す）。

### 2. `prerelease: true` を必須にする（最重要）

現在 `prerelease` を渡していないため、v2 の release を作るとそれが `/releases/latest` になり、
**`install.sh` の既定経路が stable 利用者を v2 へ引き込む**。opt-in が opt-out に反転する。
`v3.0.0` 形式の tag は `resolve_latest_release` の厳格 filter（`^v?\d+\.\d+\.\d+$`）も通ってしまう。

- beta channel の呼び出しは必ず `prerelease: true` を渡す。
- 呼び出し側が渡し忘れても事故にならないよう、**tag に prerelease 識別子（`-beta.`）が含まれるのに
  `prerelease: false` の組み合わせは job を失敗させる**。

### 3. tag 規約

v1 の version は 2.9.1、root（v2）は 2.6.0 で **semver では v2 のほうが小さい**。`2.x` の続きとして
tag を切ると version 比較・release notes・利用者から見た順序が逆転する。v2 は次の major を主張する。

- beta: `v3.0.0-beta.N`
- 正式版: `v3.0.0`（cutover 時）

### 4. release notes の PREV_TAG

`release-notes` job の `git tag --sort=-v:refname` から自分以外の先頭を取る実装は、beta tag を挟むと
v1 系の履歴を丸ごと拾う。**同じ channel 内の直前 tag**に絞る。

### 5. 起点は手動 dispatch

`auto-release.yml` と対称に root `Cargo.toml` を監視することはしない。root の version は `build.rs` の
identity や `usagi version` にも使う開発中の値で、bump ごとに公開 release が出るのは beta の運用として重い。

## テスト・確認方法

- `scripts/ci/required-contexts.sh audit-workflows` で workflow / job / context 名の drift が無いこと。
- 実際の tag を切る前に `workflow_dispatch` で 4 プラットフォームの build が通ることを確認する
  （`release-build-check.yml` は `v1/Cargo.toml` 差分に反応するため、root manifest 用の経路が必要か
  この issue で判断する）。
- prerelease flag と tag の組み合わせ検証は fixture test で固定する。
