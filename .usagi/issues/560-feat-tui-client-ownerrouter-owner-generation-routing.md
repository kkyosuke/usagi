---
number: 560
title: feat(tui): client を OwnerRouter に載せて owner generation 宛の routing を配線する
status: todo
priority: high
labels: [review, v2, tui, ipc, daemon, lifecycle]
dependson: []
related: [508, 516, 559]
parent: 559
created_at: 2026-07-26T13:57:35.232736+00:00
updated_at: 2026-07-26T13:57:35.232736+00:00
---

## 問題・根拠（コード調査で確定）

[#508](508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) は
`usagi_core::usecase::owner_routing` に `OwnerRouter` / `merge_inventory` / `GenerationLinks` を実装したが、
**production の呼び出し元が 1 つも無い**。

- `crates/core/src/usecase/owner_routing.rs` に `OwnerRouter`（684 行）/ `merge_inventory`（450 行）/
  `GenerationLinks`（548 行）がある。
- 合成ルートと TUI は `usagi_daemon::infrastructure::unix_transport::connect_current` だけで接続する
  （`src/runtime/daemon.rs` / `src/runtime/tui.rs`）。`OwnerRouter` は grep しても production 参照が無い。

したがって [#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) が挙げる 3 つの欠落配線のうち
**client routing だけが未着手のまま残っている**。

## この issue を分けた理由

本 issue は **generation が 1 つしか無い現在の build でも振る舞いを変えない**。`OwnerRouter` は active しか無ければ
active endpoint を返すので、`connect_current` と同じ接続先に解決される。したがって standby を起動できるように
なる前に独立してマージでき、かつ #559 の残りリスク（単一インスタンス性・durable state 移行）に触らない。

## 既存 issue との境界

- [#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md)（parent）— seamless rollover の
  **有効化**が対象。`SeamlessRollover` を `ReplacementPlan` に追加すること、old generation の回収、
  2 daemon process の product E2E は #559 が持つ。**本 issue は routing の配線だけ**で、rollover は有効化しない。
- [#508](508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md)（done）— `owner_routing` の
  pure authority が正本。**判定ロジックは再実装しない**。
- [#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)（done）— registry /
  standby / handoff の authority。registry の読み取り契約はそちらが正本。

## やること

1. client の接続解決を `connect_current` 直呼びから `OwnerRouter` 経由へ置き換える。
   - control / launch → active generation
   - terminal operation → `TerminalRef.daemon_generation` の exact owner
   - inventory → `merge_inventory`
2. draining endpoint の一時不通を `reconnecting` として保持し、**verified retirement まで tab を回収しない**。
3. generation が 1 つのときは現在と同じ接続先・同じ挙動になることをテストで固定する。

## 設計上の判断が必要な点

- **`OwnerRouter` の生存期間と再解決の契機**。registry revision が変わったときに再解決する必要があるが、
  接続ごとに registry を読むと IPC の hot path にファイル読み込みが入る（[#555](555-perf-daemon-pr-identity-pty-hot-path.md)
  で daemon 側の同種問題を潰したばかりである）。revision を観測して cache する形にするか、
  接続確立時だけ解決するかを決める。
- **exact owner が未知・retired のときの扱い**。`TerminalRef.daemon_generation` に対応する endpoint が registry に
  無い場合、active へ fallback すると別 generation の PTY を掴む危険がある。**fail closed（typed error）にするのが
  既定**で、その error を TUI がどう提示するかを決める。
- **capability 未対応の相手**。wire では `owner-generation-routing.v1` を双方向 advertise 済みなので、
  未対応 client / server と混在したときに routing を使わない経路へ落ちる条件を明示する。

## 受入条件

- [ ] generation が 1 つのとき、接続先と観測される挙動が現在と同一である（回帰テストで固定）。
- [ ] draining generation が存在するとき、terminal operation が exact owner endpoint へ、control / launch が
      active へ解決される（fake registry で固定）。
- [ ] exact owner が未知・retired のとき active へ fallback せず typed error になる。
- [ ] draining endpoint の一時不通で tab を回収しない（`reconnecting` を保持する）。
- [ ] registry 読み取りが IPC の per-request hot path に入っていない。
- [ ] カバレッジ 100% を維持する。[document/04-ipc.md](../../document/04-ipc.md) の routing 記述を実装に合わせて
      更新する（**未実装の rollover 契約は書かない**。それは #559）。

## 必須回帰テスト・計測

- `cargo test -p usagi-core`（`usecase::owner_routing` が退行しないこと）
- `cargo test -p usagi --bin usagi` / `cargo test -p usagi-tui`（接続解決の配線）
- fake registry で「active のみ」「active + draining」「exact owner 不明」の 3 状態を固定する。
- 実 daemon 1 プロセスの結合テストで、generation 1 つのときに現在と同じ経路で接続できることを確認する
  （起動は [`tests/support/daemon.rs` 経由](../../document/06-conventions.md#結合テストからの-daemon-起動)）。
- Rust 差分を含むため fmt / check / clippy / `scripts/recommend-tests.sh origin/main` の推奨 test を通し、
  full gate は PR CI で確認する。
