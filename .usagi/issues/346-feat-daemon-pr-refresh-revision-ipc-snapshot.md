---
number: 346
title: feat(daemon): PR refresh と revision 付き IPC snapshot を提供する
status: done
priority: high
labels: [daemon, ipc, pr]
dependson: [345]
related: [198]
created_at: 2026-07-17T23:44:47.737212+00:00
updated_at: 2026-07-18T00:44:47.842395+00:00
---

## 目的

#345 の daemon-owned PR inventory を、shell を経由しない低優先度の `gh pr view` refresh と、revision 付き IPC snapshot / dedicated subscription に接続する。TUI rendering と既定ブラウザ起動は含めない。

## 範囲

- daemon に PR refresh scheduler/worker を追加する。
  - `gh` は argv 固定の process port（例: `gh pr view <canonical-url> --json title,state`）から起動し、shell、文字列結合 command、stdin credential 注入を使わない。
  - low-priority bounded concurrency、timeout、jitter を持ち、failure/missing executable/rate limit は exponential backoff で retry する。
  - `gh` が無い・失敗する場合も URL inventory と IPC snapshot は利用可能に保つ。エラーを TUI へ生の stderr/credential として渡さない。
  - 成功時に title と state（OPEN/CLOSED/MERGED）だけを reducer 経由で更新する。dismiss/pin の user-owned metadata は auto refresh より常に優先する。
- core client/IPC vocabulary に PR snapshot request と subscribe/unsubscribe を追加する。
  - snapshot は stable session identity、monotonic revision、canonical URL、title optional、state、user metadata、refresh 状態を含む。
  - `pr.updated` は専用 subscription のヒント event とし、event が欠落・重複・順序逆転しても client は revision を指定して snapshot を再取得できる。snapshot を正本とする。
  - request/response/event は handshake capability と protocol negotiation に載せ、connection cleanup/backpressure/resync を既存 stream 規約と整合させる。
- daemon presentation adapter と client adapter を接続し、inventory mutation（新規検出、refresh 成功、pin/dismiss の更新）で revision と event を一貫して扱う。
- TUI data model/画面/BrowserOpener は #347 に残す。

## 依存関係

- #345 に依存する。
- #347 は本 issue の snapshot/subscription contract に依存する。
- 旧 TUI watcher の #198 は完了済みであり、実装の移植元ではなく refresh/backoff の参考に限る。

## 受け入れ条件

- canonical URL 1 件ごとに argv process port へ 1 refresh job を渡し、shell は一切起動しない。固定 fake runner で argv と timeout/cap を検証できる。
- `gh` が不在・timeout・非ゼロ・不正 JSON の場合、検出済み URL と既存 title/state を失わず、retry は backoff を守る。
- OPEN → CLOSED / MERGED と title 更新が inventory revision を 1 回だけ進め、pinned/dismissed entry は refresh で変化しない。
- snapshot は指定 session/revision で取得でき、`pr.updated` は変化した session と新 revision を示す。event 欠落後でも snapshot 再読で収束できる。
- subscription 解除・接続切断時に server-side subscriber が回収され、slow client は daemon refresh/terminal processing を停止させない。
- protocol schema、daemon usecase、IPC adapter、client decoder の unit/integration tests を追加する。

## テスト観点

- fake clock/runner: due 判定、backoff、jitter、concurrency、dedupe、success/failure。
- reducer: Closed/Merged/title、pin/dismiss precedence、no-op revision。
- IPC: capability negotiation、snapshot round-trip、event hint、reconnect/resync、unsubscribe/disconnect/backpressure。
- daemon integration: output-detected inventory → refresh result → snapshot/event の一連。

## 非目標

- PR modal/sidebar/toast/クリック処理と既定ブラウザ起動は #347。
- `gh auth` のセットアップや repository remote からの PR 推測は対象外。
