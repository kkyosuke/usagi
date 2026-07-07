---
number: 149
title: fix(agent): Codex 対話起動の writable_roots に worktree の Git 共通ディレクトリを追加
status: done
priority: high
labels: [fix/agent]
dependson: []
related: []
created_at: 2026-07-07T08:13:14.368642+00:00
updated_at: 2026-07-07T08:13:20.705468+00:00
---

## 目的
agent codex / codex-fugu の対話セッションで `git commit` などのたびに承認プロンプト（`--ask-for-approval on-request`）が出る不具合を解消する。usagi 経由の起動でも Git 操作を承認なしで通す。

## 原因
#645 で Codex 起動コマンドに `-c 'sandbox_workspace_write.writable_roots=[<USAGI_HOME>]'` の上書きが入った。Codex の `-c` 配列代入は配列を丸ごと置換するため、ユーザー config の `.git` writable root が消える。workspace-write では `<root>/.git` が read-only 保護されるため、usagi worktree（git-dir/git-common-dir が `.git` 配下）では毎回承認が出ていた。`.git` ディレクトリ自体を writable root に列挙すれば保護判定を回避でき、承認が消える。

## 変更方針
対話起動時の Codex `writable_roots` に USAGI_HOME（既存）＋その worktree の Git 共通ディレクトリ（`git rev-parse --git-common-dir` の絶対パス）を含める。`AgentWiring` に `sandbox_writable_roots: Vec<PathBuf>` を追加（domain はデータ保持のみ）、usecase 側で Git 解決を注入して埋め、codex adapter が data_dir と連結してレンダリング。Git 解決失敗時はフォールバック（USAGI_HOME のみ）。`--ask-for-approval` は on-request のまま、headless は変更なし。

## 受け入れ条件
- 対話起動コマンドの writable_roots に USAGI_HOME＋Git 共通ディレクトリが含まれる
- Git dir 解決不能でも起動継続（USAGI_HOME のみ）
- 既存 Codex launch/headless テスト更新・全通過
- fmt / clippy 通過
