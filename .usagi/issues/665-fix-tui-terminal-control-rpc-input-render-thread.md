---
number: 665
title: fix(tui): terminal control RPC を input/render thread から分離する
status: todo
priority: high
labels: [review, v2, tui, terminal, uiux, input, responsiveness, ipc]
dependson: []
related: [519, 521, 523, 551, 553]
parent: 664
created_at: 2026-08-06T20:39:25.157454+00:00
updated_at: 2026-08-06T22:16:15.188518+00:00
---

## Finding（P1 responsiveness / input reflection）

最新 `origin/main`（`4224b7ae2260ed1812a03353d4540626109361f0`）でも、foreground live terminal の control path は Home の input/render thread から同期実行される。

- ordinary key は `forward_live_terminal_input` → `WorkspaceUi::send_terminal_bytes` → `TerminalSession::send_input` → production `input_terminal` へ進み、daemon の final ACKまで戻らない。
- focus change / reconnect は frame先頭の `sync_foreground_terminal` / `TerminalSession::connect` から `Resize` + `Attach`、旧paneのreleaseから `Detach` を同期実行する。
- deadlineは無期限停止を防ぐが、`Input` 750ms、`Attach` 1000ms、connection 1000msであり、その間は次のdraw、scroll、modal、quitを処理できない。既存 unit testもhung inputがほぼ`INPUT_MS`、hung attachがほぼ`SNAPSHOT_MS`を消費することを正としている。

#551 が置いた「frame loopは非ブロッキングdrain → projection → draw → input」という不変条件はinventory / output pollには成立したが、terminal controlではdeadline付き同期IOとして残っている。deadlineはfailure boundでありinteractive frame budgetではない。

## 修正方針

- workspaceごとにresidentなterminal control scheduler/actorを置き、`Input` / `InputOutcome` / `Attach` / `Resync` / `Detach` / `Resize`をworker側のowner-generation laneで実行する。TUI threadはbounded enqueueとcompletion drainだけを行う。
- inputはterminalごとにsingle-flight ordered queueとし、producer `OperationId`、subscription epoch、`input_seq`、effect-unknown fenceの既存契約をそのまま利用する。blind retry、optimistic local echo、success表示の先行は行わない。
- `InputEffectUnknown`後は後続inputを既存のbounded fenceへ保持し、workerがdurable `InputOutcome`を照会して順序どおり解放する。
- queueのhard boundはrequest件数だけでなくpayload byte数にも置く。1文字・key repeat・pasteを同じ1件として数えて巨大pasteを無制限に保持せず、0-byte inputはenqueue/sequence消費を行わない。
- attach/resync completionは完全な `TerminalRef`、owner generation、connection epoch、focus/registration generation、geometryでfenceし、late completionが別paneや新しいgeometryを上書きしない。
- resizeはterminalごとにlatest geometryへcoalesceし、in-flight後に最新1件だけ送る。detach/close/leave-workspaceはqueued workをcancel/fenceし、workerをjoinする。
- queue full / worker failure / timeoutはsafe feedbackへ投影し、受付済みと未受付を区別する。input queueに入らなかったbyteを「送信中」や「成功」と表示しない。
- enqueue/completion drainはいずれも1 frameあたりの件数/time budgetを持つ。key repeatやresize stormでqueueが継続的にreadyでも、draw・scroll・quitを飢餓させない。

## 受入条件

- daemonが`Input`、`Attach`、`Resize`、`Detach`の各requestで停止しても、TUIは次のframe/input/scroll/quitを1 frame + scheduler誤差以内に処理する。
- key order、cross-connection outcome replay、same-epoch `input_seq` continuity、epoch replacementのfresh attachを維持する。
- slow input中の後続keyはbounded queueへ順序どおり入り、final後に一度だけPTYへ届く。上限超過はtyped backpressureで、silent dropしない。
- multi-byte UTF-8、bracketed paste、8 KiB境界直前/直後、0-byte、key repeat burstを扱い、payloadの途中を別operationへ分割・再送しない。
- focus連打、tab close、workspace離脱、resize storm、late/duplicate completionでstale paneを復活・更新しない。
- control schedulerのthread/queue/in-flight数はterminal数・workspace lifecycleに対してhard boundを持つ。
- leave/quit時は新規受付を閉じ、未開始workを安全に破棄し、in-flight deadlineを超えてjoinしない。close後のlate feedbackが別terminalのfooterへ出ない。

## 必須テスト

- barrier付きfake portで各actionをhangさせ、draw/input/scroll/quitの進行とenqueue latencyをassertする。
- input N件の順序、ACK loss→outcome resolution、queue full、connection epoch変更、attach/resize completion reorderを固定する。
- 1万件のkey/resize/completion burstでper-frame drain boundとquitの進行を固定し、巨大pasteとbyte cap境界を検証する。
- 実Unix socket/PTYでdaemon ownerを遅延させ、キー入力中もTUI quitがbounded wall-clockで完了し、PTY write countが0または1であることを確認する。

## 根拠箇所

- `crates/tui/src/presentation/mod.rs`: frame loop、`forward_live_terminal_input`、`WorkspaceUi::{sync_foreground_terminal,send_terminal_bytes,close_terminal}`
- `crates/tui/src/usecase/application/terminal_session.rs`: connect / input / outcome / resize state machine
- `src/runtime/tui.rs`: `DaemonAgentCommandPort` owner lane
- `crates/core/src/usecase/client.rs`: `TerminalLaneBudget`
