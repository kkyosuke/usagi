# 開発ワークフロー

AI エージェントが `usagi` で作業する際の標準手順。**新規作業**と**追加修正**で手順が異なる。
コーディング・コミット・PR の規約は [document/06-conventions.md](../document/06-conventions.md) を参照。
The documentation index is [document/README.md](../document/README.md).

## 実装依頼の完了条件

ユーザーから実装を依頼された場合、ローカルでの実装やコミットだけでは完了とせず、Draft PR の作成から
サブエージェントレビュー、必要な修正、CI の確認まで行う。質問への回答、調査・設計だけの依頼は対象外とする。
この節が AI エージェントによる実装依頼の提出・レビュー手順の正本である。
レビューサイクルは 1 件の実装依頼または追加修正ごとに数え、PR 作成後または追加修正の push 後の
初回レビューを 1 回目とする。レビュー指摘による修正の push は同じサイクルに属し、回数をリセットしない。
既存 PR への追加修正では、PR が Ready for review なら `gh pr ready <number> --undo` で Draft に戻してから
修正を push し、更新後のレビューと CI が完了するまで Draft を保つ。

```text
実装・ローカル検証 → Draft PR 作成 → サブエージェントレビュー・修正（最大 3 回） → CI 確認 → Ready for review
                                      └──────── 指摘があれば commit・push ────────┘
```

- Draft PR を作成したら、サブエージェントへ PR の差分レビューと、妥当な指摘に対する修正・検証を委譲する。
  レビューでは正しさ、回帰、テスト、ドキュメント、規約への適合を確認する。
- このレビューには実行中の coding agent が提供する組み込みサブエージェントを使う。
  レビュー必須という規約を、usagi の `agent_handoff` / `session_dispatch` で別の Agent を起動する指示と
  解釈しない。usagi 上の別 Agent にレビューさせるのは利用者がその分担を明示した場合だけとする。
  組み込みサブエージェントが利用できない場合は自己レビューの結果と独立レビュー未実施を報告し、PR は Draft に保つ。
- 修正が入った場合は親エージェントが差分を確認して commit・push し、更新後の差分を再レビューさせる。
  初回を含めて最大 3 回まで繰り返し、マージを妨げる指摘がなくなればレビューを終了する。
- 3 回のレビュー後もマージを妨げる未解決事項が残る場合は、マージ可能とみなさず、PR を Draft のまま保って
  未解決事項をユーザーへ報告する。
- ベースブランチの最新状態を反映した差分についてレビューを終え、必須 CI がすべて green、
  マージ競合なし、マージを妨げる未解決事項なしを確認してから
  PR を Ready for review にする。ここまでを「マージできるレベル」の完了条件とする。

## 新規作業（新しいタスクを始めるとき）

### 0. 作業を委譲する

- **root/コーディネータは git 追跡ファイルを直接書かない**。実装・設計・ドキュメントの変更は対象 session の worktree で行う。
  新しい作業は root が `session_delegate_brief`（または `session_dispatch`）で session を起こして委譲し、その session が
  worktree 内で変更して PR に載せる。
- root は委譲先 session が生存しているか（`session_list` / `session_status`）を進捗の実効シグナルとし、
  `session_status.merged` / `session_pr` で基点へのマージを検知する。生存 session がある作業は二重に委譲しない。
- worktree 名・ブランチ名は作業内容に合わせると対応がわかりやすい。

### 1. 隔離された作業環境を用意する

`main` を直接触らず、隔離された作業ツリーで進める。**ただし環境によって手順が異なる**。

