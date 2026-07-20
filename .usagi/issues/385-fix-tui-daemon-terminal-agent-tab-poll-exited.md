---
number: 385
title: fix(tui/daemon): terminal/Agent 終了時に右ペイン tab を確実に削除する（poll の exited 信号を配線）
status: done
priority: high
labels: [tui, daemon, bug]
dependson: []
related: []
created_at: 2026-07-20T01:23:07.121819+00:00
updated_at: 2026-07-20T01:32:57.856728+00:00
---

## 背景 / 不具合

v2 TUI で **Agent または Terminal を閉じた（プロセスが終了した）際、右ペイン（Closeup）の対応する tab が削除されず残り続ける**。Agent / Terminal の双方で再現する。

## 確認された根本原因

右ペイン tab は、`TerminalSession` が `SessionState::Exited` に遷移すると、毎フレームの sweep（`presentation/mod.rs` の `close_exited_panes` → `poll_all_terminals` → `WorkspaceRuntime::exit_pane` → `PaneEvent::Exited`）が reducer に配送して初めて削除される。reducer / runtime / render（`pane.rs` / `pane_runtime.rs` / `workspace_runtime.rs` / `views/workspace.rs`）は **完全な `TerminalRef` の stable identity で対象 tab だけを正しく削除**しており、ここは不具合ではない。問題は **`Exited` 信号が本番の poll 経路で一切発火しない**ことにある。

- **TUI transport（本番デコード）**: 合成ルートの `DaemonAgentCommandPort::poll_terminal`（`src/runtime/tui.rs`）は daemon の `Resume` 応答から `body["output"]` だけをデコードし、`exited` フラグを捨てている。したがって毎フレームの `poll` は `Err(TerminalError::Exited)` を返さず、`TerminalSession` は永遠に `Live` のまま。終了は偶発的な resync/attach（`attach_terminal` は `exited` をデコードする）でしか捕捉されず、通常は発火しないため tab が残る。
- **daemon の Agent Resume パリティ欠落（Agent 特有）**: generic terminal の `Resume` は `{"output","exited"}` を返す（`usecase/terminal_ipc.rs`）が、Agent の `Resume` は `{"output"}` のみを返す（`usecase/agent_ipc.rs`）。Agent 終了は `Resync` snapshot にしか現れないため、上記 transport を直しても Agent はなお poll で終了を観測できない。

補足: 本番ループ `drive_workspace_controller` は `WorkspaceUi`（TerminalSession poll）+ `WorkspaceRuntime`（PaneRegistry）で構成され、`RuntimePhase` / `DaemonPushAdapter` / `DaemonBackend` / `AgentRuntimeHost` は本番未配線。よって Agent 終了も Terminal 終了も **同一の terminal-poll 経路**に依存しており、この 1 経路の欠落が両者の不具合になっている。既存の contract テスト（`poll_reporting_exit_transitions_to_exited` / `an_exited_terminal_auto_closes_its_pane_and_detaches_through_the_runtime`）は fake port が直接 `Err(Exited)` を返すため通ってしまい、本番のデコード欠落を検出できていなかった。

## 変更内容

1. **daemon**: Agent の `Resume` を `{"output","exited"}` に拡張し generic terminal とパリティを取る（`terminal_snapshot(runtime).exited.is_some()`）。`usecase/agent_ipc.rs`。
2. **TUI transport（合成ルート）**: `src/runtime/tui.rs` の Resume 応答デコードを純粋関数に抽出し、`exited` が真かつ未消費 output が無くなった時点で `Err(TerminalError::Exited)` を返す。`TerminalSession::poll` は既存の `Err(Exited) → SessionState::Exited` 契約でそのまま tab を落とす（reducer/runtime/render は無変更）。

## 完了条件

- Agent / Terminal のいずれも、プロセス終了後の次の poll で対応する live tab が右ペインから削除され、Closeup へ戻る（最後の tab の場合）。
- 複数 tab・pending launch・session 切替・同名再作成・late close/completion・daemon reconnect・live input ownership を回帰させない（reducer は stable identity で対象 tab のみ削除するため保証される）。
- coverage 100%（純粋デコード関数は root package のユニットテストで担保。IO 薄ラッパは `#[coverage(off)]`）。

## テスト

- daemon: Agent `Resume` 応答が実行中は `exited=false`、runtime 終了後は `exited=true` を含む。
- root: Resume 応答デコードの純粋関数（実行中→chunks / 終了+drained→`Err(Exited)` / 終了+最終 output→chunks 後、次 poll で `Err(Exited)`）。
- TUI（既存で契約を固定）: `poll_reporting_exit_transitions_to_exited`、`an_exited_terminal_auto_closes_its_pane_and_detaches_through_the_runtime`。

## 参照

- reducer 削除: `crates/tui/src/usecase/application/pane.rs`（`exit` / `close_selected`）
- sweep: `crates/tui/src/presentation/mod.rs`（`close_exited_panes` / `poll_all_terminals`）
- 本番デコード欠落: `src/runtime/tui.rs`（`poll_terminal`）
- daemon Resume: `crates/daemon/src/usecase/agent_ipc.rs` / `crates/daemon/src/usecase/terminal_ipc.rs`
