---
number: 326
title: feat(daemon): completion event で supervisor を wake/restart する durable scheduler loop を実装する
status: done
priority: high
labels: [daemon, orchestration, supervisor, scheduler, agent]
dependson: [322, 325]
related: [219, 268, 283, 323]
parent: 324
created_at: 2026-07-17T21:12:03.425066+00:00
updated_at: 2026-07-17T23:08:21.243228+00:00
---

## 目的

#325 の durable supervisor state を daemon runtime に接続し、dispatch completion/failure/NoReport と inbox を入力として task DAG を進める event-driven scheduler loop を実装する。親 agent が停止しても daemon が次の判断 request を durable に用意し、必要な parent agent を安全に wake/restart する。

## やること

- daemon composition root が single shared SupervisorRuntime を所有し、restart 時に supervisor store、dispatch run/binding/inbox の cursor を load/reconcile する。IPC connection や client process ごとに scheduler を複製しない。
- #322/#323 が durable commit する Completed / Failed / NoReport と dispatch lifecycle を、cursor + correlation/provenance fence で SupervisorEvent に ingest する。少なくとも一回配送、duplicate、out-of-order、daemon restart 中の event を収束させる。
- reducer が Ready task を返した時だけ session_dispatch 相当の daemon usecase を一度だけ実行する。effect reservation を durable に記録してから dispatch し、response loss/retry/restart で二重 worker launch しない。
- child terminal event 後、parent task を AwaitingDecision にし、保存済み caller provenance の parent agent へ「supervisor decision required」wake request を作る。parent runtime が終了・未接続なら stable session/agent scope を再解決して restart/launch し、同じ decision generation を重複送信しない。
- wake された parent の判断入力は、完了 child の structured result、安全な failure/no-report、DAG snapshot、remaining budget summary、decision generation を含む。worker が自由文で session_prompt する経路に依存しない。
- scheduler tick は event arrival、deadline/retry timer、startup reconcile、explicit wake request で起動する。idle busy-poll を置かず、各 tick は有限の deterministic actions を出し commit/effect を分ける。
- fake clock/store/dispatch runtime で integration test を追加する。

## 受け入れ条件

- completion/failure/NoReport が一度以上届けば、同じ child run で parent wake と next decision が一度だけ決まり、重複 event や restart で二重 dispatch しない。
- parent agent の停止、daemon restart、wake ACK loss、child の late completion、同名 session の再作成を provenance/generation fence で安全に処理する。
- scheduler は dependency を満たした Ready task だけを dispatch し、未解決の parent 判断を勝手に生成・実行しない。
- daemon process が共有 runtime の唯一の writer であり、MCP client disconnect は run/scheduler を停止しない。
- integration test は dispatch→completion→parent wake→decision→next dispatch と failure/no-report/restart/duplicate の各経路を deterministic に検証し、coverage 100% を維持する。

## 非目標

- 予算、並列上限、retry 回数、cancel/escalation、artifact verification の policy 内容（#327）。
- supervisor API の MCP exposure と既存 session_* の互換ドキュメント（#328）。
- TUI 画面。
