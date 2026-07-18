---
number: 348
title: fix(daemon): legacy state.json の session を検証して一度だけ採用する
status: done
priority: high
labels: [daemon, tui, session, migration, regression]
dependson: [268, 273, 274]
related: [312, 343]
created_at: 2026-07-18T00:00:00.000000+00:00
updated_at: 2026-07-18T01:00:00.000000+00:00
---

## 背景・根拠

v2 daemon の `SessionRuntime` は shared `<data-dir>/daemon/sessions.json` の managed session だけを snapshot する。TUI の `FsWorkspaceLoader::open` はその available projection で repository-local `WorkspaceStateStore` の `sessions` を丸ごと置換するため、`<repo>/.usagi/state.json` だけに残る legacy session は daemon restart 後に sidebar から消える。

現 workspace では `state.json` に 7 件、`<data-dir>/daemon/sessions.json` には failed `test-1` だけがあり、shared lifecycle state が legacy worktree を再構成できないことを確認できる。既存 worktree を推測して lifecycle effect を再実行してはならない。

## 目的

shared lifecycle state を初期化する最初の daemon start に限り、検証済み legacy session を available managed session として atomically adoption し、stable `SessionId` / `WorktreeId` を永続化する。以後は v2 snapshot を権威とし、legacy state は UI-only metadata の保存先として読み取り結合する。

## スコープ

- `sessions.json` が存在しない場合だけ legacy `state.json` を読む。既存 v2 record または legacy lifecycle record がある場合は adoption せず、既存の durable state を変更しない。
- 全 legacy record を先に検証する。session name、期待される `<repo>/.usagi/sessions/<name>`、linked-worktree marker、canonical path、`git worktree list --porcelain` の repository / `usagi/<name>` branch binding が全て一致する場合だけ採用する。
- malformed / unreadable state、同名 record、欠損 worktree、path / repository / branch binding 不一致は fail-closed とし、`sessions.json` を一切作成しない。
- adoption は worktree 作成・削除を行わず、record ごとに fresh stable ID を一度だけ発行して available state として保存する。restart では同じ ID を復元する。
- `display_name`、origin、started_from、notes、PR、last_active など legacy UI-only metadata は `state.json` に残す。TUI projection は同名 available managed session に既存 record を結合し、lifecycle が決める root/name だけを authoritative にする。daemon / TUI は migration のために state.json を書き戻さない。
- normal lifecycle snapshot は従来どおり available managed session のみを公開し、worktree effect の単一書き手は daemon のままとする。

## 受け入れ条件

- legacy session の adoption 後、daemon restart をまたいでも sidebar loader は同じ session を表示し、stable SessionId / WorktreeId を使う。
- custom display name、notes、PR、origin を持つ legacy record は loader projection 後も保持される。
- failed / creating / deleting record は snapshot / sidebar に出ない。
- invalid legacy state、duplicate name、missing worktree、repository/branch mismatch、既存 v2 state の各ケースで部分 adoption や worktree effect は起きない。

## テスト方針

- fake Git の porcelain output と temp repository で adoption、restart、ID persistence、rejection を runtime integration test にする。
- TUI loader/projection で daemon restart 後の available session と legacy UI metadata の結合を regression test にする。
