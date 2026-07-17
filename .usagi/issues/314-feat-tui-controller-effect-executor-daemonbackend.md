---
number: 314
title: feat(tui): controller Effect の本番 executor DaemonBackend を実装する
status: done
priority: high
labels: [tui, controller, daemon]
dependson: []
related: [258, 295]
parent: 258
created_at: 2026-07-17T14:21:53.722018+00:00
updated_at: 2026-07-17T22:00:00.248317+00:00
---

## 目的

#258 の第 2 段階。`controller::BackendPort` の本番実装 `DaemonBackend` を新設し、`Effect` 全 variant を daemon port 群へ接続する。現状、本番配線があるのは `Effect::LaunchAgent`（`AgentRuntimeHost`）のみで、他 variant を実行する adapter が存在しない。runtime のループにはまだ接続しない。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.3 / §5 PR2。

## スコープ

- `usagi-tui` の usecase/application に `DaemonBackend` を新設し、`SessionCommandPort` / `AgentRuntimeHost`（既存 #295 資産）/ notes・environment store / `OverlayDataPort` を束ねる。実 IO は合成ルート（`src/runtime/tui.rs`）が注入する。
- `CreateSession` / `RefreshSessions` / `RemoveSession`: worker + mpsc で実行し、完了を `OperationResult` / `BackendEvent::Sessions` として還流する。
- `OpenTerminal` / `SelectTab`: `AgentLaunchAdapter` / `PaneRuntime` へ接続する。
- `WorkspaceCommand` / `LoadNotes` / `SaveNotes` / `LoadEnvironment` / `SaveEnvironment` / `Detach`（ループ脱出）を実装する。
- `AttachWorkspace` / `CloneProject` / `RegisterWorkspace` は Workspace 画面では未使用のため no-op で受ける（screen graph 移行時に接続）。
- 「effect を出す → 実行する → 結果 event が reducer に戻る」の単方向ループに統一する。

## 完了条件

- fake port で variant ごとの dispatch → event 還流が unit test される。
- 全 port 注入でテスト可能な構造になっており、coverage 100% を維持する。`#[coverage(off)]` は実 IO ラッパ（合成ルート側）に限る。
