---
number: 626
title: test(root): composition root の coverage debt を pure decision から除く
status: in-progress
priority: medium
labels: [review, v2, test, coverage, runtime, cli]
dependson: [484]
related: [453]
created_at: 2026-08-02T23:07:31.856402+00:00
updated_at: 2026-08-03T00:53:10.268476+00:00
---

## Finding（P2 / coverage policy 違反）

### path / symbol

`coverage-off-allowlist.json` で owner `root-cli` / reason `migration_debt` とされた root 側14 item。

- `src/main.rs::main`
- `src/runtime/bootstrap.rs::{require_expected_build, wait_for_ready, tests module}`
- `src/runtime/cli.rs::dispatch`
- `src/runtime/clipboard.rs::{ClipboardPort::write_text, current_platform, tests module}`
- `src/runtime/launchd.rs::{install, uninstall, plist_path, launchctl, tests module}`

### 発生条件

bootstrap の build/readiness 判定、CLI route/error、clipboard fallback、LaunchAgent plist/path/action の分岐を変更し、coverage gate を実行する。item 全体の exclusion により real IO と同じ関数内の pure decision/error mapping まで未計測になる。

### 影響

shipping binary の起動・daemon recovery・platform integration が壊れても、100% gate が未到達分岐を示さない。特に `wait_for_ready` の timeout/refusal mapping、`runtime::cli::dispatch` の action routing、launchd plist 生成や存在判定は実 IO そのものではなく、規約上 exclusion を許されない判断である。

### 具体的根拠

- `document/06-conventions.md#coverageoff-例外` は「テスト可能な判断を分離したあとの real IO」と composition だけを許し、validation/reconcile/error mapping を除外する。
- `document/08-coverage.md#rootcli-の内訳` は bootstrap recovery 判断、CLI routing、plist 生成・判断を削除対象と明記する。
- registry は14 itemを `root-cli-follow-up` に置くが、対応する既存 issue は `issue_search` で0件だった。
- `src/runtime/bootstrap.rs`、`src/runtime/cli.rs`、`src/runtime/launchd.rs` には既存 unit test がある一方、test module 自体まで debt exclusion され、coverage gate が契約到達を証明できない。

### 修正方針

pure decision、formatting、classification、error mapping を注入可能な関数へ分離して coverage 対象へ戻す。OS process、home lookup、clipboard/launchctl 実行、最終 composition だけを最小範囲にし、残す exclusion は許可理由・owner・期限・shipping/fake integration test を付けて再登録する。

### 必須回帰テスト

- bootstrap: expected build mismatch、readiness retry上限、workspace refusal、start/restart error。
- root CLI: 全 action route、daemon/client error、stdout/stderr/exit status。
- clipboard: platform command ordering、partial failure、全 backend failure。
- launchd: plist escaping/path、既存/不存在 uninstall、launchctl success/failure。
- shipping binary の代表 smoke/integration test、coverage registry lint、workspace line/function 100%。

### docs / migration

`document/08-coverage.md` のroot内訳と残存許可例外を更新する。永続データ migration はない。
