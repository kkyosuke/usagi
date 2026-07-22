---
number: 517
title: fix(tui): terminal input ACK outcome を正しく投影する
status: done
priority: high
labels: [review, v2, tui, terminal, ipc, safety]
dependson: []
related: [215, 216, 463, 475]
created_at: 2026-07-22T11:34:44.935701+00:00
updated_at: 2026-07-22T12:42:07.825366+00:00
---

## 問題・影響

shipping v2 の `src/runtime/tui.rs::DaemonAgentCommandPort::input_terminal` は daemon の terminal Input 応答 body を捨て、transport 上の `Ok` を常に入力成功として返す。daemon は `InputAck::{Written, Failed, Ambiguous { applied_prefix }, Cached(..)}` を返し、いずれの valid ACK でも input sequence ledger を進めるため、0-byte failure と partial write を成功表示し、client/daemon の sequence も不整合になる。

response を受信できない transport failure も side effect の不成立を意味しない。現行 UI はこれを「keystroke not delivered」と断定するため、ACK loss 後に実際は適用済みの入力を利用者が再入力して二重 command にし得る。

## 既存 issue との境界

- #463 は `TerminalSession` の reconnect/backoff と非 Live/transport failure の表示を実装済みだが、production adapter の ACK body decode を扱わない。本件はその shipping regression。
- #475 は daemon 実 PTY の `applied_prefix` 計測を修正済みで、本件はその outcome を TUI まで失わず投影する consumer 側。
- #215/#216 の correlation・timeout・response-loss/idempotency 契約は再設計しない。connection を越える完全な ACK query/replay は別 issue に分離する。

## 対象責務

- production terminal adapter で ACK body を厳格に decode し、nested `Cached` を最終 outcome へ正規化する。
- `Written`（および cached Written）だけを通常成功とする。
- `Failed` は effect-zero として「適用されなかった」と表示し、`Ambiguous` は `applied_prefix` を保持して「prefix が適用された可能性があり全体 outcome は不確定」と表示する。いずれも自動 retry しない。
- valid ACK は success/failure/ambiguous の別なく daemon が消費した input sequence と client sequence を同じく進め、次入力で gap/reuse を起こさない。ACK outcome 自体では live subscription を破棄しない。
- response を受信できない transport failure は「未配送」と断定せず、effect unknown の safe feedback と reconnect に遷移する。今回の client は blind resend しない。
- malformed/unknown ACK は成功へ縮退せず protocol/availability failure とする。

## 受入条件

- [ ] Written / Cached(Written) だけが通常成功になる。
- [ ] Failed は effect-zero、Ambiguous は applied prefix を含む不確定 outcome として safe UI feedback へ届き、成功誤報・blind retry がない。
- [ ] Failed / Ambiguous / Cached の valid ACK 後も次 input sequence が daemon と一致する。
- [ ] ACK-loss/EOF/timeout は「keystroke not delivered」と断定せず effect unknown と表示し、同じ bytes を自動再送しない。
- [ ] malformed/unknown/deeply nested Cached は bounded に fail closed し、panic・成功扱いにならない。
- [ ] Agent と generic Terminal の共有経路で同じ挙動になる。

## 必須回帰テスト

- production response body と同じ JSON fixture で Written、Failed、Ambiguous（0 / partial / full-length 境界）、nested Cached、malformed/unknown を deterministic に decode する。
- fake port で valid ACK outcome ごとの sequence advance、Live 維持、次入力、visible feedback を固定する。
- scripted IPC stream/socket で response ACK loss を発生させ、unknown-effect feedback、no automatic replay、reconnect を検証する。
- production `DaemonAgentCommandPort` の decode/adapter contract を root test から直接通し、fake が期待 outcome を先回りして返すだけのテストにしない。

## docs / gate

`document/03-tui.md` と `document/04-ipc.md` の terminal input outcome / ACK-loss 表示契約を実装へ合わせる。root runtime と TUI crate、terminal IPC 境界に影響するため fmt/check/clippy、推奨 selected testsを実行し、full test / coverage 100% は PR CI の必須 gateとする。

## final response / protocol side-effect hardening

- terminal InputAckはfinal `DaemonReply::Ok` bodyだけからdecodeする。`Accepted` はpending/non-finalなので、bodyが `{"ack":"Written"}` でも `InputEffectUnknown` としてconnectionをresetし、sequenceを進めず再送しない。
- `ClientError::Protocol` は `side_effect: None` の場合だけErrorCodeをdefinitive failureへmapする。`PartialOrUnknown` / `Applied` / `OperationAccepted` はcodeにかかわらずeffect unknownとして未配送と断定しない。
- scripted production IPC regressionでAccepted+Written、同一ErrorCodeのNone/PartialOrUnknown/Applied/OperationAcceptedを比較し、Written誤報、sequence advance、blind replayがないことを固定する。

## connection epoch / uncertainty latch hardening

- `TerminalAttach`はclient-local `connection_epoch`を返す。同じepochのcursor-gap/resync/detach→reattachではconsumed input後のnext sequenceを保持し、fresh transport epochだけ0へresetする。subscription IDからepochを推測しない。
- `Unavailable`だけではlast observed epochを消さない。same-socket malformed Resume/attach body後のreattachは同epochでsequenceを継続し、real EOFでadapterがclientをdropした時だけ次attachのepochが進む。
- ACK loss / `Ambiguous` uncertaintyはtransport recoveryや後続`Written`でclearせずlatchする。current stale/orphaned/exited/resize errorを先頭にprior uncertaintyを合成し、どちらも隠さない。
- 複数uncertain inputはfixed-memoryのcount + first/latestへ集約し、古いwarningを上書きしない。現行production UIにはclear actionを置かず、session破棄または#519 durable outcome resolutionだけがclearする。
- fake-clock unknown→fresh reattach、same/fresh epoch sequence、same-socket decode failure、連続uncertainty後fatal error compositionをmandatory regressionに含める。
