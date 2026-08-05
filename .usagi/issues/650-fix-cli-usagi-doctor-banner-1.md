---
number: 650
title: fix(cli): usagi doctor が診断ロジックを持たず banner 1行を出すだけ
status: done
priority: medium
labels: [review, v2, cli, tui, diagnostics]
dependson: []
related: []
created_at: 2026-08-05T01:02:06.714132+00:00
updated_at: 2026-08-05T08:55:27.508152+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 5。本 issue はその finding を再検証し起票したもの。

## Finding

`usagi doctor` は `crates/cli/src/cli/commands/doctor.rs` で `RunOutcome::LaunchTui(TuiRequest::Doctor)` を返し、最終的に `crates/tui/src/presentation/mod.rs` の `BannerScreenRunner::doctor` に到達する。実装は次のとおりで、`self.write_screen("doctor TUI")` の1行のみである。

```rust
fn doctor(&mut self) -> io::Result<()> {
    self.write_screen("doctor TUI")
}
```

依存ツールの検出、通知経路の確認、設定ストレージのヘルスチェックなど、診断らしい処理は一切実装されていない。他の `EntryScreen`（`Welcome`/`Config`/`Workspace`）は実際のインタラクティブ画面に接続されているのに対し、`Doctor` だけが placeholder の `BannerScreenRunner` に残っている。

`crates/cli/src/cli/mod.rs` のヘルプ文言（「必要ツールの導入状況を診断する」）や `README.md`（「必要ツールの診断画面を開く」）は実際の機能を説明しているが、実装が追いついていない。

## 影響

- ヘルプ・README が説明する機能が実際には存在しない。
- ユーザーが `usagi doctor` を実行しても何も分からない。

## 修正方針（例）

- 最低限、必須外部ツール（git 等）の存在確認、daemon 起動可否、設定ファイルの読み込み可否といった実際の診断を行い、結果を画面に表示する。

## 受け入れ条件

- `usagi doctor` が最低限の診断項目（ツール存在確認など）を実行し、結果を画面に表示する。
- 診断ロジックは `usagi-tui`/`usagi-core` の usecase 層でテスト可能な形にし、unit test を追加する。
