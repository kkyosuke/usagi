# usagi orchestration ガイド

このガイドは MCP resource `usagi://guides/orchestration` として配信される、agent 向けの
orchestration（セッションの委譲・観測・完了報告）利用手順である。ここに出てくる tool は
すべて `tools/list` に載る実在の tool で、名前・引数はそちらのスキーマが正本である。

## 前提モデル

- **実行と session 状態の権威は daemon ただ 1 つ**。MCP の session 系 tool は daemon への
  リクエストであり、MCP サーバ自身は worktree 生成・prompt 配送・状態書き込みを行わない。
- **各セッションは隔離された git worktree**で動く。git 追跡ファイル（issue / ドキュメント /
  コード）の変更はその worktree のブランチに乗り、PR 経由で基点ブランチへ反映される。
- **root/coordinator は git 追跡ファイルを直接書かない**。backlog の変更や実装は必ず
  セッションへ委譲し、そのセッションの worktree・ブランチに載せる。

## 役割

| 役割 | すること | しないこと |
|---|---|---|
| root / coordinator | 着手可能な作業の選別、セッションへの委譲、進捗の観測 | git 追跡ファイル（issue 本文・status・コード）の直接編集 |
| セッション（委譲先） | 自 worktree での issue 化・実装・テスト・ドキュメント更新・PR、完了報告 | 他セッションの worktree への書き込み |

## tool サーフェス

orchestration は 3 種類の操作に分かれる。

| 操作 | tool | 用途 |
|---|---|---|
| dispatch（委譲） | `session_delegate_issue` | 既存の committed issue を新セッションに委譲して着手させる（issue → プロンプト → session 作成 → 起動時キュー投入を 1 tool で） |
| dispatch（委譲） | `session_delegate_brief` | 事前 issue の無い作業を、自由記述のブリーフからトリアージ/設計セッションとして起こす |
| dispatch（下位） | `session_create` / `session_prompt` | セッションの素の作成と、既存セッションへの追加指示。`delegate_*` はこれらを組み合わせた合成 tool |
| observe（観測） | `session_list` | 存在するセッションの軽量な一覧（daemon state の速いクエリ） |
| observe（観測） | `session_status` | 各セッションの進捗（agent の phase、worktree の status/dirty/merged） |
| observe（観測） | `session_pr` | セッションに紐づく PR とマージ状態 |
| complete（完了） | `session_complete` | 委譲先が親（または root）へ完了を報告する（自セッション内限定） |
| lifecycle | `session_remove` | 不要になったセッション（worktree）を破棄する（dirty があれば force が必要） |

セッション内限定の作業補助（`session_note_*` / `session_todo_*` / `session_decision_*`）は
自セッションのメモ・チェックリスト・意思決定ログで、orchestration の観測対象ではない。

## ワークフロー

### 起源フロー（事前 issue が無いとき）

```text
root                         daemon                   session（委譲先の worktree）
 │ session_delegate_brief    │                        │
 │─────────────────────────► │ worktree 生成・queue    │
 │                           │──────起動時キュー──────► │ agent 起動・ブリーフを受信
 │                           │                        │ issue_create（backlog 化）
 │ session_status（観測）      │◄─────────────────────  │ 実装・テスト・docs・PR
 │ session_pr（マージ確認）     │                        │ session_complete（完了報告）
```

1. root が `session_delegate_brief` でブリーフを渡す。
2. 委譲先セッションが自 worktree で `issue_create` し、実装・テスト・ドキュメント更新・PR まで進める。
3. root は `session_status` / `session_pr` で観測し、`session_complete` の報告とマージを待つ。

### issue ベースのフロー（committed issue があるとき）

1. root が `issue_search`（`ready: true`）で、依存が満たされ生存セッションの無い `todo` を選ぶ。
2. `session_delegate_issue` で番号を指定して委譲する。
3. 委譲先が自枝で status を `in-progress` → `done` と進め、`done` を PR に載せてマージする。

## 代表例

`session_delegate_brief`（`tools/call`）:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"session_delegate_brief",
           "arguments":{"brief":"MCP の resource 導線を改善する","name":"mcp-guide"}}}
```

進捗の観測（`session_status`）:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call",
 "params":{"name":"session_status","arguments":{}}}
```

## 状態遷移

セッションと issue はそれぞれ次の状態を持つ。

```text
セッション:  作成 ──► 稼働（agent phase が進む）──► 完了報告 ──► 破棄
issue:       todo ──► in-progress ──────────────► done（PR マージで基点へ反映）
```

- セッションの生存（`session_list` / `session_status`）は「その issue が in-progress である」
  実効シグナルになる。ローカルな issue の `in-progress` は当該セッション内の表現で、基点へは
  PR マージ後（＝完了後）に遅れて届くため、root はセッションの生存で進捗を判断する。
- `done`（基点へのマージ）は `session_status` の merged / `session_pr` で検知する。
- **委譲先は PR を開く前に issue を `done` にして同じ PR に載せる**。マージ後にセッションを
  破棄すると誰も `done` を立て直せない（root は status を書かない）。

## 制約・前提

- session 系 tool は daemon を必要とする。daemon を autospawn できない環境ではエラーを返し、
  daemon を迂回する直接実行のフォールバックは持たない（書き手の一本化を優先する）。
- status（issue）の書き手はその issue を担当するセッションだけ（単一書き手）。root/main の
  チェックアウトからの issue 書き込みは拒否される。
- `session_complete` / `session_note_*` / `session_todo_*` / `session_decision_*` は自セッション内
  からのみ呼べる。
- `session_remove` は dirty な worktree に対しては force を必要とする。
