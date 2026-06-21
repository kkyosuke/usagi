---
number: 74
title: fix(tui): 切替/統括/在席でセッションが live のとき矢印・j/k キーが効かない
status: done
priority: high
labels: [fix, tui]
dependson: []
related: [66]
created_at: 2026-06-21T01:27:21.191375+00:00
updated_at: 2026-06-21T01:27:24.906861+00:00
---

## 症状

没入(Attached)で `Ctrl-O` を押して切替(Switch)に抜けると、ヘッダーのモードは switch になるのに、↑/↓（や `j`/`k`）でセッション間を移動できない。Enter はなぜか効く。切替へ通常入る（live セッションが無い状態で統括から `Ctrl-O`）と矢印は効くので、「live セッションがある状態の切替」でだけ navigation が死ぬ。

## 原因

`src/presentation/tui/term_reader.rs` の `read_key_timeout`（`animate` 時のポーリング読み取り）が、**ターミナルが cooked（canonical）モードのまま** `crossterm::event::poll` を呼んでいた。

- イベントループは `console` の「1 キーごとに raw を on/off する」読み取りに依存しており、読み取りの**合間は cooked モード**（`AlternateScreenGuard` / `echo.rs` は `ECHO` を落とすだけで canonical は維持）。
- canonical モードの tty は、ラインディシプリンが**改行で 1 行確定するまで** readable にならない。そのため `Enter` を伴わない矢印・`j`/`k` は `poll` から「ready」に見えず、`Ok(None) => continue` のティックを延々と繰り返すだけで**一度も読まれない**。
- `animate` は #66 で `state.has_live_sessions()` が追加され、**live セッションがあれば常に真**になる。没入から `Ctrl-O` で抜けた直後はそのセッションが生きているため必ず `animate` 経路に入り、矢印が握り潰される。
- 無入力時は `read_key()`（`console` がその読み取りの間だけ raw 化）なので矢印が通る。これが「live が無ければ動く／あると動かない」の差。

## 対応

`read_key_timeout` の `poll`＋デコードを **raw モードで囲う**（RAII の `RawModeGuard` で退出時に cooked へ復元）。これでラインディシプリンが各キーを即時に届け、`poll` がキー単位で ready を返す。`console` がブロッキング経路で使う「読み取りの間だけ raw」と同じ挙動を timeout 経路にも適用しただけで、ループの他の挙動は不変。

`term_reader.rs` は端末 I/O 専用ラッパでカバレッジ除外（`scripts/coverage.sh`）。`cargo fmt` / `clippy -D warnings` / `test`（1382 passed）通過。

## 確認方法

- 没入 → `Ctrl-O` → 切替 で ↑/↓・`j`/`k` がセッションを移動できること。
- live セッションがある状態の統括/在席でもキー入力が遅延・欠落しないこと。
