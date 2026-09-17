---
number: 762
title: refactor(test): 巨大 tests.rs の分割と重複 fake の test_support 集約
status: done
priority: medium
labels: [v2, refactor, test]
dependson: []
related: [760, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T17:35:48.346904+00:00
---

## 問題

- `crates/tui/src/presentation/tests.rs` は 23,469 行・334 test の単一ファイルで、workspace 最大のソースだった。
  `controller/tests.rs`（8,794 行・191 test）、`supervisor_runtime/tests.rs`（9,554 行）、`agent_ipc/tests.rs`（8,218 行）も同様。
- test double は `Fake*` / `Stub*` / `Recording*` 系で 77 種あり、同名の型が複数箇所で別々に定義されていた
  （`FakeClock` 4 箇所、`FakeProvisioner` 4 箇所、`FakeGit` / `FakeConnection` / `FakePort` 各 3 箇所）。

## やったこと

### 巨大 test ファイルの分割

bounded context ごとにファイルを分け、共有 helper と fake は各 `tests/mod.rs` に残した。

| 元ファイル | 分割後 |
|---|---|
| `presentation/tests.rs` 23,469 行 | `presentation/tests/` に `mod.rs`(3,647) + director / flow / garden / home / render / restore / session / terminal / work_run / workspace_rows |
| `controller/tests.rs` 8,794 行 | `controller/tests/` に `mod.rs` + closeup / director / garden / new / pointer / pr / session / workflow |
| `supervisor_runtime/tests.rs` 9,554 行 | `tests/` に `mod.rs` + artifact / dispatch / lifecycle / promotion / worker |
| `agent_ipc/tests.rs` 8,218 行 | `tests/` に `mod.rs` + admission / dispatch / restart / resume / terminal |

**5,000 行を超える test ファイルは無くなった**（最大は `presentation/tests/mod.rs` 3,647 行）。
分割で生まれた各ファイルには元の `#![coverage(off)]` を引き継ぎ、test fixture が coverage gate に入らない状態を保った。
`tests/architecture.rs` の 2 つの guard を、分割後の module 全ファイルを走査する形へ更新した。

### 同名 test double の解消

調査の結果、同名の fake は「同じ実装の重複」ではなく **「別々の port に対する別実装が、たまたま同じ総称名を
持っていた」** ものだった（`FakeProvisioner` は Agy / Claude / Codex / Agent の 4 つの別 trait、
`FakeConnection` は collection / rollover / workers の別文脈、`FakePort` は doctor / terminal_session の別 port）。
そのため統合ではなく、**fake している port を名前に含める改名**で曖昧さを解消した。

`FakeAgyProvisioner` / `FakeClaudeProvisioner` / `FakeCodexProvisioner` / `FakeAgentProvisioner`、
`FakeCollectionConnection` / `FakeRolloverConnection` / `FakeWorkerConnection`、
`FakeDoctorPort` / `FakeTerminalPort`、`FakeMonotonicClock` / `FakeRefreshClock` / `FakeLogicalClock` /
`FakeSessionClock`、`FakeGitRunner` / `FakeCleanGit` / `FakeSessionGit` など計 32 型を改名し、
**workspace 内に同名の test double が無い状態**にした。

## 方針の補正

当初案の「`crates/tui/src/test_support.rs` を追加して fake を集約」は行わなかった。改名の過程で調べた結果、
tui crate には module tree をまたいで重複する fake が無く（`Terminal` の fake はすべて `presentation/tests` 配下、
`FakePort` は別々の port）、空に近い `test_support` を作ることになるためである。分割後の各 `tests/mod.rs` が
その context の共有 helper / fake を持つ module として実際に機能している。

## 完了条件（達成状況）

- [x] 単一の test ファイルで 5,000 行を超えるものが無い
- [x] 同名 fake 型の重複定義が無い（`grep -rE 'struct Fake[A-Za-z]+'` の名前が一意）
- [~] `crates/tui/src/test_support.rs` の追加 — 上記の理由で不要と判断し、代わりに各 `tests/mod.rs` を
  共有 helper module として位置づけた
