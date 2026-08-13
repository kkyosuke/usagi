---
number: 675
title: fix(core): product-owned Git から repository-local helper 実行を無効化する
status: todo
priority: high
labels: [review, v2, core, daemon, git, security, integrity]
dependson: []
related: [605, 631]
parent: 671
created_at: 2026-08-13T22:28:26.087856+00:00
updated_at: 2026-08-13T22:28:26.087856+00:00
---

## Finding（P1 security / integrity）

`crates/core/src/infrastructure/git/environment.rs::confined_git_command` は inherited `GIT_*` を除去するが、`HOME` / `XDG_CONFIG_HOME` と repository-local config は意図的に継承する。product-owned Git call に `core.hooksPath=/dev/null`、`core.fsmonitor=false` 等を最高優先度で注入していない。

このため daemon/TUI/MCP が利用者の操作ではなく product effect として Git を呼ぶと、repository-local executable helper が usagi process 権限で起動する。

- `SystemSessionWorktreeIo::build_session_tree` → `git worktree add` は repository の `post-checkout` hook を実行する。fixture では hook が新しい worktree 内に `PWNED` を作成した。
- tracked `.gitattributes` と repository config の `filter.<driver>.smudge` も checkout 中に実行される。fixture では `git worktree add` だけで external smudge helper が marker を作成した。
- `IssueNumberSequence` の source discovery → `git ls-files` は repository の `core.fsmonitor` command を実行する。fixture では issue 操作のための列挙だけで marker が作成された。
- failing `post-checkout` は worktree と branch を既に作成した後に nonzero を返すため、session lifecycle は create failure と記録する一方で filesystem effect が残る。

inherited `GIT_CONFIG_COUNT` injection を消す #605 だけでは、repository 自身の config/hook は残る。root Agent の read-only Git では同じ helper 群を既に無効化しているが、product-owned `SystemGit` / issue authority には適用されていない。

## 修正方針

- `confined_git_command` に product-owned Git policy を一元化し、highest-precedence config/environment で少なくとも `core.hooksPath=/dev/null`、`core.fsmonitor=false`、`submodule.recurse=false`、pager/external diff/optional locks を無効化する。
- `git worktree add` の checkout filter は任意 driver 名を tracked `.gitattributes` から選べるため、既知 key の上書きだけを「全 helper 無効」と誤認しない。checkout/materialization は filter process を起動しない明示的な構築手順、または許可済み built-in だけを使う別境界へ分ける。
- diff 系は `--no-ext-diff --no-textconv` も明示し、将来の call site が policy を迂回しない構造にする。
- private remote clone に必要な SSH transport / credential helper の互換性は、実行を許可する範囲を明示して保持する。repository-local arbitrary command と credential transport を混同しない。
- worktree add failure後の partial effect は、hook無効化後も他のGit failureに備え、exact branch/worktree ownershipを証明して compensationするかdurable failed rowから安全にremoveできることを固定する。

## 受入条件

- [ ] repository-local `post-checkout` が `git worktree add` で実行されず、marker・任意write・partial failureを起こさない。
- [ ] tracked `.gitattributes` + repository-local `filter.<driver>.smudge/process` が session worktree作成で実行されない。
- [ ] repository-local `core.fsmonitor` が issue create/search/number allocationの `git ls-files` で実行されない。
- [ ] pager、external diff/textconv、submodule recursion、optional index refreshもproduct-owned callから起動しない。
- [ ] private remote cloneの既存transport契約を回帰させない。
- [ ] daemon session create/remove、TUI clone/diff、issue-number resolverの全real call siteが同じpolicyを通る。

## 再現

```text
git config core.fsmonitor <marker helper>
git ls-files ...
=> helper executed

.git/hooks/post-checkout writes marker
git worktree add ...
=> marker created in new worktree
```
