# 提案: v2 daemon terminal／session API と security

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [IPC protocol](03-ipc-protocol.md) ｜ 次へ → [daemon lifecycle](05-daemon-lifecycle.md)

この提案で定めた実装済みの terminal ownership、session lifecycle、operation、socket security は
[4. daemon IPC](../04-ipc.md) と [5. daemon](../05-daemon.md) を正本とする。

agent runtime の phase ingest、MCP injection、resume / reclaim は
[2. アーキテクチャの Agent orchestration の fence](../02-architecture.md#agent-orchestration-の-fence) を正本とする。
公開 daemon API は product 固有の hook payload、MCP config、credential、phase token、rendered argv を受け付けず、
これらは product adapter の scoped provisioner 内だけに存在する。crash 時の PTY master-fd continuation と unknown
ownership の自動 kill / replacement spawn はこの API の対象外である。
