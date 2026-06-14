---
number: 003
feature: session
title: session コマンド（セッション管理）
status: done
priority: high
category: tui
dependson: [002]
ref: usagi.ai doc/app/tui/session.md
---

# `session` コマンド（セッション管理）

## 概要

セッション（作業単位）を作成・管理する TUI 内コマンドを実装します。usagi の worktree ベースワークフローの中心機能であり、`space` / `sync` / `finish` / `list` / `clean` / gh 連携など多くの機能がこのセッション概念に依存します。

usagi は worktree を **リポジトリ任意の場所ではなく、ワークスペースルート直下の `.usagi/worktree/<name>/` に集約** して管理します。これによりセッションの所在が一意に定まり、一覧・削除・クリーンアップが扱いやすくなります。

### git でないルートにも対応する（再帰的処理）

ワークスペースのルート自体が git リポジトリである必要はありません。セッション作成時にルートを**再帰的に走査**し、各エントリを次のように扱います。

- **git リポジトリのディレクトリ** → そのリポジトリの `git worktree` を `.usagi/worktree/<name>/<相対パス>/` に作成する。
- **git でないファイル・ディレクトリ** → `.usagi/worktree/<name>/<相対パス>/` へコピーする。

これにより、単一リポジトリだけでなく、ルートが git でない複数リポジトリ構成（モノレポ的なディレクトリツリー）にも対応できます。

```
/root                         （git でなくてもよい）
├── app-a/      = git    → app-a の worktree を作成
├── app-b/      = git    → app-b の worktree を作成
├── be/                  （git でない素のディレクトリ → 再帰）
│   └── be1/    = git    → be/be1 の worktree を作成
└── README.md            （git 管理外 → コピー）
```

セッション `feature-x` を作成すると、`.usagi/worktree/feature-x/` 配下にルートと同じディレクトリ構造が再現され、git 配下の各サブディレクトリはそれぞれ `feature-x` ブランチの worktree、それ以外はコピーになります。

## やること

- `session new <name>`：ルートを再帰的に走査し、`.usagi/worktree/<name>/` 配下に
  - git ディレクトリごとに新しいブランチを切って `git worktree` を作成
  - git 管理外のファイル・ディレクトリはコピー
  し、セッションを構築する。
- **name 未指定での起動**：`session new` を name 引数なしで実行した場合は、セッション名を入力する**モーダル**を画面中央に表示して name を尋ねる。入力確定（`Enter`）でその名前のセッションを作成し、`Esc` でキャンセルする。
  - モーダルは既存のモーダル基盤（`src/presentation/tui/widgets/` の `boxed` / `render_modal`、テキスト入力フィールド）を再利用し、ディレクトリ選択モーダルと同じく中央寄せ・枠付きボックスで描画する。
  - 空文字や既存セッションと重複する名前はバリデーションし、モーダル内にエラーを表示して確定させない。
- `session list`：現在のワークスペースのセッション一覧を表示する。
- `session remove <name>`：セッション（`.usagi/worktree/<name>/` 配下の全 worktree とブランチ、コピーしたファイル）を削除する。
- **name 未指定での起動**：`session remove` を name 引数なしで実行した場合は、記録済みセッションのチェックリストを画面中央の**モーダル**に表示する。`↑`/`↓`（または `j`/`k`）でカーソルを移動し、`Space` で選択/非選択を切り替え、`Enter` で選択した複数セッションを**一括削除**する（`Esc` でキャンセル）。`--force` を付けて開いた場合は破棄を伴って削除する。
- セッション情報（セッション名・作成時刻・ベースブランチ・配下の各リポジトリの worktree パス／ブランチ）を `.usagi/state.json` に永続化する。
- `.usagi/worktree/` は `.gitignore` 済みであることを前提とする（各リポジトリの worktree がワークスペースのコミット対象に混入しないようにする）。
- ワークスペース画面の worktree 一覧ペインにセッションを反映する。

## 完了条件

- `session new feature-x` で `.usagi/worktree/feature-x/` 配下に、ルート以下の各 git リポジトリの `feature-x` worktree が作成され、git 管理外ファイルがコピーされ、一覧に表示される。
- `session new` を name なしで実行するとモーダルが開き、名前を入力して `Enter` で同等のセッションが作成される（`Esc` でキャンセル、空・重複名はエラー表示）。
- ルートが git リポジトリでない複数リポジトリ構成（上記ツリー例）でも、各リポジトリごとに worktree が作られる。
- `session remove feature-x` で `.usagi/worktree/feature-x/` 配下の worktree・ブランチ・コピーが安全に削除される（未コミット変更がある場合は警告）。
- `session remove` を name なしで実行するとセッション一覧モーダルが開き、`Space` で複数選択して `Enter` で一括削除できる（`Esc` でキャンセル）。
- セッション状態が再起動後も `state.json` から復元される。

## 実装状況

- ✅ `session new <name>`：ルートを再帰的に走査し、git は `.usagi/worktree/<name>/` 配下へ worktree 作成・非 git はコピー（`usecase/session.rs`、`infrastructure/git.rs` の `add_worktree`）。
- ✅ name 省略時の名前入力モーダル（中央表示・`Enter` で作成・`Esc` でキャンセル・空／重複名のバリデーション。`home/state.rs` のモーダル状態と `home/ui.rs` の描画、`home/event.rs` のキー処理）。
- ✅ 単一リポジトリ構成では作成後に `state.json` を再同期し、worktree 一覧ペインへ反映。
- ✅ 複数リポジトリ構成での state.json への集約表現（`sessions` / `SessionRecord`）。ルートが git でなくてもセッションを追跡する。
- ✅ `session list`：state.json の `sessions` を一覧表示（件数 + 各セッション名 + worktree 数）。
- ✅ `session remove <name> [--force]`：各リポジトリの worktree とブランチを削除し、コピーしたファイルを掃除して `state.json` から除去（`usecase/session.rs` の `remove`、`infrastructure/git.rs` の `remove_worktree` / `delete_branch` / `has_uncommitted_changes`）。
- ✅ 未コミット変更がある場合の削除時警告：`remove` は dirty な worktree を検出すると削除せず警告し、`--force` 指定時のみ破棄する。
- ✅ name 省略時のセッション削除モーダル（`Effect::OpenRemoveModal` → `RemoveModal`）。`↑`/`↓`・`j`/`k` でカーソル移動、`Space` で選択/解除、`Enter` で選択した複数セッションを 1 件ずつ一括削除、`Esc` でキャンセル。一覧が枠を超える場合はカーソルが収まるようスクロールし隠れた件数を表示。状態は `home/state.rs`、描画は `home/ui.rs`、キー処理は `home/event.rs`。
