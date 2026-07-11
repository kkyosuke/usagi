---
number: 196
title: perf(orchestration): live prompt queue に総量上限と retry backoff を導入する
status: todo
priority: medium
labels: [perf, orchestration, mcp, review]
dependson: []
related: [136, 184]
created_at: 2026-07-11T01:30:35.617474+00:00
updated_at: 2026-07-11T02:45:02Z
---

## 背景

`session_prompt(mode=live)` の各promptには128KiB上限があるが、worktree単位queueの件数・総bytesには上限がない。consumerが不在の間は `Vec<String>` をJSONへ保存し、appendごとに全queueをread-modify-writeする。

pane-less sessionのautostartはlaunch queueとlive queueを全件joinし、agentのshell command argvへ埋め込む。macOSの `ARG_MAX` は1,048,576 bytesであり、最大prompt 8件だけでraw payloadが上限へ達する。実際にはenv・agent config・shell escapingも同じ上限を使うため、より少ない件数でspawnが失敗する。

失敗時は巨大な結合promptをlaunch queueへ戻し、次tickで同じspawnを再試行するため、disk I/O・env解決・error logを繰り返し得る。

## 方針

- worktree単位でqueue件数と総bytesを有界化し、超過をMCP tool errorとして返す。
- 1ファイル全件rewriteではなく、append-only item/logまたは個別item fileへ変更する。
- delivery batchに件数・bytes上限を設け、順序と一度だけ配送を維持する。
- 大きなopening promptはshell argvではなく、起動後stdin・安全な一時file/fd等で渡す。
- retryには指数backoff、attempt上限、next retry、最後のerrorを保持する。恒久失敗はdead-letter/要対応状態にする。
- queue形式変更にはversion/migration方針を含める。

## 受け入れ条件

- queueのdisk/memory使用量に文書化された上限がある。
- acceptedなpromptをsilent dropしない。
- aggregate payloadがOS argv上限へ到達しない。
- 恒久spawn failureでhome loopが毎tick再試行しない。
- concurrent append/drain/requeueでも順序とexactly-once境界を維持する。

## テスト

- 件数・bytes境界、migration、concurrent writers。
- batch分割と順序。
- fake恒久failureでbackoff回数をclock駆動確認。
- oversized aggregateをargvへ渡さない回帰テスト。
