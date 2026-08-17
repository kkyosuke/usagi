---
number: 671
title: backlog: origin/main c09dddcd v2 全体コードレビュー
status: done
priority: high
labels: [review, v2, backlog, epic]
dependson: []
related: [672, 673, 675, 676, 677, 678, 679, 680, 686]
created_at: 2026-08-13T00:02:18.580332+00:00
updated_at: 2026-08-16T23:34:22.162498+00:00
---

## レビュー基点

- reviewed commit: `c09dddcd61198124791e6707ac86d5b72d8dec8a`
- reviewed at: `2026-08-17`
- 対象: usagi v2 全体（tracked `crates/*/src/**/*.rs` + `src/**/*.rs`: 310 files / 約236,618 physical lines。tests・examples・scripts・CI・configを含む監査集合: 379 files / 約259,230 physical lines）
- 観点: 正しさ、resource bound、durability、process/PTY lifecycle、authority/fence、IPC/MCP schema、TUI reducer/input/rendering/worker、install/CI

件数はreview commitのtreeから数え、作業ブランチ上の#672修正やissue本文は含めない。レビュー基点は日付ではなく上記commit hashで固定する。

## 確認領域

| 領域 | 主な確認内容 |
|---|---|
| core | domain/state machine、durable stores、env resolver、git、IPC codec、VT/checkpoint |
| daemon | lifecycle、generation authority、resource allocator、PTY/output、worker shutdown、supervisor、PR refresh |
| TUI | reducer、input ownership、live terminal、render/frame diff、background pumps、platform helpers |
| CLI / MCP | argv、tool registry/schema、caller credential、dispatch/supervisor/decision route |
| scripts / CI / install | test recommendation、coverage exclusion、required contexts、installer/update |

## 最新main増分の再レビュー

最初の全体監査後に入った `1ef8a5cd..c09dddcd` は6コミットに限定されるため、同じ全tree走査を繰り返さず、変更されたGarden、daemon retirement、shared terminal geometry、E2E / CI規約を差分監査した。

- #1492でautomatic narrow抑止、resize edge close、frame由来hitboxによるclick-to-Closeupを確認した。
- Garden overlayがordinary character / pasteのPTY forwardingと背景pane mutationを遮断することを確認した。
- #686は解消済み部分を除き、**manual narrowのinvisible overlay** と **wheel / control chord / raw input等がwake-up ownerへ届かない問題**へscopeを縮約した。
- #1494でGardenのうさぎをagent runtime単位へ変更し、#687が完了した。session hitboxとinput routingは変更されていないため、#686のmanual narrow / wake-up ownership残件は継続する。
- #1495でGardenのpose / reduced motion / selected marker / safe failure summaryを実装し、#688が完了した。Garden input routingは変更されていないため、#686の残件は継続する。
- #1490でclient workerのsocket readにretirement flag + bounded `poll(2)` readinessを追加し、通常のframe readでparkしたworkerを`shutdown(2)`の起こしだけに依存せずjoinできることを確認した。
- #1490は#673の同期decision待ち自体を変更していない。`wait_for_user_decision`はsocket readへ戻らずstore pollingを続けるため、client disconnect / daemon shutdown / generation retirementを観測しない残件はそのままである。
- #1489で複数TUIが共有するterminal geometryをdaemon権威のsmallest viewportへ一本化し、attach / resize / resume / detachのrevision fenceを更新した。`op read`、decision wait、PR refresh、clipboard、Gardenのforeground routingは変更されておらず、既存findingへの影響はない。
- 重いE2Eのcross-process lock、transient transport retry、dropped-keystroke再送、overlay close観測はtest偽陽性と診断を改善する変更で、既存findingを解消または新規findingを追加するものではない。

## Finding 対応表

