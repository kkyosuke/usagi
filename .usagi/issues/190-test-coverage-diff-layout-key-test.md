---
number: 190
title: test: coverage 下の diff layout key test の順序依存を解消する
status: todo
priority: high
labels: [test, flaky]
dependson: []
related: [181]
parent: 177
created_at: 2026-07-11T00:55:13.258324+00:00
updated_at: 2026-07-11T00:55:13.258324+00:00
---

## 背景

issue #181 の `cargo llvm-cov` 比較で全 test は成功したが、`src/presentation/tui/home/event/mod.rs` の `Key::Char('v') => state.diff_toggle_layout()` が 1 line 未到達となり、line coverage gate が 99.999%（表示上 100.00%）で失敗した。cargo test runner と nextest runner の双方で同じ profile merge 結果を確認した。通常 full suite 2 回では test failure はなかった。

## 調査

- 該当 key dispatch test が共有/global state または test ordering に依存していないか確認する
- llvm-cov profile を clean にした反復で再現率を取る
- test 単体と module/full suite の到達差を確認する

## 完了条件

clean coverage を複数回実行して 100% line/function coverage が安定し、該当 arm が各実行で到達する。
