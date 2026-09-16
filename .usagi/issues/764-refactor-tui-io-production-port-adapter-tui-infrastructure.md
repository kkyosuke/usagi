---
number: 764
title: refactor(tui): 実 IO を持たない Production*Port adapter を合成ルートから tui の infrastructure 層へ移す
status: done
priority: medium
labels: [v2, refactor, tui, root]
dependson: []
related: [757, 759]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T14:59:27.274576+00:00
---

## 問題

`crates/tui/src/infrastructure/mod.rs` は module doc 5 行だけで中身が無い。一方 `src/runtime/tui.rs`（9,899 行、
production 約 5,238 行）には `Production*Port` / `Daemon*Port` / `Fs*` / `Platform*` の adapter 型が 31、trait impl が 26
あり、daemon reply の decode・operation の correlate・live 入力の分類といった**実 IO を伴わない純粋な変換**まで
合成ルートに置かれている。そのため tui crate 単体では「port の production 実装」をテストできない。

## 方針（着手時に調査して補正した）

当初は「daemon IPC client 上の adapter と file store をまとめて tui へ移し、`src/runtime/tui.rs` を 2,000 行以下にする」
と書いたが、実装を読んだ結果この範囲は誤りだった。`DaemonAgentCommandPort`（1,010 行）などの adapter は
`LaneClient` / `TerminalPollPump` / `TerminalInventoryPump` を**自分で所有**し、`policy_client` で接続を張って thread を
起こす。これを tui crate へ移すと thread と接続の所有が tui へ移り、
[06-conventions.md の「本物の IO は合成ルートで束ねる」](../../document/06-conventions.md#品質チェックリスク比例の-gate)
に反する。`FsWorkspaceLoader`（`std::fs`）、`Platform*`（`Command::new`）、`CrosstermTerminal` も同じ理由で合成ルートに残す。

そこで範囲を「実 IO を持たない変換層」に補正した。

- daemon reply の decode / request の組み立て / operation の correlate / typed failure の射影を
  `crates/tui/src/infrastructure/daemon_reply.rs` へ移す。
- 実端末 backend が出した `LiveInput` を `Key` へ分類する adapter を
  `crates/tui/src/infrastructure/live_input.rs` へ移す。
- 接続・lane・thread・端末 backend の所有は `src/runtime/tui.rs` に残す。
- `tests/architecture.rs` で「tui infrastructure は crossterm / portable-pty / libc / signal-hook / fs2 / dirs と
  `std::{fs,process,thread}` を持たない」を guard する。

`RefreshCadence` を持つ `src/runtime/refresh_pump.rs` に依存する cadence 判定も、pump の所有が合成ルートにあるため
今回は移さなかった。

## 完了条件（補正後）

- `crates/tui/src/infrastructure/` に変換 adapter が置かれ、architecture test が実 IO の混入を禁じる。
- `src/runtime/tui.rs` から純粋な変換が無くなる（9,899 → 9,223 行、production 5,238 → 約 4,540 行）。

## 残件

接続・thread を所有する adapter（`DaemonAgentCommandPort` ほか）を tui へ移すには、lane と pump を port として
注入する設計変更が必要で、`src/runtime/{terminal_pump,inventory_pump,refresh_pump}.rs` まで波及する。当初の
「2,000 行以下」はこの設計変更を前提にした数字であり、本 issue では扱わない。必要になった時点で別 issue を起こす。
