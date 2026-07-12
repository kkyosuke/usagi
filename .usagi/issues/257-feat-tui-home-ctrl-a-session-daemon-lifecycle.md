---
number: 257
title: feat(tui): Home Ctrl+A 新規 session 作成を daemon lifecycle へ接続する
status: todo
priority: high
labels: [tui, session, lifecycle, parity]
dependson: [217, 219, 220, 225, 231, 234, 250, 251, 252, 253, 254]
related: [222, 224, 246]
parent: 227
created_at: 2026-07-12T23:28:40.613981+00:00
updated_at: 2026-07-12T23:28:40.613981+00:00
---

## 背景・再現

v2 Home 左ペインは #225 で root → session → + new session、selected / active の独立描画まで実装済みである。しかし現行 controller の Selection::NewSession は Enter で名前を持たない CreateSession { token } を発行するだけで、実ランタイムの Ctrl+A 分類、名前・Agent profile / model の入力、daemon operation の accepted/progress/final 接続、成功時の safe landing まで到達しない。

再現:

1. v2 TUI で Home の Switch を開く。
2. Ctrl+A を押す（crossterm では Home として到着する端末がある）。
3. v1 と異なり名前入力は開かず、daemon-authoritative な create operation も送られない。

v1 の実挙動は v1/src/presentation/tui/home/event/tests/overview_mode.rs と v1/document/design/home/03-sidebar.md を正とする。基底 Overview の Ctrl+A（Home に decode）は IME-safe な c alias として空の inline name input を開く。入力中の Home は create を再発火せずキャレットを先頭へ移動する。Enter は validation 済みの name で非同期 create を開始し、作成中は + new session の直前に skeleton、成功時はユーザー操作が無ければ新 session を selected / active にして Closeup へ landing し、失敗時は入力意図を失わず安全なエラーを表示する。

## 目的

v1 Ctrl+A 新規作成 UX を v2 Home の唯一の input → controller → daemon SessionLifecycle 経路へ接続する。既存の #217/#219/#220 の daemon-authoritative operation、#225 の row projection、#231/#234 の pending/reconcile reducer を再実装せず、現行 Home controller の token-only create stub を置換する最小の統合 slice とする。

## スコープ

- 非 live Home/Switch の terminal event を management input へ正規化し、Ctrl+A の control-byte / Home 表現を同じ「新規 session 入力を開く」意図へ分類する。live PTY 中の入力を奪わず、既存 LiveInputClassifier の Ctrl+O prefix / passthrough 契約を保つ。
- + new session 選択時の Enter と Ctrl+A で、名前入力フォームを開く。フォームは文字編集、Home/End、Escape cancel、Enter submit を持ち、入力中の Home は必ず caret movement とする。v1 と同じ IME-safe alias と、+ new 行での printable-character start を実装するかは、現行 v2 input vocabulary へ無理なく同居できる範囲で含める。
- name（trim、空、同名/不正の safe validation）と任意の Agent profile / model を入力する UX を定義し、profile ID / ModelSelector の product-neutral validation を用いる。CLI 名や adapter 固有 flag / model allowlist を TUI に持ち込まない。未選択時は workspace/default policy に委ねる。
- submit ごとに producer-issued OperationId を一度だけ生成し、name と optional launch selection を含む typed create intent を #234 の SessionLifecycleAdapter / daemon IPC client に渡す。TUI から store、git worktree、PTY、local fallback を直接 mutation / spawn しない。
- #231/#234 の OperationId、accepted/progress/final、snapshot/replay/reconnect、stale/duplicate rejection、pending skeleton、failure rollback を実際の Home projection/render に接続する。旧 PendingToken + OperationResult + success refresh-only create path をこの経路で置換または到達不能にする。
- create 成功時は accepted 時点から interaction counter が不変なら新 session を selected / active にし Closeup へ landing する。入力・移動・overlay 操作等があれば利用者の現在地を保つ。snapshot 消失、same-name recreation、late final/replay で selected / active を不正な identity にしない。

## 対象外

- SessionLifecycle reducer / durable journal / create worker / setup / crash recovery / IPC wire 自体の再設計（#217/#219/#220）。
- #225 の root/session/+new 行や selected/active marker の再実装。
- #231/#234 の generic pending/reconcile policy の再実装。
- Claude/Codex の argv、hook、provision、CLI 固有モデル allowlist、実 PTY spawn（#250〜#254 の境界）。
- remove UX、unite/multi-workspace、マウス操作、全 v1 sidebar 視覚 parity の拡張。ただし本 issue の state は将来の複数 workspace に name/index を identity として漏らさない。
- 実装済み仕様 document への将来形記載。本 PR は実装 issue のみを起票する。

## 依存・重複境界

| Issue | 本 issue との境界 |
|---|---|
| #217 | durable SessionLifecycle / operation / fencing を消費する。変更しない。 |
| #219 | daemon create/control authority を呼ぶ。TUI は local fallback しない。 |
| #220 | IPC client cutover を利用する。別の client surface を増やさない。 |
| #225 | + new selection と selected/active rendering の土台。フォーム・operation 接続を追加する。 |
| #231 | pending/safe landing の pure policyを再利用し、controller の旧 token path を二重化しない。 |
| #234 | daemon accepted/progress/final/reconcile adapter を Home runtime に結線する。 |
| #250〜#254 | profile/model は product-neutral launch contract を渡すだけで、adapter 実装には踏み込まない。 |

## 受け入れ条件

- Home Switch で Ctrl+A の control-byte と Home 表現はいずれも新規 session 入力を開き、live pane 中は PTY passthrough のままである。
- name 入力中の Home は create を再発火せず caret を先頭へ動かす。Escape は request を送らずに戻り、無効 name / profile / model は安全な inline feedback を残して submit しない。
- 有効 submit は name と optional profile/model を持つ 1 個の durable OperationId の create intent を daemon に送る。同じ submit、transport failure、reconnect で新しい operation ID や local fallback を発生させない。
- accepted/progress/failure/success は該当 OperationId にだけ反映され、create skeleton / safe feedback / rollback が #231/#234 の policy と一致する。
- success landing は interaction counter が不変な場合だけ新しい stable SessionId を selected / active にして Closeup へ移る。操作済み、stale/duplicate/late final、snapshot replacement、same-name recreation では現在の安全な selected / active を保つか root へ縮退する。
- Agent profile/model は TUI の表示・request 値として product-neutral に扱い、CLI flag、secret、raw adapter/protocol detail を state、render、error に置かない。
- 実装後、旧 controller の name-less CreateSession { token } / OperationResult success-refresh path が Home new-session 操作から使われない。

## テスト

- pure reducer / classifier: Ctrl+A control-byte と Home の同値性、live passthrough、入力中 Home caret、Esc、printable start、trim/empty/invalid name、profile/model validation。submit idempotency、OperationId と intent の保持、accepted/progress/final/failure、duplicate/stale/reverse order、safe error redaction。interaction counter による landing / no-landing、snapshot 消失、same-name recreation、selected/active root fallback。
- pure render: + new: <draft>、inline validation、accepted skeleton、progress/failure、selected/active marker が stable SessionId と action-row identity を混同しないこと。
- IPC / PTY integration: fake socket daemon で typed create request（name + optional profile/model）→ accepted/progress/final → reconnect replay を通し、operation を再送・二重作成しないこと。real PTY regression で Ctrl+A が management Home では form を開き、live daemon-owned terminal では bytes が一度だけ PTY へ届くこと。
