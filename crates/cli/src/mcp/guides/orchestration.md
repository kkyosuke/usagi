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

## 接続済みの orchestration 操作

| 操作 | tool | durable effect |
|---|---|---|
| session 作成 | `session_create` | daemon が session を lifecycle store に記録し、git worktree を作る |
| 一覧・進捗観測 | `session_list` / `session_status` | lifecycle snapshot と agent phase・worktree の dirty/merged を返す |
| 追加指示 | `session_prompt` | live Agent PTY または durable next-launch queue へ prompt を配送する |
| 委譲 | `session_delegate_issue` / `session_delegate_brief` | session 作成と prompt queue 投入を不可分に行う |
| PR 観測 | `session_pr` | daemon-owned PR inventory と merged 集約を返す |
| 完了報告 | `session_complete` | 呼び出し元 session を credential から復元し root coordinator へ報告する |
| scratchpad | `session_note_*` / `session_todo_*` / `session_decision_*` | 呼び出し元 session worktree の machine-local store を操作する |
| session 破棄 | `session_remove` | daemon が worktree を破棄し、lifecycle store を更新する |
| legacy state の検査・採用 | `session_recover_legacy` | 既定は検査だけを行い、`apply: true` のときだけ daemon lifecycle state へ採用する |

## observe と prompt

`session_list` は durable session identity の軽量一覧、`session_status` は Git 観測を含む詳細一覧である。
coordinator は session の生存を `session_status`、成果の統合を `session_pr` の `merged` で判定する。

`session_prompt` の `mode` は `auto`（既定）/ `queue` / `live` である。`auto` は live Agent が
あれば PTY へ送り、無ければ daemon の durable next-launch queue に保存する。`queue` は live Agent が
いると配送されない prompt を作らないためエラーになり、`live` は live Agent が無ければエラーになる。

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call",
 "params":{"name":"session_prompt","arguments":{"name":"issue-403","prompt":"追加の回帰テストも固定してください"}}}
```

## delegate

committed issue は `session_delegate_issue`、事前 issue の無い依頼は `session_delegate_brief` を使う。
どちらも daemon が worktree を作成した後、同じ session identity の queue へ初回 prompt を保存してから
成功を返す。途中で session 作成が失敗した場合は queue 投入を行わない。

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call",
 "params":{"name":"session_delegate_brief","arguments":{"name":"triage-cache","brief":"cache invalidation を調査する"}}}
```

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
