---
number: 659
title: perf(core): VT scrollback の先頭 eviction を O(1) にする
status: done
priority: medium
labels: [review, v2, core, terminal, performance, memory]
dependson: []
related: [637, 534]
parent: 654
created_at: 2026-08-05T13:49:11.598201+00:00
updated_at: 2026-08-05T23:11:27.183255+00:00
---

## Finding（P2 CPU / owner-lock latency）

`VtScreen` は scrollback を `Vec<Vec<Cell>>` で保持し、上限超過のたびに `scrollback.remove(0)` を実行する。10,000 行へ達した後は、**新しい1行ごとに残り約10,000 row handleを左へシフト**する。`SCROLLBACK_MAX` が存在するのに cap 判定は `10_000` literal でも重複している。

current build の focused probe（2×40 screen、`x\r\n`）では、最初の10,000行が18msだったのに対し、cap到達後の追加20,000行は1,615msかかった。絶対値は build profile / machine に依存するが、steady state だけが約10,000要素の先頭削除を行う構造差は不変である。retained history量に比例するmemmoveを出力行ごとに払い、daemon terminal owner lock / TUI local parser の双方で高出力時の入力・snapshotを遅らせる。

## 修正方針

- scrollback を `VecDeque` / ring buffer / logical start offset のいずれかへ移し、append + oldest eviction を amortized O(1) にする。
- renderer/checkpoint/resize/selection が oldest→newest の同じ logical row order を読む read-only API を定義する。単に毎回 `make_contiguous()` して O(N) を別箇所へ移さない。
- cap は `SCROLLBACK_MAX` を唯一の定数にし、live parser と hostile checkpoint validation を一致させる。
- `trim_to_cells`、alternate saved primary、checkpoint encode/decode、row indexing の semantics を保つ。

## 受入条件

- cap 到達後の N 行 append が `SCROLLBACK_MAX × N` ではなく N に比例することを計測/visit counterで固定する。
- 100 / 1,000 / 10,000 retained rows で steady append latency が history 長に線形増加しない。
- `scrollback()` の公開 read contractを置換する場合、TUI window projectionと既存 callersが全件移行し、identity/orderの推測を増やさない。
- scrollback cap、resize、alternate screen、checkpoint round-trip、selection/copy parityが維持される。

## 根拠箇所

- `crates/core/src/usecase/vt_screen.rs`: `scrollback: Vec<_>`, `scroll_region_up`, `trim_rows`
- `crates/core/src/usecase/vt_screen/checkpoint.rs`: `SCROLLBACK_MAX`
- `crates/tui/src/usecase/application/terminal_screen.rs`: retained row indexing
