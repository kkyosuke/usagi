---
number: 544
title: test(tui): interrupted tab の明示 resume を実 PTY の shipping TUI で E2E 検証する
status: todo
priority: medium
labels: [test, v2, tui, agent, recovery, e2e]
dependson: [510]
related: [504, 506, 509]
parent: 505
created_at: 2026-07-25T00:45:54.324084+00:00
updated_at: 2026-07-25T00:45:54.324084+00:00
---

[#510](510-feat-tui-interrupted-claude-codex-tab-resume.md) が interrupted Agent tab と tab 単位の明示 resume を実装し、実 daemon process / socket / PTY と Codex fixture を使う product E2E（`tests/agent_ipc_e2e.rs::root_ipc_cold_restart_projects_interrupted_history_and_resumes_one_exact_tab`）で cold restart → distinct interrupted tab → 1 tab の明示 resume を検証した。その E2E は shipping TUI の reducer（`interrupted_tab::project` / `resume_command` / `accept_replacement`）を実際に通すが、**TUI binary を実 PTY で起動して実キー入力から操作する経路は通っていない**。

## 対象責務

`tests/cli_tui_pty.rs` の実 PTY harness と、`tests/agent_ipc_e2e.rs` の実 daemon + Codex fixture harness を組み合わせた 1 本の E2E を追加し、次を process 境界で検証する。

- root と managed session に Agent を起動して #506 の tab intent を保存し、daemon を SIGKILL してから fresh start する（live resource を持つ通常 `daemon stop` を cold failure の代用にしない）。
- 実 PTY 上の TUI を起動し、最初の明示操作より前に **provider resume invocation / replacement spawn が 0** であることを spawn count で確認する。旧 PTY が live 復元されないことも確認する。
- 描画された tab strip に各 history が distinct な tab として現れ、label が closed vocabulary（`Claude (interrupted)` / `Codex (interrupted)` / `Agent (interrupted)`）だけであること、provider-native ID・argv・cwd・transcript が 1 バイトも画面に出ないことを frame から assert する。
- `Ctrl-O` `r` の実キー入力で選択 tab だけを resume し、fixture argv の exact provider session ID、新しい `TerminalRef` / child PID、spawn count 1 増、retained provider conversation marker を確認する。
- mixed provider（Claude + Codex）、同一 scope の複数履歴、double click（実キー 2 連打）、TUI 再起動（reconnect）、failure 後の retry、`Ctrl-O` `x` での tab close と `reopen` を含める。

## 受入条件

- [ ] 実 PTY 上の shipping TUI が cold restart 後の複数 interrupted history を distinct tab として描画し、再 open / duplicate inventory で二重 tab を作らない。
- [ ] `Ctrl-O r` の実キー入力だけが daemon resume request を発火し、fresh start / TUI open / inventory / reconnect では spawn count が不変である。
- [ ] double click / 再入力が daemon operation 1 件・child spawn 1 件・resulting tab 1 枚へ収束する。
- [ ] root と managed session の resume が同じ UX / fencing で動く。
- [ ] provider ID・argv・cwd・transcript・raw daemon error が描画 frame と log に出ない。
- [ ] Codex success case は #504 の production structured capture を通し、capture 無しの case は unavailable のまま表示する（`--last` や新規空会話へ downgrade しない）。

## 備考

harness の重さから、実行対象は `tui-e2e.yml` 相当の条件付き workflow に載せるか、`agent_ipc_e2e.rs` と同じ serial lock 下に置くかを実装時に決める。