- **usagi セッション内で起動している場合**（TUI の `agent` / `terminal` command が起動する worktree。
  カレントが `.usagi/sessions/<name>/` 配下）: **すでに隔離された worktree 内にいるので、新しく
  worktree を作成しない。そのまま作業を進める**。作業ブランチは `usagi/<name>`（セッション名 `<name>` を `usagi/` 名前空間に収めたもの）。
  Session and worktree lifecycle state is owned by the daemon.
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
- 実装と同時にテストを追加・更新する（カバレッジ 100% を維持。最終的には CI でチェックされる）。
- 検証は [06-conventions.md#品質チェック（リスク比例の gate）](../document/06-conventions.md#品質チェックリスク比例の-gate)
  を正本とする。編集中は fmt check / `cargo check --workspace --all-targets` / 変更 module・target と直接 consumer の test、
  commit 前は `cargo clippy --workspace --all-targets -- -D warnings` と risk-based selected tests を回す。
  **clippy / full test / coverage 100% の重い full gate はローカル push では走らせず、PR CI に一本化する**
  （pre-push フックはこの gate を持たない）。ローカルで先取り確認したい場合は coverage 経路を任意で実行してよく、
  coverage がテスト実行を兼ねる経路では `cargo test --workspace --quiet` を重複実行しない。
- docs-only（Rust 差分なし）は Rust gate を省略できるが、`ruby scripts/ci/docs-ssot-lint.rb` と
  Markdown link check は必須である。
- AI エージェントの完了報告には、実行した command、結果、未実行 gate と理由、[full test / coverage gate
  必須条件](../document/06-conventions.md#品質チェックリスク比例の-gate)への該当有無を含める。

- コミットは [Conventional Commits](https://www.conventionalcommits.org/ja/) 形式（例: `feat: doctor コマンドを追加`）。

### 3. ドキュメントを更新する

実装内容に合わせて `document/` 配下（v2 の正本）を更新する。仕様・構成に変更があれば対応するファイルを更新する。目次は [document/README.md](../document/README.md)。

**書き方のルールは [document/06-conventions.md#ドキュメント規約](../document/06-conventions.md#ドキュメント規約) に従う**（記載＝実装済み・SSoT・1 ファイル 1 トピック・表と図の活用・相対リンクとアンカーの整合）。

- `document/01-overview.md` - project overview
- `document/02-architecture.md` — workspace 構成・クレート責務・依存ルール
- `document/06-conventions.md` — 開発規約


ユーザー向けの変更があれば `README.md` も更新する。

### 4. PR を作成する

```bash
git push origin HEAD:refs/heads/<branch>
gh pr create --head <branch> --draft --title "<type>: <説明>" --body "<概要>"
# 上記「実装依頼の完了条件」をすべて満たしたら:
gh pr ready <number>
```

- PR タイトルも Conventional Commits 形式に合わせる。
- 本文には「目的 / 変更内容 / テスト・確認方法」を含める。
- **PR は Draft で開き、CI の必須チェックが green になってから Ready for review にする**。ローカル push では重い full gate を
  走らせないため、最終的な full gate の green は CI で確認する。CI が落ちたら Draft のまま修正して push し直す
  （正本は [06-conventions.md#プルリクエスト](../document/06-conventions.md#プルリクエスト)）。
- PR 作成後は [実装依頼の完了条件](#実装依頼の完了条件) に従い、サブエージェントレビューと必要な修正を完了する。

---

## 追加修正（既存 PR に変更を重ねるとき）

すでに PR を出しているタスクへ追加の修正を入れる場合は、worktree とブランチをそのまま使い、以下を回す。

### 1. 開発する

新規作業と同じ。[品質チェック](../document/06-conventions.md#品質チェックリスク比例の-gate)の該当 gate を通してからコミットする。

### 2. ドキュメントを更新する

追加した変更に合わせて `document/` および必要なら `README.md` を更新する。

### 3. PR を Draft に戻し、タイトル・概要を更新する

Draft に戻す条件と手順は [実装依頼の完了条件](#実装依頼の完了条件) を正本とする。
変更によって PR のスコープが変わった場合は、タイトルと本文を実態に合わせて更新する。

```bash
git push origin HEAD:refs/heads/<branch>
gh pr edit <number> --title "<新しいタイトル>" --body "<更新した概要>"
```

push 後は [実装依頼の完了条件](#実装依頼の完了条件) のレビューサイクルを改めて実行し、更新後の PR が
マージできるレベルであることを確認する。
