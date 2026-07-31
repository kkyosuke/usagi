---
number: 596
title: fix(tui): 再 attach 時に input_seq が daemon ledger とずれ、terminal 入力が恒久的に失敗する
status: todo
priority: high
labels: [tui, daemon, bug, input, terminal, agent]
dependson: []
related: [576, 577, 578, 581]
created_at: 2026-07-31T11:04:51.172800+00:00
updated_at: 2026-07-31T11:04:51.172800+00:00
---

## 症状

chat drawer（指示モード）で次の操作をすると、以降その conversation へのキー入力がすべて失敗する。

1. drawer を開く（`Ctrl-O Ctrl-G`）
2. `Ctrl-O n` → `Enter` で chat を起動し、**何か入力する**
3. drawer を閉じる（`Esc` / `Ctrl-O Ctrl-G`）
4. drawer を開き直して入力する → 入力が PTY に届かず `daemon unavailable; reconnecting` 相当の feedback が出続ける

同じ欠陥は drawer 固有ではない。foreground terminal の detach → 再 attach を通る経路（managed pane の tab / session 切り替えで一度 detach された live terminal へ戻る、など）はすべて同じ状態になる。drawer の open/close は 1 操作でこの経路を必ず踏むため、最短の再現手順になっているだけである。

## 原因

client の `input_seq` は「connection epoch ごとに 0 から始まる ordering 番号」で、daemon 側の ledger は
`Entry.inputs: BTreeMap<(ConnectionId, ClientId), InputLedger>`（`crates/daemon/src/usecase/terminal.rs`）に持つ。

- daemon の ledger を捨てるのは **connection の切断**（`TerminalRegistry::disconnect`）だけである。`detach` は
  `attachments` から subscription を外すだけで `inputs` を残す（意図どおり: sequence は connection epoch に属する）。
- ところが client 側は detach のときに `TerminalSession` を**丸ごと破棄**する。
  `WorkspaceUi::sync_foreground_terminal` → `close_terminal` が `self.terminals` から `retain` で除去し、
  再 attach 時は `start_terminal_session` が `TerminalSession::new`（`input_seq: 0`, `connection_epoch: None`）を
  作り直す。
- `TerminalSession::commit` が `input_seq` を 0 に戻す条件は「connection epoch が変わったとき」である。新規
  session は `connection_epoch: None` なので epoch が同じでも 0 から始めてしまう。

結果として、同一 connection 上で client は `input_seq = 0` を再送し、daemon は `ledger.next_seq = N`（N > 0）と
比較して `input_seq < next_seq` の分岐に入る。cache は同じ seq の**別 RequestId** しか持たないため
`RegistryError::IdempotencyExpired` を返す。client 側の `map_terminal_error`（`src/runtime/tui.rs`）は
このコードを catch-all で `TerminalError::Unavailable` に落とすため、ユーザーには「daemon 不通」に見える。
失敗後は `Reconnecting` → 同一 epoch で `connect_at` を再試行するだけなので `input_seq` は 0 のまま、
**transport epoch が変わるまで永久に入力が通らない**。

同時に、破棄された `TerminalSession` は `unresolved_input` と `fenced_queue`（#519 / #523 の input 効果不明 fence と
その後ろに積んだ入力）も一緒に失う。「acknowledgement を失った入力を勝手に解決済みにしない」という契約が
detach 経路で黙って破れている。

## 変更方針

**client は detach を跨いで ledger 位置を失わない**ことを不変条件にする。次のどちらかを選ぶ（前者を推奨）。

1. **detach しても `TerminalSession` を保持する**（推奨）。`sync_foreground_terminal` は subscription だけを
   release し、session 本体は detached state で保持する。再 attach は既存 session の `connect` を呼ぶだけになり、
   `input_seq` / `connection_epoch` / `unresolved_input` / `fenced_queue` / 復号済み screen が連続する。
   - 無制限に保持しないこと。保持数は bounded（LRU など）にし、溢れたものは今と同じく破棄する。破棄した
     terminal へ後で戻ったときに沈黙して壊れないよう、2 の adopt 経路も必要になる点に注意する。
   - connection epoch が変わったら従来どおり `input_seq` を 0 に戻す（daemon 側も `disconnect` で ledger を捨てる）。
2. **attach 応答が daemon 側の次 sequence を返す**。`TerminalAttach`（および `TerminalAction::Attach` の wire frame）に
   `(connection, client, terminal)` の `next_input_seq` を載せ、`commit` が epoch 一致時もそれを採用する。
   wire 変更なので `TERMINAL_WIRE_GENERATION` の扱いと後方互換（フィールド欠落時は現行の epoch 判定へ fallback）を
   同じ変更で決める。

いずれの経路でも、`ErrorCode::IdempotencyExpired` / `SequenceGap` を `TerminalError::Unavailable` に丸めるのを
やめる。これは daemon 不通ではなく client 側の ordering 不整合なので、専用の safe message（および可能なら
resync による自動復旧）へ写す。

## 対象ファイル

- `crates/tui/src/presentation/mod.rs`（`sync_foreground_terminal` / `start_terminal_session` / `close_terminal`）
- `crates/tui/src/usecase/application/terminal_session.rs`（`commit` の epoch 判定、必要なら ledger の adopt API）
- `crates/daemon/src/usecase/terminal.rs`（2 を選ぶ場合の `next_seq` 公開）
- `crates/daemon/src/usecase/terminal_ipc.rs` / `crates/core/src/infrastructure/ipc/mod.rs`（2 を選ぶ場合の wire）
- `src/runtime/tui.rs`（`map_terminal_error` の分類）
- `document/03-tui.md`（drawer open/close の attach 契約に「input ordering は detach を跨いで連続する」を明記）

## 受け入れ条件

- 同一 connection 上で detach → 再 attach した terminal へ入力しても daemon が `IdempotencyExpired` /
  `SequenceGap` を返さず、bytes が PTY に届く。
- connection epoch が変わった再 attach では従来どおり `input_seq` が 0 から始まる（daemon 側 ledger も破棄済み）。
- detach → 再 attach で `unresolved_input` と fenced queue が失われない（効果不明の入力が勝手に解決済みにならない）。
- `IdempotencyExpired` / `SequenceGap` がユーザーに「daemon unavailable」として出ない。
- drawer の open → 起動 → 入力 → close → open → 入力 が成功する回帰テストがある。

## テスト方針

- `cargo test -p usagi-tui usecase::application::terminal_session`（同一 epoch 再 attach で sequence が連続する unit test。
  既存の `same_connection_cursor_gap_reattach_preserves_the_next_input_sequence` / `fresh_connection_epoch_resets_the_input_sequence` に隣接して追加）
- `cargo test -p usagi-tui presentation`（`sync_foreground_terminal` の detach → 再 attach で ledger が連続する shell seam test）
- `cargo test -p usagi-daemon usecase::terminal`（`detach` が ledger を残し `disconnect` が捨てる契約の明示テスト）
- `cargo test -p usagi --test agent_ipc_e2e`（drawer close → open → 入力の実 daemon 経路。既存の daemon 起動 lock に載せる）

## 非目標

- drawer の描画・component 共有（別 issue）。
- drawer の scroll / selection 保持（別 issue。本 issue の保持方針に依存する）。
- 入力 ACK / 効果不明 fence の設計そのものの変更。
