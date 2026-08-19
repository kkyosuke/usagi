---
number: 703
title: fix(test): 結合テストが起動した bootstrap-broker が reap されず常駐し続ける
status: todo
priority: medium
labels: [fix, test, daemon]
dependson: []
related: []
created_at: 2026-08-19T11:20:34.240449+00:00
updated_at: 2026-08-19T11:20:34.240449+00:00
---

## 症状

開発機で `usagi daemon bootstrap-broker` プロセスが 1000 件以上残留していた（観測時点で 1066 件、
最古は 1 日以上前）。cwd はいずれも `/private/var/folders/**/…-workspace` や
`/private/tmp/usagi-workspace-*` のテスト fixture で、`txt` は各 session worktree の
`target/debug/usagi` を掴んだままだった。

```
usagi 283 kyosuke cwd DIR /private/tmp/usagi-workspace-X05rfU
usagi 283 kyosuke txt REG .../.usagi/sessions/pr/target/debug/usagi
usagi 283 kyosuke  3u REG /private/tmp/usagi-RjgAJ6/daemon/bootstrap-broker-<key>.lock
usagi 283 kyosuke  4u unix /tmp/usagi-RjgAJ6/daemon/bootstrap-broker-<key>.sock
```

## 原因

- daemon は endpoint を publish するたびに `spawn_bootstrap_broker` で broker を別 process group へ
  detach する（`src/runtime/daemon.rs`）。broker は `serve_bootstrap_broker` の
  `listener.incoming()` で常駐し、自分で終わる契機を持たない。
- broker の単一化は `bootstrap-broker-<digest(workspace, exe)>.lock` の flock で行うため、
  **fixture ごとに data dir と workspace が違うテストでは互いに退かない**。テストの数だけ常駐する。
- 一方 `tests/support/daemon.rs` の teardown（`reap` / `reap_channel`）は
  `daemon/daemon.json` の pid + process-start identity を唯一の権威として reap するため、
  broker は対象外である。`daemon stop` の経路にも載らない。

残留した broker は worktree の実行ファイルを掴み続けるため、`session remove` の worktree 削除を
止める（[06-conventions.md#結合テストからの-daemon-起動](../../document/06-conventions.md) が
exact reap を要求している当の問題と同じ形）。

## 対応方針

`tests/support/daemon.rs` の fixture teardown を broker まで広げる。daemon 本体と同じく
「その fixture の data dir が記録した identity だけを撃つ」形に保ち、自プロセス上の fake は
撃たないこと。broker は `daemon.json` に載らないので、reap の権威になる記録
（broker socket の owner か、broker 自身が置く pid 記録）をどこに持たせるかを先に決める必要がある。

関連: #702（この残留を見つけた調査）
