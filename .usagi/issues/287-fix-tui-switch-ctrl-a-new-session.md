---
number: 287
title: fix(tui): Switch Ctrl+A から + new session を実行可能にする
status: done
priority: high
labels: [tui, session, bug, parity]
dependson: [258]
related: [257, 260, 268, 269]
parent: 227
created_at: 2026-07-13T11:58:39.023157+00:00
updated_at: 2026-07-20T22:12:50.085526+00:00
---

## 背景

v1 Home の左 sidebar は root と session の後ろに常時 `+ new session` を置き、空の session 一覧でも keyboard target として選択できる。Switch の `Ctrl-A` は、端末によって `Home` として届く IME-safe な作成開始である。入力中の `Home` は作成を再発火せず caret 移動である。

現行 v2 controller は `root → sessions → + new session`、`Selection::NewSession`、Switch の `Ctrl-A`、create form と typed `Effect::CreateSession` を持つ。一方、実端末の `run_workspace` は legacy `WorkspaceView` / `step_switch()` を使うため、Ctrl+A が処理されず、action row も描画・選択・Enter submit の同じ state/effect 経路に接続されていない。#258 の root-first sidebar runtime 統合と競合させず、その完了後に create entry を最小の adapter 接続として実装する。

## 目的

v1 と同じ `+ new session` affordance を v2 の実行中 Switch に復元する。Switch 中の Ctrl+A は action row へフォーカスを移す、または同等に create form を開始し、既存の Enter 確定で daemon-authoritative session create を実行できるようにする。

## スコープ

- #258 が一元化する `root → sessions → + new session` row projection を runtime の唯一の sidebar source として使い、`+ new session` を session 数にかかわらず常時描画する。既存表記 `+ new session` と selected/active の marker 規約を維持し、action row を active target にしない。
- Switch の Ctrl+A の control byte・control modifier・Home decode を management input として同値に扱う。Ctrl+A は `Selection::NewSession` に選択を移すか、同じ create form を直接開く。いずれでも入力 focus と Switch/Closeup state を不正に変更しない。
- action row を選択して Enter（および既存の同値確定操作）を押すと、既存 `CreateSessionForm` の validation と typed `Effect::CreateSession` を通して daemon lifecycle port に 1 回だけ dispatch する。local store/worktree/PTY fallback を追加しない。
- create form 中の Home/Ctrl+A は caret 操作または no-op とし、フォームを再初期化・再 submit しない。Escape の cancel、Closeup の Ctrl+A/action overlay、live pane の Ctrl+A passthrough、Ctrl-O の mode ルールを #269 と既存 `LiveInputClassifier` のまま保つ。
- #257/#260/#268 で定義済みの typed create、pending/safe landing、daemon lifecycle runner を再設計しない。必要な runtime adapter のみを追加する。

## 対象外

- sidebar の row order、viewport、right-pane tab layout の再設計（#258）。
- daemon lifecycle / IPC wire / worktree 作成 worker の変更。
- profile/model UX、mouse、unite、inline renderer の v1 parity 拡張。
- Closeup の shortcut semantics の変更。

## 受け入れ条件

- session が 0 件でも実端末 Switch の左ペインに `+ new session` が末尾で常時表示され、上下移動で選択できる。選択 marker は見えるが active marker は付かない。
- Switch の Ctrl+A は control-byte、Ctrl+キー、Home decode のすべてで create entry を開始する。Closeup と live terminal は従来の input owner を維持する。
- action row の Enter と Ctrl+A から開いた form は同じ既存確定操作で有効名を create effect にし、runtime port を経て daemon create が 1 回だけ実行される。成功・失敗時の既存 pending / feedback / safe landing を壊さない。
- form 入力中の Home/Ctrl+A は新しい form や effect を発生させず、Escape は作成せず戻る。
- 実装は #258 と同じ row/state/render source を再利用し、legacy `step_switch()` に別の sidebar state machine を増やさない。

## テスト

- 描画: empty/non-empty sidebar の `+ new session` 常設、末尾位置、selected/active marker、狭い geometry の runtime frame regression。
- 状態: row navigation で action row を選択できること、Enter→form→validation→single create effect、cancel、pending/failure/success の selection/route 不変条件。
- キー: Ctrl+A の3表現、action-row Enter、form 中 Home/Ctrl+A、Closeup Ctrl+A、live-pane Ctrl+A passthrough、Ctrl-O との組合せを reducer と fake runtime/daemon port で回帰する。
- 実端末 adapter: fake terminal + fake daemon lifecycle port で、入力から create request までを 1 シナリオとして検証する。
