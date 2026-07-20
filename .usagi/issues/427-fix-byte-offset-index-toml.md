---
number: 427
title: fix: レビュー検出の小粒バグ修正束（キャレット byte offset / マジック文字列 / 非検査 index / TOML 黙殺ほか）
status: todo
priority: low
labels: [fix, review]
dependson: []
related: []
created_at: 2026-07-20T11:58:54.931342+00:00
updated_at: 2026-07-20T11:58:54.931342+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。単独では小粒な correctness 改善をエリア横断で束ねる。

## 根拠と問題（検証済み）

1. `crates/tui/src/presentation/views/workspace.rs:1463` — `widgets::block_caret(&draft.name, draft.name.chars().count(), &accent)`。`block_caret`（widgets/mod.rs:142）は **byte offset** を期待するため、マルチバイト名でキャレット位置がずれる。widget 側に `debug_assert!(value.is_char_boundary(cursor))` も追加する。
2. `crates/tui/src/presentation/views/pr_modal.rs:104, 154-155` — `lookup_error` に表示分類用のマジック文字列 `"closed"` を流用（`Some("closed".to_owned())` を格納し `as_deref() == Some("closed")` で分岐）。表示用 enum へ。
3. `crates/core/src/domain/supervisor.rs:699` — retry backoff の `(task.attempt - 2)` は `u64` の減算。現行フローでは :694 の `attempt += 1` と初期値 1（:840）により常に ≥2 で**潜在的**だが、`add_task` 側で attempt≥1 を検証するか `saturating_sub` にして不変条件を明示する。`.min(30)` はシフト量のみ clamp。
4. `src/runtime/daemon.rs:1430-1431` — metrics 応答の `"active_subscribers": 0, "dropped_updates": 0` が固定値（実値が存在しない。#411 の MetricsBroker 判断と関連）。
5. `crates/tui/src/presentation/views/closeup_modal.rs:89-91` — `selected_action` が `self.matches()[self.selected]` の非検査インデックス。Option 返しへ。
6. `crates/core/src/infrastructure/runtime_model.rs:66-77` — malformed TOML を `else { return Self::default(); }`（:70-72）で黙って空 allowlist に落とす。ErrorLog へ記録するか、少なくとも stderr 警告。
7. `crates/core/src/infrastructure/persistence/markdown_store.rs:249-269` — index 鮮度判定 `.is_ok_and(|mtime| mtime <= index_mtime)`（:266）が **mtime 同値を fresh 扱い**（同一秒内の書き込みを取りこぼす）。
8. `crates/daemon/src/usecase/agent_ipc.rs:174, 191-192` — `AgentRuntime::new` の既定 `DispatchStore` が `std::env::temp_dir()` 配下。本番で誤って既定構築すると durable データが temp に落ちる。`cfg(test)` 化か必須引数化。
9. `crates/daemon/src/usecase/session_runtime.rs:362-363` — git 失敗分類が stderr 文字列マッチ（`error.contains("branch") && error.contains("already exists")`）。ロケールや git バージョンで壊れる。`rev-parse` 等の事前判定へ。
10. `crates/core/src/infrastructure/ipc/mod.rs` — フレーム検証の非対称: 直接 write（`write_frame_with_limit` :448）は上限・空 payload を検証するが、**queue 経路 `PendingFrame::new`（:564-569）は `u32::try_from(...).expect(...)` のみで max_frame_bytes 上限なし**（read 側 :485 は両上限を検証）。`push_control`（:623-637）/`push_output`（:642-652）は空 payload 検証を通らない。`ProtocolLimits::default`（:78-90）の `1_048_576` は const `DEFAULT_MAX_FRAME_BYTES`（:16）とリテラル重複。

## 改善案（要検討）

各項の括弧内提案を基本線とし、実装時に個別判断する。1 PR で一括でも、関連 issue（#411 など）と同時でもよい。

## 受け入れ条件

- [ ] 10 項それぞれに修正またはスコープ外の判断理由が記録されている。
- [ ] 修正項目はテストで固定されている。coverage 100% を維持。
