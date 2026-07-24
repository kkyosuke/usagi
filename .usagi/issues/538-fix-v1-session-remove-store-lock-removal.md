---
number: 538
title: fix(v1/session): remove の store lock を短命化し中断した removal を自己回復させる
status: in-progress
priority: high
labels: [v1, session, concurrency, durability]
dependson: []
related: [469, 470]
created_at: 2026-07-24T22:37:39.595412+00:00
updated_at: 2026-07-24T22:38:58.287881+00:00
---

## 問題・影響

出荷バイナリ（`~/.usagi/bin/usagi` 2.9.0 = `v1/src/` のコード）の `session remove` が、workspace store の cross-process flock `.usagi/.lock` を**破壊的 IO の全区間で保持**する。実地で MCP `session_remove` が 120 秒超ブロックし、その間ワークスペース全体の write 系操作が止まった。

確認した事実（関数単位）:

| 箇所 | 事実 |
|---|---|
| `v1/src/usecase/session/mod.rs::remove` | 冒頭で `store.lock()` を取り、`reconcile_locked` → tombstone 保存 → `discard_session` → context cleanup → 最終 commit まで**一度も解放せず**関数末尾の drop まで保持する |
| `v1/src/usecase/session/reconcile.rs::discard_session` | `git::remove_worktree` → `fs::remove_dir_all(root)` → `prune_worktrees` → `delete_branch` を実行。`root` にはセッションの `target/`（coverage 実行後は `target/llvm-cov-target` が数 GB）が含まれ、削除に数分かかる |
| `v1/src/infrastructure/store_lock.rs` | `ACQUIRE_TIMEOUT` は 10 秒。つまり他プロセスは待つのではなく **10 秒で "timed out waiting for the store lock" エラーになる** |

したがって他プロセス（別セッションの `usagi mcp`、TUI の write 系、`create` / `set_note` / `reorder` / `persist_last_active`）は数分間、単に失敗する。`list` / `statuses` は lock を取らないので読み取りは生き残る。

さらに、中断した removal を再開する経路が無い。`pending_removals`（`SessionRemovalPhase::GitTeardown` / `ContextCleanup`）を drain するのは `remove(name)` を**手で再実行したときだけ**で、`reconcile_locked` は stray を `Orphaned` に隔離するのみ（`reconcile.rs`）。しかも `Orphaned` は自動 force 削除を拒否するため人手が必要になる。`create` は pending removal が名前を所有していると `already exists or has a pending removal` で拒否するので、中断すると**その名前が恒久的に塞がる**。

なお公開 API の `session::reconcile()` は現状 production から呼ばれていない（`pub use` されているだけで、`create` / `remove` は `reconcile_locked` を直接呼ぶ）。

## 成立条件 / 再現フロー

1. セッション worktree でビルド・coverage を回し `target/` を数 GB にする。
2. そのセッションに対し `session_remove`（MCP / TUI / CLI いずれでも）を実行する。
3. 実行中に別プロセスから write 系操作（`session_create`、`session_note_update` 等）を呼ぶと 10 秒後に store lock timeout で失敗する。
4. 手順 2 を `fs::remove_dir_all` の途中で kill すると `pending_removals` が `git_teardown` に残り、その名前は `create` に拒否され続ける。誰も再開しない。

## 対象責務と非対象

### 対象

1. **lock scope の短命化**。lock は「durable な状態遷移」だけで保持し、重い IO は解放して実行する（v2 の `crates/daemon/src/usecase/session_runtime.rs::perform_remove` の begin(lock 内) / execute(lock 外) / finish(lock 内) 分割が参考パターン）。段取り:

   | 段 | lock | 内容 |
   |---|---|---|
   | precheck | なし | state を load し、対象 session の存在・`Orphaned` 拒否・dirty 判定（`git::worktree_status`）を行う |
   | begin | store lock | `reconcile_locked` → state を再 load → 再検証 → tombstone（`phase=git_teardown`）を save → 解放 |
   | execute | なし | `list_repo_worktrees` + `discard_session`（git worktree remove / `remove_dir_all` / prune / branch delete） |
   | commit teardown | store lock | `phase=context_cleanup` を save → 解放 |
   | context cleanup | なし | `agent.forget_session` / `clear_removal_context` |
   | finish | store lock | session record と tombstone を落として save |

   lock を挟むたびに `Vec` の index は無効になるため、各 locked window で**名前から再解決**する。dirty 判定を lock 外へ出しても安全性は落ちない（実際の防壁は `discard_session` の `git worktree remove`（force なしなら git が dirty worktree を拒否）であり、既存テスト `discard_session_without_force_aborts_on_a_dirty_worktree_and_keeps_it` が固定している）。

