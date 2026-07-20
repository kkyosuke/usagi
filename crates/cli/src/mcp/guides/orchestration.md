# usagi orchestration ガイド

このガイドは MCP resource `usagi://guides/orchestration` として配信される、agent 向けの
session lifecycle 利用手順である。tool の名前・引数は `tools/list` のスキーマが正本である。

## 前提モデル

- **実行と session 状態の権威は daemon ただ 1 つ**。MCP server は起動時に daemon へ接続し、
  session の worktree や lifecycle state を自身では変更しない。
- **各 session は隔離された git worktree**で動く。git 追跡ファイルの変更はその worktree の
  ブランチに乗り、PR 経由で基点ブランチへ反映される。
- **root/coordinator は git 追跡ファイルを直接書かない**。実装や backlog の変更は対象 session
  の worktree で行う。

## 接続済みの操作

| 操作 | tool | durable effect |
|---|---|---|
| session 作成 | `session_create` | daemon が session を lifecycle store に記録し、git worktree を作る |
| session 破棄 | `session_remove` | daemon が worktree を破棄し、lifecycle store を更新する |
| legacy state の検査・採用 | `session_recover_legacy` | 既定は検査だけを行い、`apply: true` のときだけ daemon lifecycle state へ採用する |
| worker dispatch | `session_dispatch` | session を作成または再利用し、worker PTY と run/binding を durable に記録する |
| worker の観測 | `session_get` / `agent_list` / `agent_get` | dispatch store の agent と run を返す |
| worker の報告 | `agent_complete` / `agent_fail` | authenticated current run の報告を保存済み caller inbox へ配送する |
| caller の受信 | `agent_inbox` | authenticated caller 自身の durable inbox を返す |

dispatch/agent tool は daemon が Agent 起動時に注入した private caller credential を必要とする。手動で
起動した credential の無い `usagi mcp` からは利用できない。

## worker を dispatch する

`session_dispatch` は session 名と worker selector、prompt を受け取る。session は存在すれば再利用し、
無ければ作成する。新規 worker の runtime/model は `tools/list` の schema に列挙された組だけを使う。

```json
{"name":"session_dispatch","arguments":{"session":{"name":"issue-123"},
 "agent":{"runtime":"codex","model":"gpt-5"},"prompt":"issue #123 を実装する"}}
```

成功応答には durable `run_id`、`agent_id`、daemon-owned `terminal` が入る。既存 worker を再利用する場合は
`agent` を `{"id":"<agent_id>"}` にする。

## worker を観測する

`session_get {"name":"issue-123"}` は session 内の agent と現在または最後の task を返す。
`agent_list` は `session` / `status` で任意に絞り込み、`agent_get {"agent_id":"..."}` はその agent の
run 履歴を返す。これらは daemon の dispatch store を読み、MCP server のローカル推測を返さない。

## 完了または失敗を報告する

worker は終了前に `agent_complete` または `agent_fail` を呼ぶ。宛先は指定しない。daemon が credential の
current run と dispatch 時の binding を照合し、元 caller の inbox へ一度だけ配送する。

```json
{"name":"agent_complete","arguments":{"summary":"実装と検証が完了",
 "result":{"pr":"https://example.test/pr/123","commits":["abc123"],
 "changed_files":["src/example.rs"],"verification":"cargo test: green"}}}
```

`run_id` は通常省略する。指定した場合は authenticated current run と一致することの assertion として扱い、
別 run の選択には使わない。失敗時は `agent_fail` に `summary` と任意の `error` を渡す。

## caller inbox を読む

caller は `agent_inbox {"unread_only":true}` で自分宛ての報告を読む。`since` には RFC 3339 timestamp を
指定できる。worker が報告せず終了した場合も daemon が `no_report` を配送する。

## session を作成する

`session_create` は session 名を受け取り、daemon が lifecycle operation として処理する。

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"session_create","arguments":{"name":"mcp-guide"}}}
```

MCP 応答の text には daemon が受理した operation ID と revision が入る。worktree 作成と lifecycle
store の更新は daemon 内で同期的に完了してから応答する。同名 session や branch がすでにある場合は
エラーになり、別の session を作ったものとして扱わない。

## session を破棄する

`session_remove` は session 名を受け取る。未コミット変更のある worktree は `force: true` を明示しない
限り破棄しない。

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call",
 "params":{"name":"session_remove","arguments":{"name":"mcp-guide"}}}
```

`force` は変更を失う可能性があるため、dirty であることを別の信頼できる経路で確認し、破棄が意図された
場合だけ指定する。

## legacy state を扱う

`session_recover_legacy` は引数無し、または `apply: false` なら検査結果だけを返す。検査結果を確認した
うえで `apply: true` を呼ぶと、検証に通った legacy session 一式を daemon lifecycle state へ採用する。
通常の daemon restart や MCP 起動はこの採用を暗黙には行わない。

## 制約

- session tool は daemon を必要とする。daemon を autostart または接続できなければ MCP server は
  stdio serve を開始しない。
- session 名、worktree、branch の対応は daemon lifecycle state が権威を持つ。MCP server 側の cwd
  や名前だけを根拠に状態を補完しない。
- `session_remove` の `force` は dirty worktree の保護だけを明示的に解除する。他 session や repository
  root を削除対象へ広げない。
