---
number: 702
title: fix(agent): session Claude が毎回 first-run になり全 tool 呼び出しが EPERM で落ちる
status: done
priority: high
labels: [fix, agent, sandbox]
dependson: []
related: []
created_at: 2026-08-19T11:18:03.260521+00:00
updated_at: 2026-08-19T11:18:08.550405+00:00
---

## 症状

v2 で session の Claude を起動すると、

1. 起動ごとに onboarding（初回フロー）が走り、theme・permission mode（auto edit）・MCP 承認などの利用者設定が一切読み込まれない
2. すべての tool 呼び出しが `Error: EPERM: operation not permitted, mkdir '/private/tmp/claude-501/<cwd の slug>'` で失敗する
3. すべての hook が `agent phase report failed [unavailable]: daemon transport is unavailable` を返す

## 原因

いずれも「session launch の sandbox writable root を own worktree だけにした」ことに帰着する。

- **(2)**: Claude Code は tool 実行ごとに `$TMPDIR` を無視した固定 path `/tmp/claude-<uid>/<cwd の slug>` へ
  scratchpad を作る。session mode は `/tmp` を writable root に含めないため、seatbelt の `(deny file-write*)` が
  この `mkdir` を拒否する。`sandbox-exec -p` に同じ profile を渡して再現済み。
- **(1)**: 上記の代替として daemon が `CLAUDE_CONFIG_DIR` を `<worktree>/.usagi/claude`、`TMPDIR` を worktree 自体へ
  向けていた。session worktree は task ごとに新規作成されるため、Claude は毎回まっさらな config directory を
  掴み、onboarding からやり直しになる。認証情報も git worktree の中に書かれていた。
- **(3)**: lifecycle hook（`usagi agent-phase <phase>`）が `policy_client` を使い、cold-start 経路に入っていた。
  sandbox の writable root は data home を含まないため `bootstrap.lock` の取得が `PermissionDenied` になり、
  bootstrap broker が居ない環境ではそのまま `Unavailable` へ落ちる。tool 呼び出しごとに走る hook が
  bootstrap の遅延を払っていた点も誤り。

## 対応

- `usagi-core::usecase::claude_sandbox` の普遍領域（`$TMPDIR` / `/tmp` / `/var/tmp` / agent state / macOS Keychain・
  MDS cache）を **両 mode に同じだけ**与える。session と root を分けるのは repository への書き込み境界
  （起動固有 writable root と `protected_root`）であって、agent 自身の scratch / state / 認証領域ではない。
- daemon の session 起動から `CLAUDE_CONFIG_DIR` / `TMPDIR` の上書きを削除する。
- `agent-phase` / `codex-session-capture` hook を `attached_client`（bootstrap を取らず、動いている daemon へ
  attach するだけ）に切り替える。

## 確認

- `usagi claude-sandbox --mode session … -- <claude 名の program>` で、scratchpad の `mkdir`・`~/.claude`・
  Keychain が書けて、repository root / sibling session / data home が引き続き拒否されることを実機確認する。