2. **同一セッションの重複 teardown を防ぐ per-session teardown lock**。store lock を離す間、別プロセスが同じ session の `discard_session` を並走させると、ownership preflight（canonicalize / worktree 登録の照合）が race で不整合になり `OwnershipError` → `Orphaned` 隔離（＝人手が必要）に落ちうる。session ごとの専用 lock ファイル（例 `.usagi/removals/<name>/.lock`）を **store lock より先に**取得し、`teardown lock → store lock` の順序を全経路で固定して deadlock 環を作らない。取得は短い timeout の try とし、失敗は「removal が進行中」という明示エラーにする。

3. **中断した removal の自己回復**。`git_teardown` / `context_cleanup` に留まった tombstone を同じ machinery で完遂する resume を追加する。v1 は daemon を持たないため常駐 drain worker は作らず、同期エントリポイントで完遂させる:

   | 呼び出し元 | 対象 |
   |---|---|
   | `remove(name)` | その name の tombstone（現状の挙動を machinery 経由に統一） |
   | `create(name)` | その name を塞いでいる非 `Orphaned` tombstone を先に完遂してから作成（今の即 bail をやめる） |
   | `reconcile()`（公開エントリ） | 非 `Orphaned` の全 tombstone。quarantine パスの lock を解放した**後**に実行する |

   resume は caller を失敗させない（1 件詰まった tombstone が `create` 全体を止める方が悪い）。結果は per-session の outcome として返し、呼び出し元がログに出せる形にする。

4. **`force` を tombstone に永続化する**。`PendingSessionRemoval` に `#[serde(default)] force: bool` を追加し（既存 state.json と後方互換）、resume が最初の removal と同じ force 判断を引き継ぐ。現状は resume 時に force が失われ、teardown が途中で失敗して worktree が dirty のまま残ったケースを再開できない。

### 非対象

- MCP server の逐次 dispatch そのものの並行化（同一プロセス内で 1 件の長い tool 呼び出しが後続要求を止める問題）。本 issue の修正は**別プロセス**の store lock timeout を解消するが、同じ `usagi mcp` プロセスが持つ後続要求のブロックは残る。→ 別 issue で扱う。
- `remove` 自体の応答時間を O(1) にする rename-to-trash 化。→ 別 issue（本 issue に依存）。
- TUI の periodic tick から sweep する配線。tick は UI スレッド寄りの経路であり、削除が O(1) になるまで tick で重い IO を起こさない。

## 受入条件

- [ ] 数 GB の `target/` を持つセッションの `session_remove` 実行中に、別プロセスの store lock 取得（`session_create` / note 更新など）が timeout せず成功する。
- [ ] store lock は precheck / execute / context cleanup の各区間で解放されている。
- [ ] 同一 session に対する remove の並走が、`Orphaned` 隔離ではなく「進行中」の明示エラーになる。
- [ ] `git_teardown` / `context_cleanup` で中断した removal が、`state.json` の手編集なしに `reconcile()` / `create(同名)` / `remove(同名)` のいずれかで完遂する。
- [ ] force 付きで開始した removal の resume が force を引き継ぐ。
- [ ] `Orphaned` tombstone は resume 対象外で、自動 force 削除されない（`470` の fail-closed を維持）。
- [ ] 既存の removal 系テスト（context 保全・部分 teardown の再試行・ghost 隔離・fail-closed 群）の挙動とエラーメッセージが変わらない。

## 必須回帰テスト

- teardown 区間（begin 直後、`execute` 呼び出し前）で store lock が短い timeout 内に取得できる。
- context cleanup 区間で lock が空いている（`Agent` fake の `forget_session` 内から短い timeout で lock を取得して確認する）。
- teardown lock を保持した状態の `remove` が「進行中」エラーで即座に返り、tombstone を壊さない。
- `git_teardown` 中断からの resume（`reconcile()` 経由）が worktree・branch・session record・tombstone をすべて片付ける。
- `context_cleanup` 中断からの resume が git を再実行せずに完遂する。
- 中断した removal を `create(同名)` が完遂させてから作成に成功する。
- force 付き removal の中断 → resume が dirty worktree を discard して完遂する。
- resume が失敗し続ける tombstone があっても、別 name の `create` / `reconcile()` が成功する。
- `Orphaned` は resume されない。

## docs / 移行影響

`state.json` の `pending_removals[].force` は追加フィールドで、既存ファイルは `false` として読める（旧バイナリは未知フィールドを無視する）。v1 の仕様ドキュメント（`v1/document/`）は退避版のため更新しない。
