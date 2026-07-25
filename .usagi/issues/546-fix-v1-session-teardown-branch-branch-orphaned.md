---
number: 546
title: fix(v1/session): teardown の branch 照合を実際の branch に基づかせ、生きたセッションを orphaned 隔離しない
status: todo
priority: high
labels: [v1, session, durability, fail-closed]
dependson: []
related: [538, 541, 547]
created_at: 2026-07-25T02:23:01.213786+00:00
updated_at: 2026-07-25T02:24:08.480044+00:00
---

## 問題・影響

出荷 v1（`~/.usagi/bin/usagi` 2.9.0 = `v1/src/`）の `session remove` は、teardown が照合する branch 名を **セッション名から毎回導出**する（`v1/src/usecase/session/mod.rs::begin_removal` が `branch: branch_name(name)` を `RemovalPlan` に詰める。`branch_name` は `usagi/<name>` を返す唯一の SSoT）。実際に作成された branch は `SessionRecord` にも `WorktreeProvenance` にも `PendingSessionRemoval` にも**永続化されていない**。

そのため、セッション worktree の HEAD が `usagi/<name>` から外れた瞬間に、**健全に生きているセッションが恒久的に削除不能**になる。セッション内でエージェントが作業 branch を切り直す・`git branch -m` で改名する、といった普通の操作がこれを引き起こす。

連鎖はこうである。

| 段 | 箇所 | 挙動 |
|---|---|---|
| 1 | `reconcile.rs::discard_session` | 候補 worktree は記録済み provenance（repo / git common dir / 記録 path）と一致するので `identity.is_some()`。しかし `branch_matches`（`wt.branch == Some("usagi/<name>")`）が false のため `worktree ... lacks complete ownership proof` の `OwnershipError` を返す |
| 2 | `mod.rs::execute_teardown` | `OwnershipError` を**一律** `SessionRemovalPhase::Orphaned` へ落とす |
| 3 | `mod.rs::removal_target` | `Orphaned` の session は以後 `remove` を**`--force` でも**拒否する（`is quarantined as an orphaned pending removal`） |
| 4 | `mod.rs::resume_pending_removals` | `Orphaned` を skip するので `usagi clean` でも回復しない |
| 5 | `mod.rs::create` | tombstone が名前を所有するので `already exists or has a pending removal` で同名再作成も拒否 |

結果、そのセッション名は `state.json` を手編集するまで**削除も再作成もできない**（実地で名前 `issue-525` / branch `usagi/issue-525-tui` の組み合わせで発生を確認した）。破壊的効果は一切出ていない（#470 の fail-closed は正しく効いている）ので、データが失われるわけではないが、**復帰手段が手編集しか無い**のが問題である。

### 再現（v1 lib test で確認済み）

`v1` の `usecase::session::tests` で次を実行すると、上記 1〜4 がそのまま観測できる。

1. `create(root, "issue-525")` でセッションを作る（HEAD = `usagi/issue-525`）。
2. その worktree で `git switch -c usagi/issue-525-tui`。
3. `remove(root, "issue-525", /* force */ true, ...)` → `Err`:
   `session "issue-525" ownership is ambiguous and has been quarantined; ... worktree <root>/.usagi/sessions/issue-525 lacks complete ownership proof`
   このとき `pending_of(...) == Some(Orphaned)` かつ worktree は残っている。
4. `--force` で再試行 → `Err`: `session "issue-525" is quarantined as an orphaned pending removal; ...`。phase は `Orphaned` のまま。
5. `resume_pending_removals(...)` → `[]`（skip される）。phase は `Orphaned` のまま。

## 原因の本質

`discard_session` の ownership 証明が、性質の異なる 2 つの照合を **AND で結んだ上に、どちらが欠けても同じ「危険」として扱っている**。

| 照合 | 何を証明するか | 欠けたときの意味 |
|---|---|---|
| identity（repo canon + git common dir + 記録 worktree path、かつ `root_canon` 配下） | **その worktree がこのセッションのものである**こと | 証明できない → 消してはいけない（本当に危険） |
| `branch_matches`（`wt.branch == Some(branch)`） | worktree に checkout されている **ラベル**が導出名と一致すること | ラベルが変わっただけ。所有関係は identity 側で既に証明済み |

