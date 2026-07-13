# 提案: v2 daemon lifecycle／配置／実装計画

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [daemon API](04-daemon-api.md) ｜ 次へ → [PTY crash continuation](07-pty-crash-continuation.md)

この文書は daemon lifecycle の実装前提案として使われていた履歴 stub である。実装済みの
daemon process lifecycle と配置は [2. アーキテクチャ](../02-architecture.md)、IPC transport は
[4. daemon IPC](../04-ipc.md)、authority・terminal ownership・generation rollover・orphan safety は
[5. daemon](../05-daemon.md) を正本とする。

未実装の計画・acceptance 条件はここへ残さない。将来の作業候補と依存関係は
[issue store](../../.usagi/issues/) で管理する。
