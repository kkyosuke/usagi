---
number: 409
title: fix(tui): session 行の stable identity ダブルクリックで Closeup へ切り替える
status: in-progress
priority: high
labels: [tui, mouse, controller, regression]
dependson: []
related: [395]
created_at: 2026-07-20T05:19:32.173769+00:00
updated_at: 2026-07-20T05:25:41.225002+00:00
---

## 目的

controller-driven Home の sidebar pointer 経路で、session 行のダブルクリックを Enter と同じ session activate / Closeup 遷移として復旧する。単クリックは選択だけに留め、時間窓内に同じ stable `SessionId` を 2 回押した場合だけ activate する。

## 背景・原因

現行の `crates/tui/src/presentation/mod.rs` は `sidebar_pointer_event` で直前の `(column, row, Instant)` を shell-local に保持し、同じセルを 400ms 内に押すと `PointerAction::Activate` を生成する。一方 `controller.rs` の reducer は Activate を受けた時点でそのセルに見える row を再解決し、`Root` / session / `+ new session` を区別せず Enter 相当にする。

座標は stable identity ではないため、sidebar scroll や backend snapshot による row 差し替え後、同じセルに別 session が来ても誤って activate できる。また Root と `+ new session` もダブルクリック対象になり、modal / inline input が背景を所有する期間をまたいだ click latch も shell 側では安全に失効できない。

旧 sibling session `triage-fix-session-double-click-switch` は branch tip が現行基点と同じで committed 調査差分はない。上位 sandbox 制約により sibling worktree の未コミットファイルは直接参照せず、本 issue の内容は現行コードから独立に再検証した。

## 設計

- double-click の判定責務を座標だけを見る shell から controller-owned pointer state へ移す。
- real terminal shell は click の座標と単調時刻（または同等の deterministic timestamp）を `AppEvent::Pointer` として渡すだけにする。判定ロジックには時計を直接読ませず、テストから時刻を注入できる純粋な reducer/runtime seam にする。
- reducer は hit-test 後の `Selection::Target(Target::Session(SessionId))` だけを click candidate として保持する。直前 candidate と stable `SessionId` が一致し、経過が 400ms 以下の場合だけ Enter と同じ activate / Closeup 遷移を行う。
- 最初の click と window 外 click は session の選択だけを行い candidate を更新する。double-click 成立後は candidate を消費し、triple click が連続 activate にならないようにする。
- 別 session row、Root、`+ new session`、sidebar miss は pending candidate を失効させ、activate しない。Root / new session の単クリック選択という既存 pointer UX は維持する。
- overlay / inline create input が pointer を所有する間の背景 click は state/selection/route を変更せず、pending candidate を引き継いで後続 click を誤って二打目にしない。
- sidebar scroll や row list / backend snapshot の更新で座標に別 identity が来ても stable ID 不一致で activate しない。snapshot reconciliation 時は pending click state を明示的に失効させ、同じ ID が残った場合も snapshot 境界をまたぐ誤発火を防ぐ。
- keyboard Enter の既存挙動は変更しない。pointer double-click は session row に限りその activate path を再利用する。
- `Instant::now()` に依存する分岐や double-click 判定ロジックへ `#[coverage(off)]` を付けない。実時計の取得だけを shell boundary に閉じ込める。

## スコープ

### 含める

- `crates/tui/src/usecase/application/controller.rs` の pointer event/state/reducer と session activate 経路。
- `crates/tui/src/presentation/mod.rs` の controller runtime click wiring。
- 必要に応じて pointer timestamp 用の小さな runtime clock seam。
- `document/03-tui.md` の Home sidebar mouse 契約更新。
- deterministic reducer / runtime tests と既存 pointer tests の契約修正。

### 含めない

- live terminal の drag selection / copy / URL click。
- sidebar layout や row rendering の変更。
- keyboard Enter / Ctrl 系 key binding の意味変更。
- Root / `+ new session` のダブルクリック activate（明示的に禁止）。

## 回帰テスト（必須）

- 1 回目の session click は selection のみで active / route / overlay を変えない。
- 同じ `SessionId` の 2 回目が 400ms 以下なら Enter と同じ active target / Closeup / overlay 契約になる。
- window 超過は activate せず新しい 1 回目として扱う。境界値 400ms も固定する。
- 別 session、Root、`+ new session`、sidebar miss を間に挟むと誤 activate しない。
- scroll 後に同じ座標へ別 `SessionId` が見えても activate しない。
- modal / inline create input 背景の click は inert で、閉じた後の click と結合しない。
- backend snapshot / session list reconciliation 後は、同じ座標または同じ ID でも以前の click と結合しない。
- double-click 成立後の追加 click で再 activate しない。
- shell/runtime adapter が click 座標と注入時刻を controller event へ欠落なく渡す。
- deterministic tests は sleep を使わない。
- coverage 100% を維持する。

## ドキュメント

`document/03-tui.md` を実装後の現在形で更新し、session row の single click / double-click、stable identity と時間窓、Root / `+ new session` 非対象、modal / inline input ownership を記載する。内部 state 名は仕様へ持ち込まない。

## 完了条件

- 上記の誤発火ケースをすべて deterministic test で固定する。
- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings`、推奨 selected tests、Markdown link check が通る。
- issue を実装 session 自枝で `in-progress` → PR 前に `done` とし、Draft PR を作成する。
- CI の fmt / clippy / full test / coverage 100% / Markdown link check が green になった後、Ready for review にする。
