---
number: 384
title: fix(tui): 新規 session 作成中の skeleton wave（loading）が表示されない回帰を直す
status: done
priority: high
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-20T01:14:13.614950+00:00
updated_at: 2026-07-20T02:14:26.889623+00:00
---

# 概要

v2 TUI の Home サイドバーで新規 session を作成（`+ new session` → `+ new: <name>` inline 入力 → Enter）したとき、**daemon 作成完了までの間に出るはずの「skeleton wave」（loading アニメーション）が表示されない**回帰。

ユーザー報告: 「作成の時の waving が消えている。削除の時には出ている」。**削除の wave（`✂` + Danger shimmer）は正常**。壊れているのは**作成側の skeleton wave**。

## 正本仕様（既に document/03-tui.md に記載＝あるべき挙動）

`document/03-tui.md:147-151`:
> 名前を入力して Enter を押すと通常の `session create <name>` と同じ daemon request を非同期に開始し、**完了まで行の直前（`+ new session` の直前）に session と同じ 2 行の skeleton を表示する。skeleton の activity glyph と session 名は同じ左から右へ流れる低速の wave で描き**、静的な点滅にはしない。daemon が同一 `OperationId` と revision を持つ `session.created` 完了 hook を返したときだけ、skeleton をその response 内の snapshot row に置き換えて loading を終了する。

`document/03-tui.md:139-140,94-95` も「作成中の skeleton は `+ new session` の直前に置く」「pending skeleton は current target にならない（非選択）」と規定。

これは **doc drift**（仕様に書かれているが実装が無い＝「記載＝実装済み」規約違反）。本 issue は実装を仕様に合わせて回帰を解消する。

# 根本原因（file:line 付き・確定）

作成の pending skeleton wave が **本番の controller/legacy shell 経路にそもそも実装されていない**。削除は存在する snapshot 行に `removing` フラグを立てて shimmer するので動くが、作成は「まだ存在しない session」の合成 skeleton 行が必要で、それがどこにも投影・描画されていない。

- reducer `request_create_session`（`controller.rs:3142-3158`）は `state.pending` に `PendingKind::CreateSession` を積むが **name を保持せず**、sidebar 行にもならない。
- shell の `ui.creating_session: Option<PendingToken>`（`mod.rs:544`）は dispatch（`mod.rs:1618`）でセットされ drain（`mod.rs:1099`）で take されるが、**描画にも投影にも一切使われていない**（`removing_session` と非対称。`removing_session` は `project_controller_sessions` `mod.rs:1338` で `ProjectedSession.removing` に投影されるが、`creating_session` に対応する投影が無い）。
- `HomeProjection::from_state`（`workspace.rs:235-289`）と `home.rows()`（`workspace.rs:372-381`）は `Root → session* → NewSession` だけを作り、pending 作成 skeleton 行を挿入しない。`home_row_lines_at`（`workspace.rs:1215-1290`）にも create skeleton 分岐が無い（`create_draft` inline 入力と `removing` のみ）。

経緯: inline 作成仕様（#361）→ CreateSession modal（#315/#1055）→ inline へ revert（#1089）と作成 UI が変遷する中で、当初 inline 仕様にあった作成 skeleton wave の描画が落ちた。削除 wave（#1078）は別途 `removing` として実装されているため生き残った。

# 実装方針（削除 wave と対称に shell 経路で）

作成 skeleton は shell state `ui.creating_session` を情報源にする（lifecycle が正しい: dispatch でセット、`drain_session_completions` が完了時に take＝daemon が新 row を投影する瞬間にクリアされ、skeleton→実 row の atomic swap になる）。reducer の `state.pending` は legacy 経路では成功時に OperationResult が飛ばず clear されないため情報源に使わない。

1. `ui.creating_session` に **作成中の name を保持**させる（`Option<PendingToken>` → token+name。dispatch で `intent.name` を格納）。
2. `render_controller_frame` に pending 作成 name を渡し、`HomeProjection.create_pending: Option<String>` へ thread。
3. view: `+ new session` の直前に **2 行の skeleton**（activity glyph + name を `mascot_tick` 駆動の低速 wave で描画。非選択・current 対象外）を描く純関数を追加し、`home_left_pane` の viewport 計上に skeleton 2 行を含める。
4. 完了時（`drain_session_completions` の take）に自動でクリア。成功時は `apply_session_projection` が同フレームで実 row を出すため skeleton と実 row が二重表示・取りこぼしにならない。

# 追加する regression tests（reducer / render / runtime / fake daemon）

- **render（純関数）**: `create_skeleton_lines(width, name, tick)` が 2 行・幅 pad・name を含み、tick でフレームが変化する（静的点滅でない）。
- **render（frame）**: `render_controller_frame` に create pending name を与えると `+ new session` の直前に waving skeleton 行が出る／tick で進む。
- **runtime/shell**: `Effect::CreateSession` dispatch で `creating_session`（name 付き）がセットされ、`drain_session_completions` で成功/失敗いずれもクリアされる。成功時は実 row に置換、失敗時は error dialog（#1091）で skeleton が残らない。
- **fake daemon**: fake `SessionCommandPort` で create 成功/失敗、Esc cancel、reconnect/stale/duplicate row を回帰させない。

# 壊してはいけない不変条件

- typed lifecycle（`Effect::CreateSession` → `SessionCommandPort::create` → `OperationId` 保存・blind retry しない）を維持（#1089）。
- inline `+ new: <name>` 入力・local validation・折り返し error（#1089/#1090/#1097）を壊さない。
- 作成失敗 dialog（#1091）が skeleton/pending を片付けて開く挙動を維持。
- pending skeleton は非選択・current 対象外（`document/03-tui.md:94-95`）。
- `Enter` 連打で二重作成しない（作成実行中は入力を読まない）。
- 削除 wave（`removing`）を回帰させない。
- root 行・`+ new session` 行は作成 skeleton の位置・順序（`Root → session* → skeleton → NewSession`）を保つ。

# 参考（コード位置）

- shell: `mod.rs:544, 615, 1099, 1618`（`creating_session`）, `mod.rs:1565-`（`render_controller_frame`）
- view: `workspace.rs:235-289`（`from_state`）, `372-381`（`rows`）, `1053-1135`（`home_left_pane`）, `1215-1290`（`home_row_lines_at`）
- reducer: `controller.rs:3142-3158`（`request_create_session`）, `2241-2270`（create 完了）
- 正本仕様: `document/03-tui.md:139-151`

削除 wave（正常動作）については本 issue の対象外。
