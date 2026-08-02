---
number: 632
title: fix(security): MCP caller credential を専用 child にだけ渡す
status: todo
priority: high
labels: [review, security, daemon, mcp, agent, credential]
dependson: []
related: [383, 457, 629, 630]
created_at: 2026-08-02T23:13:59.797212+00:00
updated_at: 2026-08-02T23:14:21.805625+00:00
---

## Finding（P0 security）

### 脅威モデルと対象

`USAGI_MCP_CALLER_CREDENTIAL` は同一UID内でも一つのlive Agent runtime / 正規MCP childだけを認証するbearer capabilityである。repository code、Agentが起動するbuild/test/shell child、terminal/provider outputはuntrustedとする。

`crates/daemon/src/usecase/runtime.rs::RuntimeCoordinator::launch_with_semantic` はcredentialを `SpawnProvision.daemon_environment` に追加し、`SpawnProvision::compose_environment` はそれをAgent provider本体の完全なspawn環境へ合成する。`crates/daemon/src/usecase/codex/mod.rs::mcp_arguments` は親環境からMCP childへ転送する。

### 発生条件・影響・根拠

Agent本体と通常childはcredentialを継承できる。`env`、build script、crash diagnostic等がstdout/stderrへ出すとPTY observerがbytesをjournal/replayへ保存する。漏れたtokenはruntimeがliveな間、別の `usagi mcp` processからreplayでき、当該Agentのdispatch、decision、completion/inbox権限として認証される。

credentialがdurable snapshotに無いことやexit/restartで失効することは、live中のexposureを防がない。

### effect-zero 条件

MCP wiringの無いlaunchではcredentialを発行しない。正規MCP child以外のprocess、別PID/sibling、forged token、runtime exit後、daemon restart後からのcallはmutation 0で拒否する。terminal journal、snapshot、log、error、argvにcredential bytesを残さない。

## 修正方針

- bearerをAgent本体envへ置かない。
- daemon-owned wrapper、専用MCP spawn provision、またはpeer-bound one-shot exchangeで正規MCP childだけへ渡す。
- tokenだけでなくlive runtime、MCP child process identity、generation/scopeを再検証する。
- credential redactionを後段output filterだけに依存させない。

## 必要な回帰テスト

- Agentが起動したuntrusted childのenvにcredentialが無い。
- childが全環境をstdoutへ出してもjournal/snapshot/replayにcredentialが無い。
- 正規Claude/Codex MCP childはdispatch/decisionを成功できる。
- sibling process、別PID、forged token、exit/restart後はeffect 0。
- secretがdurable state、argv、logs、safe errorへ出ない。

## 既存 issue との差分

#383 はruntime-fenced provenanceを導入しprivate provisionとterminal非露出を要求したが、現実装はAgent本体環境へcredentialを配る。#457 はprovider output redactionの関連境界であり、本issueはcredential delivery scopeを修正する。
