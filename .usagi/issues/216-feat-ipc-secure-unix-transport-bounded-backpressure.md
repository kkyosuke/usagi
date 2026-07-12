---
number: 216
title: feat(ipc): secure Unix transport と bounded backpressure を実装する
status: todo
priority: high
labels: [ipc, security, daemon]
dependson: [215]
related: []
parent: 213
created_at: 2026-07-12T11:38:43.903832+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

protocol core を実 Unix domain socket の client/server へ配線し、同一 UID、登録 workspace、bounded resource の境界を daemon 入口で強制する。設計は [security boundary](../../document/proposals/04-daemon-api.md#security-boundary) と [backpressure](../../document/proposals/03-ipc-protocol.md#bounded-resource-と-backpressure) を正本とする。

## 対象

- owner-only daemon directory `0700`、generation endpoint socket `0600`、symlink／owner 検査と atomic bind。
- Linux `SO_PEERCRED`／macOS `getpeereid` 相当の peer UID 検証。取得不能は fail-closed。
- current generation locator と active/draining endpoint discovery。
- handshake timeout、in-flight request 上限、client outbound queue、terminal input queue、output backlog の byte/frame 上限。
- response/control と terminal output の queue 分離、slow client eviction／`resync_required`、fair scheduling。
- nonblocking socket の frame ごとの partial-write buffer／offset と control response 用の予約容量。
- surface-neutral client connection port は core、TUI attach state machine は TUI、socket accept/connect 実 IO は合成ルートに分離。

## 受け入れ条件

- socket mode だけに依存せず peer credential を検証し、別 UID を request decode 前に切断する。
- slow client が PTY drain、他 client、control ACK を停止させない。欠落 output は黙って飛ばさず resync へ遷移する。
- partial write後の`WouldBlock`でframeを破損せず、mutation済みresponseを送れない場合もOperationIdから照会できる。
- 上限超過は side effect 前に structured `resource_exhausted`／`backpressure` を返す。
- disconnect は subscription だけを解放し、terminal／accepted operation を kill/cancel しない。
- fake transport と実 Unix socket で permission、handshake、multiplex、partial write、slow consumer、reconnect を検証する。
