# 開発ワークフロー

AI エージェントが `usagi` で作業する際の標準手順。**新規作業**と**追加修正**で手順が異なる。
コーディング・コミット・PR の規約は [document/06-conventions.md](../document/06-conventions.md) を参照。
ドキュメント全体の目次は [document/README.md](../document/README.md)（v1 時点の仕様は [v1/document/README.md](../v1/document/README.md)）。

## 新規作業（新しいタスクを始めるとき）

### 0. 着手する issue を選ぶ

実装すべきタスク（issue）は usagi の issue ストア（`.usagi/issues/`）に `NNN-feature.md` 形式で管理されている。`usagi issue list` / `usagi issue show <番号>`（CLI）や MCP ツール（`issue_search` / `issue_get`）で一覧・参照する。

- 各 issue のメタデータ（`status` / `priority` / `dependson`）を確認し、`dependson` が満たされている `todo` を選ぶ。
- **root/コーディネータは issue の定義（起票・本文編集）も行わない**。issue ファイルは git 追跡下なので、root が
  `issue_create` / `issue_update` / `issue_delete` を実行すると `main` のチェックアウトを汚してしまうため、MCP は root
  での書き込み系 issue tool を拒否する。新しい作業を issue 化したい場合、root は `session_delegate_brief` で
  トリアージ/設計 session を起こし、その session が worktree 内で `issue_create` して PR に載せる。既存の committed
  issue を遂行する場合だけ、root は `session_delegate_issue` で `issue-<番号>` session に委譲する。
- **`status` の書き手はその issue を担当する session だけ**（「単一書き手」）。root/コーディネータは `status` を一切書かない。`main`（リポジトリルート）で root が `status` を触ると、その差分が並行する session の PR と分岐・衝突するためである。`status` を書くのは常にその session の枝だけ、という書き手の一本化で衝突を防ぐ。
- **status ライフサイクルは自枝でこう回す**（`usagi issue update <番号> --status ...` または MCP `issue_update`。すべてその issue を担当する session の worktree 内で行う。issue の書き込みは worktree に routing され、ブランチに乗って PR で `main` へ反映される）:

  | 遷移 | いつ | どこで |
  |---|---|---|
  | `todo` → `in-progress` | 着手時 | 自枝（この session の worktree） |
  | `in-progress` → `done` | **PR を開く前** | 自枝。status 差分を実装差分と**同じブランチ・同じ PR**に載せる（別コミットでよい） |

  `done` を反映できるのは当該 session の枝だけで、PR がマージされて初めて `main` の issue が `done` になる。**マージ後に `session_remove` されると誰も `done` を立て直せない**（root は原則 `status` を書かない）ため、必ず PR を開く前に `done` のコミットを PR に含めること。#104 が `main` にマージ済みなのに `todo` に取り残されたのは、この「PR 前 done」を欠いたためである。
- **root は `status` を書かずに進捗を判定する**。issue ファイルの `in-progress` は当該 session 内のローカルな進捗表現で `main` には遅れて届く（マージ後＝実際は完了後）ため当てにしない。代わりに root は「その issue の session が生存しているか」（`session_list` / `session_status`。命名規約 `issue-<番号>`）を **in-progress の実効シグナル**とし、`session_status.merged` / `session_pr` で **`done`（基点へのマージ）** を検知する。root は「`main` で `todo` かつ生存 session が無い issue」だけを ready 候補として委譲する（二重委譲を避ける）。
- worktree 名・ブランチ名は対象 issue の内容に合わせると対応がわかりやすい。

### 1. 隔離された作業環境を用意する

`main` を直接触らず、隔離された作業ツリーで進める。**ただし環境によって手順が異なる**。

- **usagi セッション内で起動している場合**（`usagi agent` / `terminal` が起動する worktree。
  カレントが `.usagi/sessions/<name>/` 配下）: **すでに隔離された worktree 内にいるので、新しく
  worktree を作成しない。そのまま作業を進める**。作業ブランチは `usagi/<name>`（セッション名 `<name>` を `usagi/` 名前空間に収めたもの）。
  セッションと worktree の構築は [04-orchestration.md](../v1/document/04-orchestration.md) が正本。
