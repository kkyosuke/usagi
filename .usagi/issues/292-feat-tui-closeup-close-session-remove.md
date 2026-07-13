---
number: 292
title: feat(tui): Closeup close を session remove と共通化する
status: todo
priority: high
labels: [tui, closeup, session, parity]
dependson: [258]
related: [260, 268, 274, 285, 286, 288]
parent: 227
created_at: 2026-07-13T12:10:03.702657+00:00
updated_at: 2026-07-13T12:10:45.569179+00:00
---

## 背景

v1 の `close` は focused session を `Effect::CloseSession { force }` に正規化し、`-f` と `--force` は dirty worktree の破棄を許可する同値フラグである。名前なし `session remove` は複数選択の RemoveModal を開き、cancel、二重確定防止、一覧更新・失敗時の復旧を modal state が所有する。

v2 では Closeup の `close` が controller 内の `parse_close_force` で空文字列または `--force` だけを独自解析し、Overview の `session remove` も active `SessionId` 固定の別 parser である。実 runtime は #258 が接続する controller projection と別の legacy state をなお通るため、v1 と同じ selector / focus landing を Closeup に追加する前に共通 command → state → effect の境界を定義する必要がある。

## 目的

Closeup の `close` を session remove と同じ target grammar、候補補完、複数選択 modal、daemon-authoritative remove effect に統一する。対象を指定しない close は session remove と同じ selector を開き、`-f` / `--force` は位置にかかわらず同値に扱う。削除・cancel・snapshot 更新・同名再作成・競合時も stable `SessionId` 以外を実行 target に使わない。

## スコープ

- `session remove` と Closeup `close` が共用する typed remove command/parser を置く。対象名（必要なら current target を含む）と force を構文化し、name/path/表示 index を effect identity に持ち込まない。no-target の Closeup close は selector-open intent に正規化する。
- `-f` と `--force` を同じ flag とし、先頭・末尾・target 前後で受理する。未知 flag、重複 flag、複数 target、空 token、root/current session、invalid/ambiguous candidate は notice と no effect で安全に拒否する。v1 の寛容に無視する legacy 挙動は誤削除を招かない形で parser contract として明文化し、session remove と完全に揃える。
- snapshot の stable `SessionId` / label projection から候補を出す Remove selector state を controller projection に統合する。empty list、reorder、stale snapshot、remove/recreate、selector open 中の更新では selection を stable ID で再解決し、消失した候補を dispatch しない。
- selector は v1 と同じ keyboard flow（↑/↓/j/k、Space、Enter、Esc）を持つ。Enter は空選択・pending 中に no-op、confirm は選択済み ID だけを一度ずつ typed lifecycle remove effect に変換する。cancel は Closeup/Overview の origin、focus、overlay/input ownership を復旧する。
- non-force は daemon remove の dirty guard をそのまま通し、force だけが破棄を許可する。Closeup の direct target remove と selector remove は同一 effect/port を使い、TUI local git/store fallback、blind retry、name-based late completion を追加しない。
- remove accepted/progress/final と snapshot 反映は既存 lifecycle reducer の operation/revision/fencing を再利用する。Closeup から削除を要求した場合の Switch landing と adjacent selection は v1 の操作感を #258 の row contract 上で定義し、他操作後は user selection を奪わない。

## 受け入れ条件

- Closeup で `close` を対象なしで実行すると session remove と同じ selector を開き、候補選択・confirm・Esc cancel が keyboard で動作する。
- Closeup / Overview の対象指定 syntax と completion は共通 registry/parser から導出され、`close foo -f`、`close -f foo`、`close foo --force` が同じ stable `SessionId` + `force: true` effect になる。
- `-f` と `--force` の重複、未知 flag、複数 target、root/current target、候補なし、候補の snapshot 消失、同名再作成、reorder、late/duplicate event が panic・誤削除・二重 dispatch を起こさない。
- non-force の dirty failure は対象を消さず安全な feedback と selector/selection を維持し、force は同じ daemon remove port に `force: true` だけを渡す。
- selector の空 confirm / pending 中 Enter は no-op、cancel は origin の overlay・focus・input owner を戻す。Closeup remove の成功 landing、background input 後の selection preservation、Overview の一覧-only remove を v1 契約に沿って回帰する。
- 実装済み仕様は `document/03-tui.md` の session command / modal / removal landing の正本へ更新する。

## テスト

- parser/completion: target あり/なし、`-f` / `--force` の全位置、unknown/duplicate flag、複数 target、空入力、root/current/ambiguous target。
- reducer/modal: empty/reordered snapshot、toggle/wrap/cancel/empty-confirm/pending-confirm、success/failure、dirty non-force、force、stale/duplicate completion、remove/recreate の stable-ID fence、focus/overlay restoration。
- controller/adapter: Closeup と Overview が同一 typed remove effect を生成し、selector の選択 ID だけが daemon port に一度ずつ渡ること。
- runtime: fake terminal + fake daemon lifecycle port で Closeup → selector → confirm/cancel、Closeup direct remove、Switch landing、background input、tiny/empty list を通す。
- push / PR 前は Rust full gate / coverage 100% と Markdown link check を実行する。

## 依存・境界

- #258 の controller projection を実 runtime の唯一の Home source にする作業に依存する。legacy `Workspace` state に selector を二重実装しない。
- #260 の typed session remove、#268 の daemon lifecycle、#274 の snapshot refresh、#285 の pane/session snapshot同期、#286 の Switch/Closeup transition、#288 の stable selection projectionと整合させる。
- v1 参照は `v1/src/presentation/tui/home/command/builtins.rs`、`state/modal.rs`、`event/handlers.rs`、`v1/document/design/home/05-overlays.md`。v1 自体は更新しない。
