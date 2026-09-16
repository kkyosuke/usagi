---
number: 760
title: refactor(daemon): supervisor_runtime.rs / agent_ipc.rs / session_runtime.rs を bounded context 別に分割し tests を隣接ファイルへ出す
status: done
priority: high
labels: [v2, refactor, daemon]
dependson: []
related: [757, 762]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T17:02:40.342660+00:00
---

## 問題

`crates/daemon/src/usecase/` の 3 ファイルが 1 万行規模で、production とインライン `mod tests` が同居していた。

| ファイル | 総行数 | production |
|---|---|---|
| `supervisor_runtime.rs` | 14,399 | 約 4,783 |
| `agent_ipc.rs` | 13,207 | 約 4,914 |
| `session_runtime.rs` | 7,482 | 約 2,792 |

同じ crate の `usecase/resources/durable.rs` + `durable/tests.rs` は既に「production と tests を隣接ファイルに分ける」形に
なっており、これらだけが古い形で残っていた。

## やったこと

### test を隣接ファイルへ

インライン `mod tests` を `<module>/tests.rs` へ出した（`durable` と同じ配置）。`runtime.rs` と `terminal.rs` も同様に
揃えた。

### 巨大 impl を bounded context 別の子 module へ

`SupervisorRuntime`（84 methods / 3,134 行）と `AgentRuntime`（複数 impl 合計 3,867 行）の inherent impl を、
子 module ごとの `impl` ブロックへ分けた。

| 子 module | 内容 |
|---|---|
| `supervisor_runtime/reservation.rs` | dispatch / goal / handoff の予約・bind・失敗処理（18 methods） |
| `supervisor_runtime/obligations.rs` | worker stop の義務、terminal handoff、artifact verification、promotion（23 methods） |
| `agent_ipc/admission.rs` | 受け入れ判定と容量確保、commit（6 methods） |
| `agent_ipc/delivery.rs` | prompt の投函と report の受理・照合（11 methods） |
| `agent_ipc/dispatch.rs` | dispatch worker の計画・実行と peer 通知（7 methods） |
| `agent_ipc/lifecycle.rs` | 起動・再開・daemon 再起動をまたぐ復帰（23 methods） |

### 結果

`crates/daemon/src/usecase/` の production ファイルはすべて 3,000 行以下になった（最大は
`session_runtime.rs` 2,792 行）。

## 当初の計測の訂正

issue に書いた「`cognitive_complexity` 25 超の production 関数 18 件のうち 10 件が daemon crate」は誤りだった。
当時の計測は test コードを含んでおり、daemon の 10 件のうち 7 件は `mod tests` 内のテスト関数だった。
production の実数は `generation.rs` / `workflow.rs` / `orchestration.rs` の 3 件で、これは本 PR の前後で変わらない。
テストを隣接ファイルへ出したことで、以後この取り違えは起きない。

## 完了条件（達成状況）

- [x] `crates/daemon/src/usecase/` に 3,000 行を超える production ファイルが無い
- [x] production と tests を同居させない（`durable` と同じ配置に統一）
- [~] 「cc 25 超の production 関数が半減」— 上記のとおり前提の計測が誤りで、production の実数は元から 3 件。
  残る 3 件（`generation.rs` / `workflow.rs` / `orchestration.rs`）は本 issue の対象ファイル外なので #766 で扱う
- [x] `document/02-architecture.md` に分割後の module を反映
