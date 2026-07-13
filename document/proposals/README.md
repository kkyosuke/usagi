# 設計提案（proposals）

> [ドキュメント目次](../README.md)

`document/` 直下の番号付きドキュメント（`01-` …）は**現在のビルドで動作する仕様の正本**であり、
[06-conventions.md#記載実装済み](../06-conventions.md#記載実装済み) に従って未実装の内容を含めない。

一方、まだ実装されていない**構成・機構の設計判断**を記録したいことがある。これを spec に混ぜると
「どこまで本当か」が読者に判断できなくなるため、**設計提案はこの `proposals/` に分離**する。実装が進んで
挙動が確定したら、その内容を正本（`02-architecture.md` など）へ畳み込み、提案は撤去またはリンクだけ残す。
ロードマップ（実装タスク）は issue ストア（`.usagi/issues/`）で追跡する。

## 一覧

| # | ドキュメント | 内容 | 状態 |
|---|---|---|---|
| 2 | [02-ipc-id.md](02-ipc-id.md) | v2 daemon IPC の目標・権威・typed ID・fencing invariant | [04-ipc.md](../04-ipc.md) へ畳み込み済み |
| 3 | [03-ipc-protocol.md](03-ipc-protocol.md) | envelope、handshake、stream、idempotency、bounded transport、error | [04-ipc.md](../04-ipc.md) へ畳み込み済み |
| 4 | [04-daemon-api.md](04-daemon-api.md) | terminal/session command・event と socket/workspace/launch security | [04-ipc.md](../04-ipc.md) / [05-daemon.md](../05-daemon.md) へ畳み込み済み |
| 5 | [05-daemon-lifecycle.md](05-daemon-lifecycle.md) | active/draining restart、crash orphan、配置、実装 issue、test strategy | [05-daemon.md](../05-daemon.md) へ畳み込み済み |
| 7 | [07-pty-crash-continuation.md](07-pty-crash-continuation.md) | PTY broker／FD handoff による daemon crash 後の terminal 継続 | 提案（MVP 非依存） |
