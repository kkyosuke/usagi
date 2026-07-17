---
number: 334
title: docs(proposals): dispatch/decision proposal の issue 番号参照を監査する
status: done
priority: medium
labels: []
dependson: []
related: []
created_at: 2026-07-17T22:45:48.827285+00:00
updated_at: 2026-07-17T22:45:54.189132+00:00
---

## 目的

main 取り込み後の `document/proposals/`（`08-agent-dispatch-mcp.md`・`09-user-decision-mcp.md`・`README.md` など）に含まれる issue 番号参照が、実際の `.usagi/issues/` の issue と正しく対応しているかを監査する。特に、MCP 利用導線改善タスクが一時 `#323` を使っていたが main 側の別 `#323`（session_dispatch 実装 issue）と衝突し `#333` へ振り直した経緯があるため、proposals の `#323` が session_dispatch を正しく指しているかを確認する。

## やること

- `grep -rnE '#[0-9]+' document/proposals/` で全 issue 番号参照を洗い出す。
- 各参照先 `.usagi/issues/<番号>-*.md` のタイトル・内容が参照文脈と一致するか確認する。
- 齟齬（存在しない番号・別テーマ・陳腐化）があれば修正する。`#323` が session_dispatch を指すものは正しいので変更しない。
- 全て正しければ no-op で、監査結果を issue と PR に記録する。

## スコープ外

- 重複番号（165/166/182/201/268/270/302/303）自体の解消は並行セッション `issue-number-dedup` の担当。
- MCP resource 実装（PR #1024）には触れない。

## 監査結果（2026-07-18）

`document/proposals/` の全 issue 番号参照を洗い出し、各参照先 issue の実タイトルと突き合わせた。**全参照が整合し、修正不要（no-op）**。

| 参照箇所 | 番号 | 参照文脈 | 参照先 issue 実タイトル | 判定 |
|---|---|---|---|---|
| README.md:30 / 08:11 | #321–#323, #331–#332 | agent dispatch 実装 issue（下記個別） | — | ✅ |
| 08:239,256 | #321 | core: dispatch durable store | feat(core): agent dispatch の durable ドメイン型と store を追加する | ✅ |
| 08:242,257 | #322 | daemon: launch runtime へ接続・binding・報告なし検知 | feat(daemon): agent dispatch を launch runtime へ接続し caller↔worker binding と「報告なし」検知を実装する | ✅ |
| 08:245,258 | #323 | mcp: session_dispatch / *_get / *_list / agent_complete\|fail / inbox | feat(mcp): session_dispatch / session_get / agent_list / agent_get / agent_complete / agent_fail / agent_inbox を実装する | ✅（session_dispatch。背景通り正しい） |
| 08:248,259 | #331 | mcp: runtime/model allowlist schema snapshot | feat(mcp): workspace allowlist から runtime/model schema snapshot を生成する | ✅ |
| 08:251,260 | #332 | daemon: launch 前再検証 | feat(daemon): MCP agent dispatch で runtime/model allowlist を再検証する | ✅ |
| 08:95,300 | #146 | agent capability / model allowlist 語彙 | refactor(orchestration): session agent override 検証を Agent capability に接続する | ✅ |
| 08:223 | #110 | session_delegate_issue 基点コミット検証 | fix(mcp): session_delegate_issue は issue が委譲先の基点ブランチにコミット済みか検証する | ✅ |
| 08:129 | #1234 | `pr?` フィールドの例文字列（`例 "#1234" or URL`） | — | 対象外（issue 参照でない） |
| 09:6 / README.md:31 | #329–#330 | user decision 実装 task | #329 feat(daemon): user decision request を durable state と inbox 配送へ接続 / #330 feat(tui): user decision modal と pending 一覧 | ✅ |
| 07-pty-crash-continuation.md:102 | #209 / #220 | MVP cutover 前提 | #209 feat(daemon): live terminal generation rollover と orphan safety / #220 feat(clients): TUI／CLI／MCP を v2 daemon IPC へ cutover | ✅ |

### 補足

- proposals から重複番号（165/166/182/201/268/270/302/303）への参照は無く、`issue-number-dedup` セッションのスコープと干渉しない。
- `#1234` は `session_dispatch` の返却型 `pr?` の例示文字列であり、issue 番号参照ではないため対象外。
- Rust 差分なし。issue ファイル（`.usagi/issues/`）以外の Markdown 差分はなく、`document/` は無変更のため lychee のリンクチェック対象に変更は入らない。

## 結論

齟齬なし。proposals の issue 番号参照はすべて正しく、`#323` は session_dispatch 実装 issue を正しく指している。ドキュメント修正は行わない（no-op）。
