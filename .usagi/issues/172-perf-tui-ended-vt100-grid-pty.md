---
number: 172
title: perf(tui): 終了（ended）ペインの vt100 grid / PTY リソースを解放する
status: in-progress
priority: medium
labels: [perf, tui]
dependson: []
related: [128, 159]
created_at: 2026-07-10T20:46:50.759156+00:00
updated_at: 2026-07-10T21:20:14.980906+00:00
---

## 背景（メモリ調査 2026-07-11）

TUI の常駐メモリの支配項はペインごとの vt100 grid で、1 ペインあたり `(rows + scrollback) × cols × ~32B/cell` ≒ **既定設定（scrollback 2,000 行・120 cols）で ~7.8MB**（上限 50,000 行なら ~320MB）。`TerminalPool` は全セッションの全ペインを常駐させる設計だが、**プロセスが終了（ended）したペインの grid も解放されない**。

現状、ペインの grid が解放されるのは次の 4 経路のみ:
- 全ペイン死亡後の再入場（`enter()` が stale エントリを drop して respawn）
- 明示的なタブクローズ（`close_tab` / `close_active`）
- worktree 削除（`remove_under`）
- workspace 画面からの離脱（`TerminalPool::drop`）

watcher の毎 tick prune は watcher 側の `Watched` ハンドルを外すだけで、本体 `TerminalPool.sessions` の `Pane` / `PtySession` / grid には触れない。マルチペインのセッションで 1 ペインだけ死んだ場合は上記経路でも残る。長時間運用で「終わったのに ~7.8MB×ペイン数を抱え続ける」状態になる。

## やること

- 全ペイン終了（`any_alive() == false`）を検知したセッション、および個別に ended になったペインについて、**最終画面の軽量スナップショット（可視領域のみ、preview 表示に必要な分）だけ残して parser / scrollback を解放**する。
  - サイドバー preview・切替時の見た目は最終スナップショットで維持する。
  - 再入場（respawn）時は現行どおり新しい PTY を張る。
- watcher の prune（all-dead 検知）と本体側の解放の橋渡しを設計する。#128（pool.rs から monitor / PR スキャンを分離）とファイル境界が重なるため、着手順を調整する（#128 先行が望ましい）。

## トレードオフ・関連

- ended ペインの scrollback を遡れなくなる。「終了直後はまだ読み返したい」ケースに配慮するなら、解放を遅延（例: 終了から N 分後 / 他セッションへ切替時）にする案もある。
- #159（daemon 化 Epic）で vt100 権威が daemon へ移った後は、同じ「ended 端末のバッファ解放」を daemon 側に実装する。恒久解は「TUI は可視ペインのみ購読し、バッファは daemon に一元化（TUI/daemon の二重保持を避ける）」であり、本 issue の解放ポリシーはそのまま daemon 側へ引き継ぐ。

## 確認方法

- ended ペインを含むセッションを複数作った後の TUI RSS が、解放前より減ること（ペイン数×数 MB オーダー）。
- preview・タブ UI の見た目が維持されること（既存 ui テスト + スナップショット）。
- カバレッジ 100% 維持。
