---
number: 728
title: "feat(mcp): Ollama の local LLM を third opinion tool として公開する"
status: done
priority: medium
labels: [feat, mcp, ollama, local-llm]
dependson: []
related: []
created_at: 2026-08-30T00:00:00+09:00
updated_at: 2026-08-30T00:00:00+09:00
---

## 目的

Ollama で動く local LLM に、usagi の MCP client から third opinion を質問できるようにする。

## 受け入れ条件

- [x] localhost の Ollama API を呼ぶ MCP tool を追加する。
- [x] model と質問を明示し、入力・応答サイズを hard bound する。
- [x] 未導入・未起動・不正応答を明確なエラーにする。
- [x] localhost 外へ接続しない。
- [x] README と MCP 正本ドキュメントに導入・利用手順を追加する。
