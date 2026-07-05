---
number: 114
title: refactor(domain): issue/memory の markdown 封筒処理を frontmatter に集約する
status: done
priority: high
labels: [refactor, core, review]
dependson: []
related: []
parent: 113
created_at: 2026-07-04T23:13:07.199754+00:00
updated_at: 2026-07-05T01:02:05.387017+00:00
---

## 背景（なぜ問題か）

`domain/frontmatter.rs` がフォーマットの原始関数（`split_frontmatter` / `format_string_list` / `inline` / `parse_timestamp` 等）を正本として持つのは良いが、その外側の「封筒（envelope）」処理が両エンティティにコピペされている。

- `to_markdown`: `"---\n"` ヘッダ → フィールドごとの `writeln!` → `"---\n\n"` + `body.trim_end_matches('\n')` + `push('\n')` の骨格が同一（infallible な `writeln!` の説明コメントまで逐語一致）。
- `from_markdown`: `strip_prefix("---\n").or_else("---\r\n")` の開始ガード → `split_frontmatter` → `for line in frontmatter.lines()`（空行スキップ・`split_once(':')`・`text_value`/`value` 分割・`_ => {}` 未知キー、コメントごと同一）→ 必須フィールド `ok_or_else` → body 正規化（`trim_start/end_matches(['\r','\n'])`）まで同一。

差分はフィールド集合とエラー型のみで、各 ~60 行が重複している。

## 対象箇所

- `src/domain/issue/markdown.rs`（`Issue::to_markdown` / `Issue::from_markdown`）
- `src/domain/memory/markdown.rs`（`Memory::to_markdown` / `Memory::from_markdown`）

## やること

- `frontmatter.rs` に、(1) ヘッダ／トレーラ／body 正規化を持つ writer と、(2) 開始ガード＋行ループ＋未知キー処理を行い `(key, value)` をエンティティ側のフィールドディスパッチャに渡す reader を追加する（トレイト or クロージャ）。
- 各 `markdown.rs` はフィールド列挙・ディスパッチだけを実装する。issue 固有の `format_number_list` / `parse_number_list` は issue 側に残す。

## 受け入れ条件

- 両 `from_markdown` / `to_markdown` が共通スキャフォールドを呼ぶ形になり、封筒ロジック（`---` 処理・行ループ・body 正規化）が 1 か所に集約される。
- ラウンドトリップ・パースエラーの既存テストが無変更で緑。カバレッジ 100% 維持。

## 補足

親 #113 の起点。この共通化が usecase CRUD 共通化（子 issue）の土台になる。domain 内で完結し影響範囲が読みやすい。