- **リポジトリのルート（`main` のチェックアウト）で直接作業している場合**: 自分で worktree を切って隔離する。

  ```bash
  git worktree add .claude/worktrees/<name> -b <type>/<説明>
  cd .claude/worktrees/<name>
  ```

  - ブランチ名は `<type>/<説明>` 形式（例: `feat/add-doctor-command`）。type は `feat|fix|docs|refactor|perf|test|build|ci|chore`。
  - worktree のディレクトリ名はタスク内容がわかる短い名前にする。

> 迷ったら `git rev-parse --show-toplevel` で現在地を確認する。`.usagi/sessions/<name>` を指していれば
> セッション内なので worktree は作らない。

### 2. 開発する

- クリーンアーキテクチャ（`domain → usecase → infrastructure ← presentation`）の依存方向を守る。
- 実装と同時にテストを追加・更新する（カバレッジ 99% 以上を維持。CI / pre-push でチェックされる）。
- 検証は [06-conventions.md#品質チェック（リスク比例の gate）](../document/06-conventions.md#品質チェックリスク比例の-gate)
  を正本とする。編集中は fmt check / `cargo check --workspace --all-targets` / 変更 module・target と直接 consumer の test、
  commit 前は `cargo clippy --workspace --all-targets -- -D warnings` と risk-based selected tests、push / PR 前は Rust 差分に
  full test + coverage 99% を通す。coverage がテスト実行を兼ねる経路では `cargo test --workspace --quiet` を重複実行しない。
- docs-only（Rust 差分なし）は Rust gate を省略できるが、Markdown link check は必須である。
- AI エージェントの完了報告には、実行した command、結果、未実行 gate と理由、[full test / coverage gate
  必須条件](../document/06-conventions.md#品質チェックリスク比例の-gate)への該当有無を含める。

- コミットは [Conventional Commits](https://www.conventionalcommits.org/ja/) 形式（例: `feat: doctor コマンドを追加`）。

### 3. ドキュメントを更新する

実装内容に合わせて `document/` 配下（v2 の正本）を更新する。仕様・構成に変更があれば対応するファイルを更新する。目次は [document/README.md](../document/README.md)。

**書き方のルールは [document/06-conventions.md#ドキュメント規約](../document/06-conventions.md#ドキュメント規約) に従う**（記載＝実装済み・SSoT・1 ファイル 1 トピック・表と図の活用・相対リンクとアンカーの整合）。

- `document/01-overview.md` — プロジェクト概要（v2 の位置づけ・v1 との関係）
- `document/02-architecture.md` — workspace 構成・クレート責務・依存ルール
- `document/06-conventions.md` — 開発規約

v1 時点の仕様（コマンド・画面・データ構造・orchestration）は `v1/document/`（退避版）にあり、更新しない。

ユーザー向けの変更があれば `README.md` も更新する。

### 4. PR を作成する

```bash
git push -u origin <branch>
gh pr create --title "<type>: <説明>" --body "<概要>"
```

- PR タイトルも Conventional Commits 形式に合わせる。
- 本文には「目的 / 変更内容 / テスト・確認方法」を含める。

---

## 追加修正（既存 PR に変更を重ねるとき）

すでに PR を出しているタスクへ追加の修正を入れる場合は、worktree とブランチをそのまま使い、以下を回す。

### 1. 開発する

新規作業と同じ。[品質チェック](../document/06-conventions.md#品質チェックリスク比例の-gate)の該当 gate を通してからコミットする。

### 2. ドキュメントを更新する

追加した変更に合わせて `document/` および必要なら `README.md` を更新する。

### 3. PR のタイトル・概要を更新する

変更によって PR のスコープが変わった場合は、タイトルと本文を実態に合わせて更新する。

```bash
git push
gh pr edit <number> --title "<新しいタイトル>" --body "<更新した概要>"
```
