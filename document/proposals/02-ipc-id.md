# 提案: v2 daemon IPC／ID overview と identity

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ 次へ → [IPC protocol](03-ipc-protocol.md)

> **Status:** 採用済みの設計履歴
>
> **Baseline:** 原版 commit `785fec57eba04e7e3cd294ac8ab92bc4772c5e90`（2026-07-12）。本文は IPC identity と fencing を導入した時点の snapshot であり、現在仕様ではない。現行仕様は [daemon IPC](../04-ipc.md) と [daemon](../05-daemon.md) を参照する。

この提案で定めた実装済みの identity、fencing、daemon authority は
[4. daemon IPC](../04-ipc.md) と [5. daemon](../05-daemon.md) を正本とする。
