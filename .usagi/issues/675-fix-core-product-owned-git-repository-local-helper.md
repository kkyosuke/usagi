---
number: 675
title: fix(core): product-owned Git から repository-local helper 実行を無効化する
status: todo
priority: high
labels: [review, v2, core, daemon, git, security, integrity]
dependson: []
related: [605, 631]
created_at: 2026-08-13T22:28:26.087856+00:00
updated_at: 2026-08-23T23:22:34.965949+00:00
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

## 2026-08-24 時点の進捗（v3.0.0 リリースレビュー）

product-owned Git policy を `crates/core/src/infrastructure/git/environment.rs` の
`CONFINED_GIT_CONFIG` に一元化し、`confined_git_command` が最高優先度の
`GIT_CONFIG_COUNT` 系で注入するようにした。`confined_git_command` がこの workspace で
git command を組み立てる唯一の経路なので、全 call site が同じ policy を通る。

- [x] repository-local `post-checkout` が実行されない（`core.hooksPath=/dev/null`）
- [x] repository-local `core.fsmonitor` が `git ls-files` で実行されない（`core.fsmonitor=false`）
- [x] pager が起動しない（`core.pager=cat`）
- [x] submodule recursion が起きない（`submodule.recurse=false`）
- [x] optional index lock を取らない（`GIT_OPTIONAL_LOCKS=0`）
- [x] private remote clone の transport 契約を回帰させない（system/global config は
      意図的に残す。credential helper と SSH 設定は利用者のものを使う）
- [ ] tracked `.gitattributes` + `filter.<driver>.smudge/process` — **未対応**。
      driver 名を repository が任意に選べるため固定 key の上書きでは無効化できない。
      filter process を起動しない materialization 手順、または許可済み built-in だけを
      使う別境界が必要で、この issue に残る唯一のスコープはこれである。
- [ ] `git worktree add` 失敗後の partial effect の compensation
