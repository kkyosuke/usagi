---
number: 388
title: feat(tui): workspace open 時に scope inventory を pane tab へ投影して live Agent/Terminal を復元する
status: done
priority: high
labels: [tui, terminal, agent, restore, pane]
dependson: [386]
related: [193, 195, 256, 282, 367]
parent: 390
created_at: 2026-07-20T01:42:46.446935+00:00
updated_at: 2026-07-20T03:32:04.124170+00:00
---

## 目的

workspace open 時に、daemon の scope inventory（#386）を pane tab へ投影し、同一 daemon が生存所有する live Agent / Terminal を stable identity で復元する。session pane（`Target::Session`）と root pane（`Target::Root`）の両方を対象にする。

親: #390。依存: #386。

## 現状の問題

- open flow（`crates/tui/src/presentation/mod.rs::drive_workspace_controller`）は空の `PaneRegistry` / 空の `WorkspaceUi.terminals` から始まり、起動時に `port.inventory()` を呼ばない。
- reducer の `PaneEvent::Restore` に production caller が無く、`PaneRuntime::reconnect` / agent host は shell に配線されていない。
- `PaneRuntime::reconnect`（`crates/tui/src/usecase/application/pane_runtime.rs`）は**既にメモリ上にある tab を検証するだけで、inventory item から tab を新規生成しない**。

## 変更内容

- **restore reducer の拡張**: `PaneEvent::Restore` / `PaneState::with_live`（`crates/tui/src/usecase/application/pane.rs`）を、inventory item から `PaneTab::Live` を **seed（新規生成）** できるように拡張する。Agent item は Agent tab、Terminal item は Terminal tab として、target（`session_id?` から `target_for_terminal`）ごとに `PaneRegistry` へ投影する。
- **open 時 projection の配線**: #193 の非同期 launch-job パターンに従い、**初回 frame を paint した後**に、workspace root scope と各 available session scope について `inventory{scope}` を off-thread で取得し、結果を restore event として reducer に流す。UI thread を daemon handshake で直列ブロックしない。
- **attach policy**: 投影した live tab のうち、選択（foreground）tab だけを `PaneRuntime::reconnect` 相当で attach / resync し、他は live-but-detached にする（既存の per-target visible projection 契約 #282 を維持）。attach は必ず `TerminalRef` で fenced に行い、名前 / path から terminal を推測しない。
- **stable identity / 二重 tab 防止**: `TerminalRef::fences` で dedup。再 open・duplicate snapshot・複数回の inventory 応答があっても同一 runtime に対して tab を 1 枚だけにする。
- **安全な縮退**:
  - non-live / 死んだ process の item → tab を作らない。
  - `session_id` が現 lifecycle snapshot（`FsWorkspaceLoader` の session projection）に無い stale / recreated session → skip（scope mismatch）。
  - 別 workspace / 別 worktree / daemon generation 不一致 → stale として skip・attach しない。
  - PTY master 復元不能（orphan / identity_unknown）→ 既存契約（[03-tui.md](../../document/03-tui.md) resume data compatibility / orphan 表示、#350 interrupted）に従い live tab を作らず・再 spawn せず・input を無効化する。
- daemon 不通 / transport failure → safe feedback を表示し local PTY を生成しない。restore は best-effort で、失敗 scope 以外の復元と手動操作を継続できる（#193 の partial-failure 契約）。

## 完了条件

- 同一 daemon 生存中に、live Agent / Terminal を持つ workspace を開き直すと、root / session 各 scope の live runtime が stable identity で tab に復元され、選択 tab が attach されて双方向 IO が再開する。
- 再 open / duplicate snapshot / 複数回 inventory でも二重 tab が発生しない。
- stale session・scope mismatch・generation 不一致・orphan / identity_unknown・非 live item で、誤って live tab を作らない・local spawn しない。
- 初回 frame paint とキー入力が inventory 取得でブロックされない（#193 first-paint 契約）。
- session pane の既存投影・live 判定・IO の回帰テストが green。
- coverage 100%。

## テスト方針

- **event-loop / reconnect regression**: fake inventory port（root + 複数 session の混在 runtime、dead、stale-session、scope-mismatch、generation-mismatch、duplicate-snapshot、orphan / identity_unknown）で restore projection・dedup・safe 縮退を検証する（`pane_runtime.rs` / `parity_suite.rs` の既存 fake `TerminalPort` と `resume_compatibility_fixture...` を拡張）。
- **first-paint 順序**: inventory を off-thread 化し、初回 frame とキー入力が待たされないことを固定する（`presentation/mod.rs` の frame-loop test）。
- **no-duplicate-tab**: 同一 `TerminalRef` の重複 inventory / 再 open で tab が 1 枚に収束することを固定する。
- **runtime regression**: 投影後の attach / resync / input / resize / detach / exit が既存 stream 契約どおり動く。

## 依存

#386（unified scope inventory）。
