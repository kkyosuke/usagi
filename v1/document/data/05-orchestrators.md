# orchestrator plan・claim・event

> [データ永続化の目次](README.md) ｜ ← 前へ [メモリ](04-memory.md)

workspace-local な durable orchestrator の保存形式は本書が正本である。ファイルは git 管理外の
`<repo>/.usagi/orchestrators/` に置く。

## 目次

- [配置](#配置)
- [stamped envelope](#stamped-envelope)
- [plan と node](#plan-と-node)
- [claim](#claim)
- [event](#event)
- [TUI reconcile](#tui-reconcile)
- [整合性](#整合性)

## 配置

```text
.usagi/orchestrators/
├── .lock
├── claims.json
└── <plan-id>/
    ├── .lock
    ├── state.json
    ├── events/<event-id>.json
    └── rejected-events/<event-id>.json
```

## stamped envelope

`state.json` と `claims.json` は次の envelope を持つ。`state.json` は version 1、workspace 識別子を
必須にした `claims.json` は version 2 である。`revision` は CAS の比較値、`written_at` は最後に永続化した
UTC 時刻である。format と version が一致しないファイル、または必須 field を欠く旧 claim は曖昧な状態として
fail-closed し、自動 dispatch しない。

```jsonc
{
  "format": "usagi-orchestrator",
  "version": 2,
  "revision": 4,
  "written_at": "2026-07-11T00:00:00Z",
  "value": {}
}
```

## plan と node

`value` は `id`、`owner`、`max_parallel` と issue number を key にした `nodes` を持つ。node は
`issue`、`dependencies`、`state`、`attempt`、`generation`、`process`、`retired_generations`、`lease`、
`deadline`、`next_retry`、`worker`、`base`、`pull_request` を保持する。`process` は generation 固有の
`credential` と `starting` / `active` / `stop_requested` / `retired` / `unknown` の liveness state を持つ。
時刻は RFC 3339 UTC、未確定値は `null` である。

state は `blocked`、`runnable`、`delegating`、`running`、`pr_open`、`review_wait`、`ci_wait`、
`ci_failed`、`retry_wait`、`merge_wait`、`merged`、`failed`、`cancelled` のいずれかである。

## claim

`claims.json` の `value.by_issue` は issue number ごとに canonical workspace path の `workspace`、`issue`、
`plan`、`owner`、`generation`、`lease { owner, expires_at }` を保存する。claim key は
`(canonical workspace path, issue)` である。workspace の全 plan が同じ `.lock` を取って更新するため、同じ
workspace/issue の active claim は一つだけであり、別 workspace の同番号 issue は別 authority になる。

delegate action は worker session、binding、prompt、owner wake-up を作る前に claim を atomic に取得する。競合した
action はこれらの副作用を起こさず、node を `runnable` に戻す。tick outcome の `busy` は競合 claim の owner、plan、
generation、lease を保持し、TUI の reconcile 通知は `busy claims` 件数を表示する。

worker の `succeeded` / `failed` / `interrupted` / `timed_out` event、node の `cancelled` / `failed` / `merged`、
dispatch 失敗では、plan・owner・generation が現在の claim と一致する場合だけ release する。retry は generation を
増やして新しい claim を取得するため、前 generation の遅延 release が新 owner を解放しない。

lease 期限切れだけでは takeover できない。候補 coordinator は保存済み claim と一致する worker binding、および同じ
plan/issue/generation の未 merge PR がないことを再観測し、その観測対象 claim が lock 取得時にも一致した場合だけ
reclaim する。owner が claim 後・spawn 前に crash した場合、期限までは busy のまま保持し、期限後の不在再観測と
次の reconcile で新 generation を dispatch する。live session または未 merge PR があれば期限後も reclaim しない。

## event

event は `id`、`plan`、`issue`、`generation`、`credential`、`kind`、`terminal_revision`、`observed_at` を持つ。`kind` は
`pr_opened`、`succeeded`、`failed`、`interrupted`、`timed_out` のいずれかである。id は
`plan-issue-generation-kind-terminal_revision` から決定的に生成し、plan lock 下の同名ファイル検査を重複排除点にする。
worker worktree の `.usagi/orchestrator-worker.json` は次に起動する process の active binding である。generation ごとの
immutable binding は `.usagi/orchestrator-workers/<credential>.json` に保存し、Agent process は起動時に credential を
環境へ取り込む。lifecycle hook は active binding を event provenance に使わず、process が保持する credential から
generation を解決する。credential のない旧 process は generation 不明として発行され、active generation とは扱わない。

lifecycle hook は event を先に保存し、owner に live pane があれば live queue、なければ launch queue を wake-up として送る。
queue は event の正本ではなく、失敗しても event は残る。現行 node と credential / generation が一致しない event、旧形式の
credential がない event、terminal node への遅延 event は plan を変更せず `rejected-events/` へ理由と拒否時刻を付けて移す。

## TUI reconcile

TUI home の起動時、通常 idle tick、没入中の autostart tick は `<repo>/.usagi/orchestrators/*/state.json`
を列挙し、各 plan を reconcile する。reconcile は保存済み event、worker binding、session の cached status を
観測し、CAS で plan を保存してから claim admission を行う。取得できた delegate action だけを worker dispatch へ渡し、
event による release と ack は plan の永続化後に行う。

delegate action は worker session を `owner-issue-N` 形式で作り、`started_from` に plan owner を記録する。
worker の `.usagi/orchestrator-worker.json` を書き、issue prompt を worker launch queue に入れる。owner が
終了済みまたは不在で action がある場合は、owner worktree の launch queue に集約 wake-up prompt を冪等に置く。

worker dispatch は `min(plan.max_parallel の空き, autostart_queued_prompt_limit の空き)` だけ進める。
global agent 枠が埋まっていると runnable node は runnable のまま残り、後続 tick で枠が空いてから delegate される。
`retry_wait` は `next_retry` まで worker 枠を使わず、`deadline` を過ぎた `delegating` / `running` は
`retry_wait` へ移る。retry 時は旧 Agent process を kill し、backend が reap を確認して process state を `retired` として
保存した後の tick だけが generation を増やして新 process を起動する。kill 失敗、複数 Agent pane、process 不明のいずれも
新 spawn を行わない。`cancelled` も同じ retirement fence を使うが後続 generation は作らない。
`review_wait`、`merge_wait`、`pr_open` は worker 枠に数えない。

daemon / TUI restart 後は phase と pid-stamped live-pane marker を照合する。終了を確認できれば `retired`、live pane を確認できれば
`active`、どちらも確認できなければ `unknown` とする。旧 `process` 欠落 record と mutable binding は `unknown` に移行し、
active と推定して上書きしない。

## 整合性

- snapshot は一時ファイルの fsync と rename による atomic write を行う。
- plan 更新は plan lock 下で期待 `revision` と現在値を比較する。相違時は書き込まない。
- claim 更新は workspace orchestrator lock 下で直列化する。
- event は append-only で、同じ lifecycle hook の再実行は no-op になる。
- reconcile は plan を永続化した後に event を ack（削除）する。ack 前の再起動では同じ event を再適用できる。
- stale / unknown event は plan 保存後に durable rejection ledger へ移し、新 generation の state を変更しない。
- active process の retirement を plan に保存するまで、retry / cancel は同じ worktree へ別 process を起動しない。
