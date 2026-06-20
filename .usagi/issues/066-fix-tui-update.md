---
number: 66
title: fix(tui): 統括/在席モードで背景状態(バッジ・update通知)の反映が次のキー入力まで遅延する
status: todo
priority: medium
labels: [fix, tui, review]
dependson: []
related: []
created_at: 2026-06-20T12:04:14.473558+00:00
updated_at: 2026-06-20T12:04:14.473558+00:00
---

## 背景

コードレビューで判明した UI フィードバックの欠落。統括(Overview)/在席(Focus) モードでタスク非実行・無入力のとき、背景スレッドが更新する状態が画面に反映されない。

`src/presentation/tui/home/event/mod.rs:223-240` のループ末尾は、`animate` のときだけ `read_key_timeout(ANIM_TICK)` でポーリング起床し、それ以外は `reader.read_key()` で**無期限ブロック**する。`animate` の判定対象は install 進行中とセッション create/remove タスクのみ。

一方、次の 2 つはループ先頭でしか反映されない:

- **update 通知**（`update.status()`）
- **セッションのバッジ**（`monitor.snapshot()` の running/waiting/done/live。ウォッチャーは 200ms 間隔で更新）

→ タスクが走っていない待機中にキー入力が無いと、背景エージェントが「待機中(◆)」「完了(✓)」になっても、サイドバーのバッジや update 通知が**次のキー入力まで一切更新されない**。「いつエージェントが入力待ちになったか」が TUI 上で分からず、デスクトップ通知頼みになる。没入(Attached)は `terminal_pane::drive` が別途バッジ監視するため影響を受けないが、統括/在席に穴がある。

## 改善方針

- `animate` の条件に「live なセッションが存在する（バッジが動きうる）」を加え、200ms 程度で起床して再描画・通知反映する。
  - 例: `let animate = install... || tasks.is_active(now) || state.has_live_sessions();`
- 起床コストが気になる場合は、バッジ未変化フレームの再描画を差分判定でスキップする。

## 確認方法

- 統括/在席で無入力のまま、エージェントが待機/完了に遷移したらバッジが ~200ms 以内に更新されること。update 通知も同様。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
