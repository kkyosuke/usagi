---
number: 567
title: test(tui): restore retry の skipped-tick 受入テストが frame skip を待たず flake する
status: done
priority: high
labels: [v2, tui, test, ci]
dependson: []
related: [554]
created_at: 2026-07-26T14:58:29.514342+00:00
updated_at: 2026-07-27T02:20:23.159542+00:00
---

## 問題・影響

`usagi-tui` の `presentation::tests::a_skipped_tick_still_admits_the_restore_retry`
（[#554](./554-perf-tui-frame-budget-file-io-full-rebuild.md) の受入テスト）が run ごとに揺れる。

```text
thread 'presentation::tests::a_skipped_tick_still_admits_the_restore_retry' panicked at
crates/tui/src/presentation/mod.rs:8683:9:
every restore admission followed a redraw: [1, 2, 3]
```

このテストは「frame を skip した tick でも restore retry は admit される」ことを主張しており、
その証拠として **admission の drawn-at が 2 回連続で同じ値になること**（= 間に redraw が挟まらないこと）を
assert する。`[1, 2, 3]` は 3 回の admission すべてが redraw の直後だった run であり、
つまり **その run では frame が 1 度も skip されなかった**。

frame skip は renderer の入力比較で決まるため、controller が何 tick 回るか・どの tick で material が
変わるかに依存する。テストは skip が起きるまで待たず、observed 列に skip が現れることを期待している。

## これは共有 gate を壊す

**failure は `coverage` job の test 実行を abort させるため、`crates/tui` に一切触っていない PR の
coverage gate が落ちる。** 手元でもローカル `coverage_enforce` を 2 回連続で abort させ、**CI でも再現した**。

| 観測 | 場所 |
|---|---|
| ローカル `cargo test -p usagi-tui --lib <name>` を 6 回回して 1 回失敗 | macOS |
| ローカル `coverage_enforce` を 2 回連続で abort | macOS |
| CI `coverage` job の test 実行が abort（[#559](./559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) 系列の daemon 側 PR） | ubuntu-latest |

test 実行が abort すると report 生成まで到達しないため、**sticky な coverage comment は前 run の内容が
残り**、実際には未計測なのに「未達ファイルがある」と読める。これは診断を誤らせる。

## 対象責務

`a_skipped_tick_still_admits_the_restore_retry` を、frame skip が起きたことを**観測してから**
admission 列を判定するようにする。tick 回数や到着順に依存させない。

- controller を「skip が少なくとも 1 回起きる」まで駆動する、または skip を決定的に起こす material を
  fake terminal 側で固定する。
- どちらも取れない場合は、assert を「admission 回数 >= 2 かつ skip 済み tick の admission が存在する」へ
  分解し、skip が 0 回の run は inconclusive として明示的に扱う（無条件 pass にはしない）。

## 非対象

frame budget（#554）の product 挙動そのもの。本 issue は受入テストの決定性だけを扱う。

## 受入条件

- [ ] `cargo test -p usagi-tui --lib a_skipped_tick_still_admits_the_restore_retry` を 50 回連続で回して失敗しない。
- [ ] テストが主張している契約（skip した tick でも restore retry を admit する）は弱まっていない。
- [ ] skip が 1 度も起きない run を「pass」と読み替えていない。
