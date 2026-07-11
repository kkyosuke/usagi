---
number: 203
title: ci: required Rust gate の sccache 対象を絞る
status: done
priority: medium
labels: [ci, perf]
dependson: [202]
related: [201, 202]
created_at: 2026-07-11T06:50:52Z
updated_at: 2026-07-11T07:32:00Z
---

## 背景

#202 の warmed 追加観測では、#747 で入れた required ubuntu Rust jobs の sccache 実験が CI 採用基準を満たさなかった。

- `Test` main push は sccache cache restore に hit したが、実際の compile request は lint 1 件 / full-test 2 件に留まり、job duration 合計は近い sccache なし baseline より短くなかった。
- `Coverage` PR run は PR branch 間で sccache cache が共有されず、追加観測でも 0% hit と save overhead が続いた。
- release build check / release workflow / test-metrics へ広げる根拠はない。

## やること

- `.github/workflows/coverage.yml` から sccache を外す、または PR branch で有効な cache 戦略に変更する。
- `.github/workflows/test.yml` の sccache を外すか、build-heavy で効果が出る job に限定する。
- `swatinem/rust-cache` だけを残した場合の required Rust jobs と、sccache 限定継続の場合の job duration / workflow duration / billed minutes 相当を比較する。
- `document/proposals/04-sccache-rust-builds.md` の判断を、実施した CI 設定に合わせて更新する。

## 完了条件

- required Rust gate の CI 設定が、観測結果に基づく no-go / 限定継続のどちらかへ調整されている。
- PR に #202 の観測結果と、release build check / release workflow / test-metrics へ展開しない判断が明記されている。
- `Test` / `Coverage` の CI が成功している。
