---
number: 202
title: ci: warmed sccache CI run を追加観測する
status: todo
priority: medium
labels: [ci, perf]
dependson: []
related: [201]
created_at: 2026-07-11T05:51:18.747834+00:00
updated_at: 2026-07-11T05:51:18.747834+00:00
---

## 背景

#201 / PR #747 で required ubuntu Rust jobs に sccache 実験を入れた。merge 後の初回 Test run は sccache cache miss かつ RUSTC_WRAPPER 追加で swatinem/rust-cache key も変わり、baseline より遅かった。Coverage は PR run で success / 100% gate を維持したが、sccache hit rate は 0% だった。

## やること

- #747 後の同一 cache key の Test main push run を 3 回観測する。
- #747 後の Coverage PR run を 3 回観測する。
- 各 run で job duration、workflow duration、cache restore/save time、sccache --show-stats の hit/miss/non-cacheable/cache size を記録する。
- 近い sccache なし baseline (#745/#743 など) と比較し、setup/cache save を含めて required Rust jobs が 10% 以上短縮するか判断する。
- workflow duration または billed minutes 相当が悪化する場合は、sccache を外す、または actions/cache/sccache 対象 job を絞る調整 issue を作る。

## 完了条件

- warmed run の観測結果が document/proposals/04-sccache-rust-builds.md に追記されている。
- go / no-go / 調整の判断が明記されている。
- go の場合でも release build check / release workflow / test-metrics への展開は別 issue として起票されている。
