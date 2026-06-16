# 開発ワークフロー

AI エージェントが `usagi` で作業する際の標準手順。**新規作業**と**追加修正**で手順が異なる。
コーディング・コミット・PR の規約は [document/06-conventions.md](../document/06-conventions.md) を参照。
ドキュメント全体の目次は [document/README.md](../document/README.md)。

## 新規作業（新しいタスクを始めるとき）

### 0. 着手する issue を選ぶ

実装すべきタスク（issue）は `issues/` 配下に `NNN-feature.md` 形式で管理されている。一覧と凡例・依存関係・推奨着手順は [issues/README.md](../issues/README.md) を参照。

- 各 issue の上部メタデータ（`status` / `priority` / `dependson`）を確認し、`dependson` が満たされている `todo` を選ぶ。
- 着手したら `status` を `in-progress`、完了したら `done` に更新する。
- worktree 名・ブランチ名は対象 issue の feature 名に合わせると対応がわかりやすい。

### 1. 開始時に worktree を作成する

タスクごとに git worktree を切って隔離環境で作業する。`main` を直接触らない。

```bash
git worktree add .claude/worktrees/<name> -b <type>/<説明>
cd .claude/worktrees/<name>
```

- ブランチ名は `<type>/<説明>` 形式（例: `feat/add-doctor-command`）。type は `feat|fix|docs|refactor|perf|test|build|ci|chore`。
- worktree のディレクトリ名はタスク内容がわかる短い名前にする。

### 2. 開発する

- クリーンアーキテクチャ（`domain → usecase → infrastructure ← presentation`）の依存方向を守る。
- 実装と同時にテストを追加・更新する（カバレッジ 100% を維持。CI でチェックされる）。
- コミット前に必ず以下を通す:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- コミットは [Conventional Commits](https://www.conventionalcommits.org/ja/) 形式（例: `feat: doctor コマンドを追加`）。

### 3. ドキュメントを更新する

実装内容に合わせて `document/` 配下を更新する。仕様・画面・データ構造に変更があれば対応するファイルを更新する。目次は [document/README.md](../document/README.md)。

**書き方のルールは [document/06-conventions.md#ドキュメント規約](../document/06-conventions.md#ドキュメント規約) に従う**（記載＝実装済み・SSoT・1 ファイル 1 トピック・表と図の活用・相対リンクとアンカーの整合）。

- `document/01-overview.md` — プロジェクト概要
- `document/02-architecture.md` — クリーンアーキテクチャ・`src/` のモジュール構成
- `document/03-commands/` — CLI / TUI 内コマンドのリファレンス（系統ごとに分割）
- `document/04-orchestration.md` — セッション・worktree オーケストレーション
- `document/05-settings.md` — 設定項目・保存場所・変更方法
- `document/06-conventions.md` — 開発規約
- `document/design/` — TUI 画面構成（画面ごとに分割）
- `document/data/` — `state.json` / `workspaces.json` / `settings.json` などの永続化仕様

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

新規作業と同じ。`cargo fmt` / `clippy` / `test` を通してからコミットする。

### 2. ドキュメントを更新する

追加した変更に合わせて `document/` および必要なら `README.md` を更新する。

### 3. PR のタイトル・概要を更新する

変更によって PR のスコープが変わった場合は、タイトルと本文を実態に合わせて更新する。

```bash
git push
gh pr edit <number> --title "<新しいタイトル>" --body "<更新した概要>"
```
