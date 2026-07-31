---
number: 584
title: refactor(daemon): session_runtime.rs の git/fs 実IOを infrastructure層へ分離する
status: in-progress
priority: high
labels: [daemon, architecture, coverage, refactor]
dependson: []
related: []
created_at: 2026-07-30T10:47:01.109312+00:00
updated_at: 2026-07-30T22:30:16.654666+00:00
---

## 背景

`document/02-architecture.md` は「PTY 所有・IPC socket サーバ・daemon 永続化（daemon 専用の外部接続）」を `crates/daemon/src/infrastructure/` に置くと定め、`document/06-conventions.md` は「実 IO は引数やジェネリックで注入し、本物の IO は合成ルートで束ねる」ことを求めている。

`crates/daemon/src/usecase/session_runtime.rs`（3065行）はこの境界に反し、`usecase/` 直下に実 IO を直接実装している。

- `SystemGit::run`（`session_runtime.rs:109-123`）が `std::process::Command::new("git")` を直接呼ぶ。
- `canonical_path` / `build_session_tree` / `mirror_directory` / `collect_session_worktrees` 系（`session_runtime.rs:1233-1293` 付近）が `std::fs::canonicalize` / `fs::create_dir_all` / `fs::read_dir` を直接呼ぶ。
- ファイル冒頭のコメント自身が「This adapter is their only daemon-side effect owner」（`session_runtime.rs:3-6`）と明記し、実 IO をこの usecase ファイルに意図的に集約していることを認めている。

さらに、このファイル先頭には module 全体を覆う `#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_runtime_fake_git_contract`（`session_runtime.rs:8`）があり、`mod tests`（`session_runtime.rs:1454`）より前の**production コード全体（約1453行）**が計測対象外になっている。この範囲には `SystemGit::run` のような真の real_io だけでなく、validation（`valid_legacy_name`）・reconcile（`adopt_legacy_workspace_sessions` / `reconcile`）・parse（`session_name` / `force`）・error mapping（`worktree_failure_detail`）等も含まれる。`document/06-conventions.md` の `coverage(off)` 例外ポリシーは reason=`composition` を「production の依存を束ねるだけ」に限定し、「reducer、parser、validation、reconcile、error mapping は許可理由にならない」と明記しており、この module-wide 適用はポリシーに反する。他の daemon usecase ファイル（`terminal_selection.rs` 等）は同じ `composition` 理由を `#[cfg(test)] mod tests` の内側だけに絞っており、`session_runtime.rs` だけが production スコープへ拡張している。`scripts/coverage-off-lint.rb` は reason/owner/expires/tests フィールドの存在のみを検証し、実際のコード内容が reason に適合しているかは検証しないため、この違反は CI lint を通過している。

## 対象

- `SystemGit::run` と実 fs 操作関数（`canonical_path` / `build_session_tree` / `mirror_directory` / `collect_session_worktrees` など）を `crates/daemon/src/infrastructure/` へ切り出す。
- `usecase/session_runtime.rs` はポート（trait）越しにこれらを呼ぶ形に変更し、reducer・validation・reconcile ロジックは fake port で計測可能なユニットテストの対象に戻す。
- 分離後に残る真の real_io 関数だけへ、function 単位で `#[coverage(off)]` を再付与する（module-wide の `#![coverage(off)]` は撤去する）。
- `coverage-off-allowlist.json` 側の登録が必要であれば追加する。

## 受入条件

- [ ] `crates/daemon/src/usecase/session_runtime.rs` に `std::process::Command` / `std::fs::*` の直接呼び出しが残らない（ポート越しの呼び出しに置き換わる）。
- [ ] module-wide の `#![coverage(off)]` が撤去され、真の real_io 関数だけに function 単位の `#[coverage(off)]`（reason=real_io、fake/integration test 証跡付き）が付与されている。
- [ ] validation / reconcile / parse / error mapping ロジックがカバレッジ計測対象に戻り、既存の振る舞いを検証するユニットテストが追加されている。
- [ ] `ruby scripts/coverage-off-lint.rb` と coverage gate（100%）が green。
- [ ] `cargo test -p usagi-daemon` が green。
