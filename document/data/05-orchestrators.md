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
- [整合性](#整合性)

## 配置

```text
.usagi/orchestrators/
├── .lock
├── claims.json
└── <plan-id>/
    ├── .lock
    ├── state.json
    └── events/<event-id>.json
```

## stamped envelope

`state.json` と `claims.json` は次の envelope を持つ。`revision` は CAS の比較値、`written_at` は
最後に永続化した UTC 時刻である。

```jsonc
{
  "format": "usagi-orchestrator",
  "version": 1,
  "revision": 4,
  "written_at": "2026-07-11T00:00:00Z",
  "value": {}
}
```

## plan と node

`value` は `id`、`owner`、`max_parallel` と issue number を key にした `nodes` を持つ。node は
`issue`、`dependencies`、`state`、`attempt`、`generation`、`lease`、`deadline`、`next_retry`、
`worker`、`base`、`pull_request` を保持する。時刻は RFC 3339 UTC、未確定値は `null` である。

state は `blocked`、`runnable`、`delegating`、`running`、`pr_open`、`review_wait`、`ci_wait`、
`ci_failed`、`retry_wait`、`merge_wait`、`merged`、`failed`、`cancelled` のいずれかである。

## claim

`claims.json` の `value.by_issue` は issue number ごとに `issue`、`plan`、`owner`、`generation`、
`lease { owner, expires_at }` を保存する。workspace の全 plan が同じ `.lock` を取って更新するため、
`(workspace, issue)` の active claim は一つだけである。期限切れだけでは takeover できず、呼び出し側が
session と PR の不在を再観測した場合だけ置換できる。

## event

event は `id`、`plan`、`issue`、`generation`、`kind`、`observed_at` を持つ。`kind` は
`pr_opened`、`succeeded`、`failed`、`interrupted`、`timed_out` のいずれかである。id は
`plan-issue-generation-kind` から決定的に生成し、plan lock 下の同名ファイル検査を重複排除点にする。

## 整合性

- snapshot は一時ファイルの fsync と rename による atomic write を行う。
- plan 更新は plan lock 下で期待 `revision` と現在値を比較する。相違時は書き込まない。
- claim 更新は workspace orchestrator lock 下で直列化する。
- event は append-only で、同じ lifecycle hook の再実行は no-op になる。
