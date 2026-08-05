---
number: 659
title: fix(cli): doctor と daemon status を実動診断・安全な復旧案内へ接続する
status: todo
priority: medium
labels: [review, stability, cli, tui, daemon, diagnostics, recovery]
dependson: [657]
related: [19, 67, 350, 515, 550, 645]
parent: 654
created_at: 2026-08-05T01:20:37.772297+00:00
updated_at: 2026-08-05T01:20:37.772297+00:00
---

## 症状

`README.md` は `usagi doctor` を「必要ツールの診断画面」と案内するが、v2 production は `BannerScreenRunner` で `usagi v…: doctor TUI` の1行を出して終了するだけで、診断を行わない。

`usagi daemon status` も `daemon.json` とexact process identityからrunning/stale/unverified/absentを分類するだけで、次を確認しない。

- current locator / generation registry / Unix socket handshake
- daemon IPCの応答性とbuild/workspace fence
- critical background workerの生存・劣化
- metrics freshness / terminal pipeline degradation
- error logの所在と、状態別の安全な復旧手順

そのため、PIDだけ生存する部分故障やhung daemonをoperatorが切り分けられず、必要以上に`--force` restartへ進む危険がある。

## 既存issueとの境界

- #19 / #67 は旧doctor/placeholder整理で完了済みだが、現行v2のbanner-only経路は解消していない。
- #350 はmacOS launchd supervisionとcrash後のdurable recovery。
- #657 はdaemon worker supervisionとworker healthの権威を実装する。本issueはその安全な観測・表示とoperator導線を担当する。
- stale cleanupの実行権威は既存daemon lifecycle/recoveryを再利用し、doctorが独自unlink/signalを行わない。

## 修正方針

- `doctor` をpureなcheck/result語彙と実IO portへ分離し、CLI/TUIのどちらからも同じ診断結果を表示できるようにする。
- `daemon status` は軽量なlifecycle statusを維持しつつ、少なくともIPC readinessまたはworker-health summaryをtypedに区別する。重い全診断をstatusへ詰めず、詳細はdoctorへ案内してよい。
- checkはread-onlyを既定とする。修復を追加する場合は、ownershipを証明できるstale artifactと既存lifecycle usecaseだけを対象にし、`--fix`等の明示操作を要求する。
- raw socket error、path、credential、PTY outputを画面へ出さず、安全なsummaryとerror/log locatorだけを表示する。

## 受け入れ条件

- `usagi doctor` がbannerではなく、最低限次を診断する: data directory/permission、daemon record/lock/locator/registry、exact owner、IPC handshake/readiness、workspace fence、worker health、metrics freshness、利用可能Agent CLI。
- healthy / absent / stale-owner-gone / PID-reused / unverified / hung socket / workspace mismatch / worker-degradedを区別する。
- 各状態について、破壊しない次の操作を具体的に示す。通常restartを先に案内し、live PTYを破棄する`--force`を無条件に勧めない。
- error logの実pathまたは安全な取得方法を示す。
- `daemon status` がPID aliveだけをhealthyと断定せず、IPC/worker healthが不明または異常ならその旨とdoctor導線を表示する。
- fake portのtable testとreal daemon subprocessのhealthy/hung/stale testを追加する。
- `--fix`を実装する場合、証明できないrecord/endpoint/workerを削除・signalしないfail-closed testを持つ。

## docs

`README.md`、`document/01-overview.md`、`document/05-daemon.md` を実装済み診断と復旧手順へ更新する。
