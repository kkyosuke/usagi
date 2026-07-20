# 3. タスク issue（`issues/`）

> [データ永続化トップ](README.md) ｜ ← 前へ [2. workspace 毎（リポジトリ単位）](02-workspace.md) ｜ 次へ → [4. メモリ（`memory/`）](04-memory.md)

プロジェクトのタスクを管理する issue の保存フォーマットです。`<repo>/.usagi/` の他のファイルと異なり
**git で共有**され、チームで同じタスク一覧を見られます。`infrastructure/issue_store.rs` の `IssueStore` が
管理します。操作する CLI / MCP は [3.1 CLI コマンド](../03-commands/01-cli.md#usagi-issue) /
[3.3 MCP サーバ](../03-commands/03-mcp.md) を参照してください。

## 目次

- [保存場所](#保存場所)
- [issue ファイル（`NNN-<slug>.md`）](#issue-ファイルnnn-slugmd)
- [採番（ワークスペース横断）](#採番ワークスペース横断)
- [番号 identity と重複修復](#番号-identity-と重複修復)
- [`index.json`（派生キャッシュ）](#indexjson派生キャッシュ)
- [依存関係の解決（着手可能な issue）](#依存関係の解決着手可能な-issue)

## 保存場所

```
<repo>/.usagi/issues/
├── 001-add-doctor-command.md   # 1 issue = 1 ファイル
├── 002-fix-login.md
├── index.json                  # 派生キャッシュ（git 管理外）
└── .lock                       # プロセス間書き込みロック（git 管理外）
```

issue ファイル（`NNN-*.md`）は git 追跡対象、`index.json` は再生成可能なので `.usagi/.gitignore` で除外します
（[02-workspace.md#保存場所](02-workspace.md#保存場所)）。

`<repo>` は **操作したカレントの worktree のルート**です。セッション worktree
（`.usagi/sessions/<name>/`。[04-orchestration.md](../04-orchestration.md)）内で操作した issue は、
ワークスペースルートではなく**そのセッション自身の `.usagi/issues/` に書かれ**、セッションのブランチに乗って
PR 経由で `main` に流れます。これによりセッションの issue 変更がワークスペースのチェックアウトを未コミットで
汚しません（同じ仕組みは memory も同様）。採番だけは worktree をまたいで一意にする必要があるため、後述のとおり
ワークスペース全体を横断して決めます。

## issue ファイル（`NNN-<slug>.md`）

上部に **frontmatter**（行ベースのメタデータ）、その下に自由記述の markdown 本文を持ちます。ファイル名は
`番号(3桁ゼロ詰め)-タイトルのスラッグ.md`。

```markdown
---
number: 1
title: doctor コマンドを追加
status: todo
priority: medium
labels: [cli, infra]
dependson: [2, 3]
related: [5]
parent: 4
milestone: v1
created_at: 2026-06-14T00:00:00+00:00
updated_at: 2026-06-14T00:00:00+00:00
---

# doctor コマンドを追加

本文（markdown 自由記述）。
```

| フィールド | 型 | 意味 |
|---|---|---|
| `number` | u32 | 採番された一意な番号（ファイル名の接頭辞と一致）。新規作成時に「ワークスペース全体（ルート + 全セッション worktree）の最大値 + 1」で振る（[採番](#採番ワークスペース横断)） |
| `title` | string | タイトル |
| `status` | enum | `todo` / `in-progress` / `done` |
| `priority` | enum | `high` / `medium` / `low`（既定 `medium`） |
| `labels` | array&lt;string&gt; | 任意のラベル |
| `dependson` | array&lt;u32&gt; | 先に `done` になっている必要がある issue 番号（ブロックする先行条件） |
| `related` | array&lt;u32&gt; | 関連する issue 番号（ブロックしない緩いリンク） |
| `parent` | u32? | 親 issue 番号（Epic ⊃ サブタスクの階層）。`dependson`（先行条件）とは別概念 |
| `milestone` | string? | 束ねるマイルストーン名 |
| `created_at` / `updated_at` | RFC3339(UTC) | 作成・更新日時 |

- `parent` / `milestone` は値があるときだけ frontmatter に出力します（未設定の issue には行自体が現れません）。`labels` / `dependson` / `related` は空でも `[]` を書きます。
- frontmatter は `serde_yaml` 不採用の方針に合わせ、既知フィールドを対象にした軽量パーサで読み書きします。未知のキーは無視するので、フォーマットを後方互換に拡張できます。
- 書き込みはアトミック（一時ファイル + `rename`）。タイトル変更でスラッグが変わった場合は、**先に新しいファイルを書いてから**同じ番号の旧ファイルを削除して 1 issue = 1 ファイルを保ちます（順序が逆だと書き込みの途中でクラッシュした場合に issue の実体ファイルが消えうるため）。
- 同一ストアに対する read-modify-write（旧ファイル削除、`index.json` の更新）は、ストアごとの `.lock` ファイルに対するプロセス間排他ロック（advisory lock）で直列化します。採番は下記の workspace-global authority で別に直列化します。
- source Markdown の write/remove 前に `.derived-dirty` を durable に記録し、source の commit point と派生 `index.json` の更新を分離します。source commit 後に index 更新が失敗しても操作は成功として返り、marker を残したまま次の read または process reopen 後の read が source から index を再生成します。source write が失敗した場合は issue の identity を変更しません。同一 create request の再送は既に commit 済みの内容を照合して同じ番号を返し、update/remove の再送は issue 番号に対して冪等です。

## 採番（ワークスペース横断）

新規 issue の `number` は、**ワークスペース内のすべての issue ストアを横断した最大値と、予約済み high-water mark の大きい方 + 1** で決めます。
対象は次の 2 種類です。

```
<workspace>/.usagi/issues/                      # ルート（main のチェックアウト）
<workspace>/.usagi/sessions/<name>/.usagi/issues/  # 各セッション worktree
```

issue がセッション worktree ごとに書かれる（[保存場所](#保存場所)）ため、自ストアだけを見て採番すると、
同じ起点から分岐した 2 つのセッションが同じ番号を振り直し、ブランチをマージしたときに衝突します。Git linked
worktree は共通の Git directory を持つため、usagi はその配下の `usagi/issue-numbers/` を採番 authority として使います
（Git repository でない workspace は `<workspace>/.usagi/issue-numbers/`）。authority の `.lock` を取得したまま全
worktree を走査し、番号を予約するため、別 process・別 worktree の同時 create も直列化されます。

```
<git-common-dir>/usagi/issue-numbers/
├── .lock
├── sequence.json
└── reservations/
    └── 0000000472.reserved
```

予約 marker と `sequence.json` は issue markdown より先に durable + atomic write します。予約直後に process が crash
して issue ファイルが作られなくても、その番号は欠番として残り再利用されません。`sequence.json` が欠落・古い・JSON
として破損している場合は、authority lock 内で「全 worktree の既存番号・予約 marker・読み取れた sequence」の最大値へ
移行してから次を予約します。したがって、派生キャッシュ `index.json` や壊れた sequence を正として番号を戻しません。

## 番号 identity と重複修復

番号は CRUD の identity です。同じ `.usagi/issues/` に `NNN-*.md` が複数ある場合、`show` / `update` / `delete` と
対応する MCP tool は **ambiguity error** で fail closed します。エラーは衝突した全 path を辞書順で列挙し、どの sibling
も読み選ばず、書き換えず、削除しません。内部 error は番号と `PathBuf` 一覧を持つ typed
`AmbiguousIssueNumber` なので、presentation も文字列推測なしで識別できます。

重複は履歴を監査して次の手順で明示的に修復します。エラーに表示された exact path をそのまま対象にし、glob や
`issue delete <番号>` は使いません。

1. `git log --all -- .usagi/issues/NNN-first.md .usagi/issues/NNN-second.md` で、表示された各 path の起源と参照履歴を監査する。
2. 元の番号に残す 1 ファイルを決める。
3. `usagi issue create --title '<退避する issue の title>'` で未使用番号 `MMM` を予約し、生成された exact path を確認する。
4. 退避する sibling の frontmatter/body を生成ファイルへ移し、`number: MMM` と filename の `MMM-` を一致させる。
5. `git rm -- .usagi/issues/NNN-second.md` のように、退避元の exact path だけを削除する。
6. `usagi issue show NNN` と `usagi issue show MMM` で両 identity を確認し、参照番号の変更が必要なら同じ migration で更新する。

既存の重複を自動 renumber すると履歴上の参照先を誤るため、通常 CRUD は repair を代行しません。

## `index.json`（派生キャッシュ）

一覧・検索を速くするための、各 issue のメタデータ（本文を除く）のキャッシュです。`version` を持ち、markdown ファイルが常に **正**。書き込み・削除では該当 issue のエントリだけを差し替える**増分更新**で、markdown 全件の読み直しは行いません（ロック保持時間を issue 件数に比例させないため）。`index.json` が無い／壊れている場合のみ markdown 群から自動再構築されるため、欠落しても整合性は損なわれません（だから git 管理外でよい）。再生成可能なキャッシュなので、書き込みはアトミック（一時ファイル + `rename`）だが fsync は行いません。

`index.json` は usagi 自身の書き込み・削除でしか更新されないため、**git pull・ブランチ選択・セッションブランチのマージ・手編集**で markdown が変わると取り残されます。そこで一覧（`summaries`）はキャッシュを信頼する前に **鮮度を安価に検証**します。markdown ファイルを 1 つも読まず、`index.json` の更新時刻（mtime）と issue ファイル群の `stat` だけで判定します。

| 判定 | 意味 | 挙動 |
|---|---|---|
| ファイル数 ≠ キャッシュのエントリ数 | 外部でファイルが追加／削除された | 再構築 |
| いずれかの issue ファイルが `index.json` より新しい | 外部で編集された | 再構築 |
| 上記どちらも該当しない | キャッシュは最新 | そのまま信頼（stat のみの fast path） |

usagi 自身の書き込みは markdown → `index.json` の順で行うため、通常運用では `index.json` が常に issue ファイル以上に新しく、fast path に乗ります（余計な再構築を誘発しません）。mtime が同一刻みに収まる同時編集だけは検知できませんが、それは usagi 自身の一連の書き込み（同時にキャッシュも更新する）でしか起きず、この検証が守る外部変更には該当しません。採番の `max_number` も同じ理由でキャッシュを信頼せずファイル名から導出しています。

```jsonc
{
  "version": 1,
  "issues": [
    {
      "number": 1,
      "title": "doctor コマンドを追加",
      "status": "todo",
      "priority": "medium",
      "labels": ["cli"],
      "dependson": [2, 3],
      "related": [5],
      "parent": 4,
      "milestone": "v1",
      "file": "001-add-doctor-command.md",
      "created_at": "2026-06-14T00:00:00+00:00",
      "updated_at": "2026-06-14T00:00:00+00:00"
    }
  ]
}
```

## 依存関係の解決（着手可能な issue）

一覧・検索では各 issue に **着手可能か（ready）** を付与します。`dependson` に挙げた issue が **すべて `done`** で、かつ自身が `done` でないものを ready とみなします（存在しない依存番号は未達として扱う）。これにより「今すぐ着手できるタスク」を絞り込めます。

| 層 | モジュール | 役割 |
|---|---|---|
| domain | `domain/issue.rs` | `Issue` / `IssueSummary` / `IssueStatus` / `IssuePriority`、frontmatter の読み書き |
| infrastructure | `infrastructure/issue_store.rs` | 単一の `.usagi/issues/` の走査・読み書き、`index.json` の生成・再構築、そのストアの最大番号取得 |
| usecase | `usecase/issue.rs` | `create` / `get` / `list` / `search` / `update` / `delete` と ready 判定、ワークスペース横断の採番（`usecase/session.rs` の worktree 列挙を利用）、進捗集計（`IssueStats`）・グルーピング（`group`）・依存ツリー（`dependency_tree`） |
