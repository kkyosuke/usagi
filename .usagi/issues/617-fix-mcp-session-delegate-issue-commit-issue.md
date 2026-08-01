---
number: 617
title: fix(mcp): session_delegate_issue の commit 済み判定を issue 番号で解決する
status: todo
priority: high
labels: [orchestration, mcp, v1, fix, correctness]
dependson: []
related: [104, 110, 618]
created_at: 2026-07-31T23:44:38.548451+00:00
updated_at: 2026-07-31T23:45:12.997984+00:00
---

## 症状（P1 運用ブロック）

root（`main` チェックアウト）で MCP `session_delegate_issue` を実行すると、origin/main に commit 済みの issue に対して次の error が返り、委譲が完全に blocked になる。

```
issue #605 is not committed to the base branch (origin/main) yet:
uncommitted issues will not be present in the new session's worktree.
```

commit しても merge しても解消しない。回避策は `session_create` + `issue_to_prompt` + `session_prompt` の手動 3 手。

## 根本原因

`v1/src/presentation/mcp/usagi.rs::tool_delegate_issue` は precheck の対象 path を `format!(".usagi/issues/{}", rendered.file_name)` で組み立てる。`rendered.file_name` は `Issue::file_name()` ＝ `{:03}-{slugify(title)}.md` で、**title から派生した名前**であって store に実在する path ではない。

一方 issue の読み出しは番号 prefix（`IssueEntry::key_from_path` → `number_from_filename`）で解決されるため、on-disk 名が派生名と違っても `issue_get` / `issue_search` / `issue_to_prompt` は成功する。結果として「読めるのに commit 済み判定だけ落ちる」false negative になる。

#605 の実測:

| | 名前 |
|---|---|
| title slug からの derived（precheck が探す） | `605-fix-daemon-worktree-git-command-inherited-repository.md` |
| 実ファイル（#1373 でこの名前で追加。rename されていない） | `605-fix-daemon-git-command-environment-confinement.md` |

`git cat-file -e origin/main:.usagi/issues/<derived>` は失敗し、実ファイル側は存在する。

派生名と実ファイル名がずれるのは、store を経由せず markdown を直接書いて起票した issue（bulk backlog PR）や、起票後に title を変えた issue で起きる。**現在の backlog 554 件中 29 件が該当**し、そのうち #604〜#611・#613〜#615 の 11 件は review backlog（いま委譲したい対象そのもの）である。

`IssueStore::write_locked` は書き込みのたびに canonical（派生）名へ寄せて stale sibling を削除するため、session が着手後に `issue_update`（status 遷移）を 1 回でも実行すると実ファイル名が派生名に揃う。実装 PR の diff に `{...-old.md => NNN-....md}` の rename が現れるのはこれであり（例: #1379 の `612-...`）、**一度着手した issue だけが後から委譲可能になる**という循環になっている。

## 棄却した仮説（再調査を避けるため記録）

- **index cache の stale**: `.usagi/issues/index.json` を消しても再現する。トリアージ session の worktree には index.json が存在しないが `issue_search` は同じ derived 名を返す。derived 値は毎回 title から計算されるだけで cache は無関係。
- **source fingerprint が rename を検出しない**: v1 `infrastructure/markdown_store.rs::source_fingerprint` も v2 `infrastructure/persistence/markdown_store.rs::source_fingerprint` も、file 名を長さ framing 付きで hash に含めている。rename は検出される。
- **MCP プロセスの in-memory cache**: v1 の MCP server は store の in-memory index を保持しない。プロセス再起動は不要。

## 最小修正方針

precheck を **derived 名の lookup から番号解決**へ変える。

1. `v1/src/infrastructure/git/branch.rs` に `files_at_rev(repo, rev, dir) -> Vec<String>`（`git ls-tree -r --name-only <rev> -- <dir>`）を `file_exists_at_rev` の隣に追加する。
2. `tool_delegate_issue` は base ref の `.usagi/issues/` の entry から、`number_from_filename` 相当で番号が一致するものを集める。
   - 1 件 → commit 済みとして続行する。
   - 0 件 → 現在と同じ「not committed」error（真の未 commit はこれまでどおり拒否する）。
   - 2 件以上 → ambiguity error で拒否し session を作らない（store の `AmbiguousIssueNumber` と同じ方針）。
3. `rendered.file_name` は precheck に使わない。error message には解決に使った番号と base ref を出す。

既存 29 件の一括 rename（data migration）は不要。実ファイル名が何であれ番号で解決できるようになる。

v2 の `session_delegate_issue` は現状 tool schema だけで、`SessionRuntime` は `SessionAction::DelegateIssue` を `InvalidRequest` で返す（未実装）。v2 で実装するときも「番号で base ref を解決する」ことを前提にし、derived 名を path として使わない。

`file` が実在しない path を報告する構造そのものは store 側の問題で、本 issue では直さない（related の store 側 issue を参照）。本 issue は運用のアンブロックに限定する。

## テストと受け入れ条件

v1 `presentation/mcp/usagi.rs` の既存 fixture（`init_repo` / `server_at` / `commit_issues`）で追加する。

- **回帰テスト**: title と噛み合わない名前（例 `001-hand-written-name.md`）で issue markdown を直接置いて commit した状態で `session_delegate_issue` が成功し、session が作られる。修正前はこの test が「not committed」で落ちる。
- **真の未 commit**: 既存の `delegate_issue_refuses_uncommitted_issue` は引き続き error（false positive を作らない）。
- **ambiguity**: 同番号の 2 ファイル（`001-a.md` / `001-b.md`）を commit した状態では拒否し、`session_list` が空のまま。
- **base ref 未解決**: 既存 `delegate_issue_without_a_resolvable_base_uses_head_in_the_error` の挙動を維持する。
- `files_at_rev` の unit test（entry のある dir / 空 dir / 不明 rev）。
- v1 のカバレッジ 100% を維持する。
