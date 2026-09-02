# 提案: v2 IPC envelope／transport protocol

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [IPC／ID overview](02-ipc-id.md) ｜ 次へ → [daemon API](04-daemon-api.md)

> **Status:** 採用済みの設計履歴
>
> **Baseline:** 原版 commit `785fec57eba04e7e3cd294ac8ab92bc4772c5e90`（2026-07-12）。本文は IPC protocol を導入した時点の snapshot であり、現在仕様ではない。現行仕様は [daemon IPC](../04-ipc.md) を参照する。

この提案で定めた実装済みの frame、handshake、envelope、idempotency、transport、error は
[4. daemon IPC](../04-ipc.md) を正本とする。