`branch` が導出値であるため、後者は「セッションの記録」ではなく「セッション名からの推測」との照合になっている。ここが誤りである。逆向き（`branch_matches` は真だが identity が無い＝別物が同名 branch を持っている）は本当に危険なので、そちらの fail-closed は維持しなければならない。

## 対象責務と非対象

### 対象

1. **branch を durable に記録する**。セッションが実際に作成した branch を `SessionRecord`（および teardown が resume から参照できるよう `PendingSessionRemoval`）に持たせる。`#[serde(default)]` で後方互換を取り、既存 `state.json`（フィールド無し）は従来どおり `branch_name(name)` に解決する。`branch_name` は create 時の SSoT として残す。
2. **teardown の authorization を identity 主体に組み替える**。候補 worktree が「記録 provenance と一致し、かつ canonical に session root 配下」であれば ownership は証明済みとして target に採る。checkout されている branch 名の不一致は、それ自体を `OwnershipError` にしない。逆向き（branch は一致するが identity が無い）は今のまま `OwnershipError` で fail-closed を維持する。
3. **記録外の branch は消さない**。`git branch -D` の対象は記録された branch に限る。改名先（例 `usagi/issue-525-tui`）は記録が無いので削除せず、残ったことを呼び出し元へ報告できる形にする（黙って消すのは #470 の趣旨に反する）。記録 branch が既に存在しない場合の skip は現状の `if git::branch_exists` のままで成立する。
4. **`OwnershipError` → `Orphaned` の一律降格を見直す**。`execute_teardown` が隔離するのは「所有権が証明できない」場合に限る。teardown 由来の隔離と reconcile 由来の隔離が同じ `Orphaned` 値を共有していて区別できない点も併せて整理する（前者は provenance を完備しているのに後者は空である。回復経路の設計は #547 を参照）。

### 非対象

- `Orphaned` から抜け出す明示的な回復操作（既に隔離されてしまった workspace の救済）。→ #547。
- store lock の短命化と中断 removal の resume（#538、完了済み）。
- MCP の逐次 dispatch（#539）／rename-to-trash による O(1) 化（#541）。
- セッション名と branch の乖離そのものを禁止すること（create 時の検証強化）。乖離は worktree 内の正当な git 操作で起こり得るので、禁止ではなく teardown 側が耐えるのが正しい。

## 受入条件

- [ ] worktree の HEAD が `usagi/<name>` 以外へ移ったセッションの `remove` が成功し、worktree・session tree・記録 branch・session record・tombstone が片付く。
- [ ] そのケースで tombstone が `Orphaned` に落ちない。
- [ ] 記録されていない branch（改名先）は削除されず、残存が呼び出し元に分かる。
- [ ] identity を証明できない候補（branch 名だけ一致する別物）は従来どおり `OwnershipError` で隔離され、破壊的効果を一切出さない（#470 の fail-closed 維持）。
- [ ] `branch` フィールドを持たない既存 `state.json` が `usagi/<name>` として読め、挙動が変わらない。
- [ ] 既存の ownership fail-closed テスト群と、そのエラーメッセージが変わらない。
- [ ] 中断からの resume（#538 の machinery）が記録 branch を引き継ぐ。

## 必須回帰テスト

- HEAD を別 branch に切り替えたセッションの `remove` が完遂し、phase が `Orphaned` にならない。
- `git branch -m` で記録 branch を消したセッションの `remove` が完遂し、改名先 branch が残ることを assert する。
- branch 名だけ一致し identity が無い worktree は `OwnershipError` になり、session tree が消えていない。
- `branch` 無しの `state.json` を読み込んだ removal が `usagi/<name>` で完遂する（後方互換）。
- `git_teardown` 中断 → resume が記録 branch を使って完遂する。
- 記録外 branch を `delete_branch` の対象にしない。

## docs / 移行影響

`state.json` の `sessions[].branch` / `pending_removals[].branch` は追加フィールドで、既存ファイルは欠落として読める（旧バイナリは未知フィールドを無視する）。v1 の仕様ドキュメント（`v1/document/`）は退避版のため更新しない。