| priority | issue | invariant |
|---|---:|---|
| high | #675 | product-owned Git が repository-local hook / fsmonitor / smudge 等の helper を実行しない |
| high | #673 | pending user decision 待機が client worker / shutdown / generation retirement を塞がない |
| high | #672 | `op read` の stdout / stderr retained memory を stream ごとの hard cap 内に保つ |
| high | #676 | dispatch registry / inbox を count・byte・age bound、pagination、ack/GC と backpressure で bounded にする |
| high | #677 | user decision の入力・履歴・pending admission を hard bound と retention で bounded にする |
| medium | #678 | supervisor store / scheduler history を query・snapshot・journal・runtime metadata 全体で bounded にする |
| medium | #679 | PR refresh の `gh` child を bounded output と process-group cleanup で完了・回収する |
| medium | #680 | system clipboard helper の wait を deadline / cleanup 付きにする |
| medium | #686 | manual narrow Gardenと全user-activityのforeground wake-up ownershipを一致させる |

## 確定した根拠

- `confined_git_command` は inherited `GIT_*` を除去する一方、repository-local configを読む。実Git fixtureで`git worktree add`が`post-checkout`、issue source discoveryの`git ls-files`が`core.fsmonitor`、tracked `.gitattributes`のcheckoutが任意名smudge helperを実行した。failing hookはworktree / branch作成後にnonzeroを返しpartial effectを残した。
- `wait_for_user_decision` は期限なし`Pending`を25 msごとのstore readで待ち、client disconnect / daemon shutdown / generation retirementを観測しない。#1490はsocket frame readのretirementをboundedにしたが、このhandlerはsocket readへ戻らないため、accept loopの全client worker joinを塞ぐ残件は変わらない。
- review commitの`env_resolver`はbinding 128、secret 32、並列child 4、30秒deadlineを持つ一方、stdout / stderr readerが`read_to_end`でbyte上限を持たない。本PRの#672修正はstreamごと64 KiB retained cap、overflow後のEOF drain、safe failure、reader cleanupを実装する。
- `store/dispatch.rs`はregistryとinboxを全件read-modify-atomic-rewriteしretention/GCがない。production `agent_inbox`は`since` / `unread_only`のみでlimit/cursor/ackがなく、`mark_inbox_read`はproduction callerがない。
- `store/user_decision.rs`はterminal decisionを削除せず全stateをmutationごとに置換する。decision field / option countにもdomain hard boundがない。
- `store/supervisor.rs::events`はpage指定前にjournal全件を読む。journalだけでなくsnapshotの`applied_events`、schedulerのstart/wake reservations、terminal run自体にもretentionがない。
- `src/runtime/daemon.rs::GhProcess`は5秒timeout後にparentをkill/reapするがstdout byte capとprocess group ownershipを持たない。
- `src/runtime/clipboard.rs`はTUI render thread上でclipboard childのstdin writeと`wait()`を無期限に行い、helperがhangすると入力・描画もhangする。
- latest Gardenはautomatic narrow / resize / clickを修正したが、手動`garden`はgeometryを見ずoverlayを開く。wheelはpane-control interceptor、Ctrl-C / Ctrl-Qはoverlay control chord、Ctrl-D / raw passthrough / terminal copyは`app_event_from_key(None)`で消費または脱落し、Gardenのwake-up reducerへ届かない。

## 問題なし／既存対策を確認した事項

- daemon generation authority、global allocator、terminal/Agent retention、PTY output pipeline、critical worker shutdownは既存issueのfence/bound/GCと回帰testを確認した。
- TUI live terminalのbounded scrollback、viewport、restore/reconnect、background pumpは既存修正とtestを確認した。
- CLI/MCPのtyped argv、caller credential、tool registry validation、1 MiB frame/IPC boundを確認した。
- `coverage-off` registry lint、test recommendation map、required contexts、installer checksum/version verificationの契約を確認した。
- sidebarのrepository-local diff helper実行疑いはproductionと同じcommandで再現せず、findingにしない。
- latest Garden overlay中のordinary key / pasteはlive PTYへ漏れず、covered pane controlsも背景paneを変更しない。残件は#686のwake-up semanticsに限定する。

## 完了条件

- 最優先の`op read` output bound（#672）を同じPRで実装し、issueを`done`にする。
- 残るfindingを独立した追跡可能な子issueとして起票する。
- fmt/check/clippy、risk-based selected tests、workspace full test、Markdown link check、PR CI required contextsを確認する。
