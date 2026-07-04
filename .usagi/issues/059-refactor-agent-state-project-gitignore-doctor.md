---
number: 59
title: refactor: 層責務の漏れを是正（agent_state 遷移ポリシー・project .gitignore 編集・doctor 生プロセス）
status: done
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-06-19T22:16:31.906344+00:00
updated_at: 2026-07-04T00:14:08.367740+00:00
---

## 背景

複数の層で責務の置き場所がずれている箇所をまとめて是正する。

### 1. agent 状態遷移ポリシーが infrastructure に漏れている（`src/infrastructure/agent_state_store.rs:204-240`）
`ready_overwrite_allowed`（`:238-240`）等は「compact ソースや mid-turn では ready で上書きしない」という **agent 状態遷移のポリシー判断**で、usecase の責務。フック JSON のパース（`worktree_from_hook_json` 等）は IO 寄りで infra に残してよいが、遷移可否判断は usecase へ移す。

### 2. `.gitignore` 行編集が usecase に常駐（`src/usecase/project.rs:85-160`）
`USAGI_GITIGNORE` 定数・`is_legacy_root_ignore_line`・行フィルタ・末尾空行トリム・書き戻しという低レベル文字列処理が usecase の約 1/3 を占める。usecase は「`.usagi` を ignore させる」意図だけ持ち、行操作の実体は infrastructure（gitignore writer）へ降ろす。

### 3. doctor が CommandRunner 抽象を使わず生プロセス直叩き（`src/usecase/doctor/mod.rs:142-150`）
`CommandRunner` 抽象（`runner.rs`）があるのに doctor 本体の `which` だけ直接 `std::process::Command` を叩いており、テスト容易性・層責務の一貫性が崩れている。→ `CommandRunner` 経由に統一する。

## 確認方法

- 各ロジックが適切な層へ移り、usecase/infra の責務境界が締まること。
- 既存の挙動が変わらないこと（既存テスト維持、カバレッジ 100%）。
