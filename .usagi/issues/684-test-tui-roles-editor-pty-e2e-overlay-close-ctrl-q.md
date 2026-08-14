---
number: 684
title: test(tui): roles editor の PTY E2E が overlay の close を待たずに Ctrl-Q を送る
status: done
priority: high
labels: [v2, tui, test, ci]
dependson: []
related: [682, 567]
created_at: 2026-08-14T00:26:30.264205+00:00
updated_at: 2026-08-14T00:33:43.046939+00:00
---

## 問題・影響

`tests/cli_tui_pty.rs::real_pty_roles_editor_ctrl_s_persists_workspace_catalog` が負荷下で落ちる。
`cli_tui_pty` は `full-test` / `coverage` の両 gate で走るため、無関係な変更の PR が落ちる。

```text
panicked at tests/cli_tui_pty.rs:768:13:
timed out waiting for Leave this workspace?; feedback=[]; screen=" USAGI > roles-editor-workspace ...
                 ┌─ Roles ──────────────────────────────────────────────┐
                 │   workspace roles.toml · versioned TOML              │
                 │   Ctrl-S: validate + save   Tab: scope   Esc: close  │
                 ...
[switch] ↑↓ select / Enter closeup   [Switch] preview pane"
```

本セッションで 2 回再現した（`cargo test --workspace --quiet` 1 回、重い E2E 2 binary の同時実行 1 回）。
単独実行では green。

## 原因

送ったキーの効果を**観測せずに**次のキーを送っている。

```rust
send(&mut master, b"\x1b");                       // Roles editor を閉じる
assert!(quit_from_switch(...).success());         // 中で wait_for_screen_since("[switch]") → Ctrl-Q
```

`quit_from_switch` の最初の待ちは `[switch]` を待つが、**`[switch]` は Roles overlay が開いたままでも
status bar に描かれている**（上の失敗時 screen がまさにそれ）。したがってこの待ちは即座に満たされ、
Ctrl-Q は Roles editor に入る。editor がそれを飲むので `Leave this workspace?` は永久に現れず、
30 秒 deadline で落ちる。

CPU 負荷はこの race の窓を広げるだけで、原因ではない。Esc の再描画がキー送信に間に合った run は
たまたま通っていた。これは
[背景 worker を残したままテストを終えない](../../document/06-conventions.md#背景-worker-を残したままテストを終えない)
が禁じている「タイミングで決まる事象を観測せずに pass と読み替える」形であり、
[#567](567-test-tui-restore-retry-skipped-tick-frame-skip-flake.md) と同じ類である。

## 対象責務

Esc の後、**Roles overlay が実際に閉じたことを観測してから** Ctrl-Q を送る。
既存の `wait_for_screen_absent_since` がこの用途の helper で、`submit_closeup_command` は同じ形を取っている。

## 非対象

- deadline の延長。窓を広げるだけで race は残る。
- `quit_from_switch` の `[switch]` 待ちそのもの。overlay を持たない呼び出し側では正しい。
- `real_pty_cold_restart_resumes_only_the_selected_interrupted_tab_from_real_keys` /
  `real_pty_root_launch_keeps_the_managed_agent_tab_live` の負荷下失敗。こちらは同じ観測欠落が
  見つかっておらず、CPU 競合側（[#682](682-test-e2e.md)）として扱う。

## 受入条件

- [ ] Esc 後に overlay の close を観測してから Ctrl-Q を送る。
- [ ] `cargo test -p usagi --test cli_tui_pty` が単独でも、重い E2E を同時に走らせた状態でも green。
- [ ] deadline も固定 sleep も増やしていない。
