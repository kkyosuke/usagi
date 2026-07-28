---
number: 572
title: feat(daemon): rollover trigger を IPC verb にして old active が gated handoff を駆動する
status: done
priority: high
labels: [review, v2, daemon, lifecycle, ipc, recovery]
dependson: []
related: [507, 508, 516, 559, 573, 574]
parent: 559
created_at: 2026-07-27T22:57:44.389093+00:00
updated_at: 2026-07-28T12:25:52.074054+00:00
---

## 問題・根拠（コード調査で確定）

[#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) の 3 つの前提配線
（[#560](560-feat-tui-client-ownerrouter-owner-generation-routing.md) の client routing、
[#561](561-refactor-daemon-serve-role-aware-standby-process.md) の standby serve、
[#562](562-refactor-daemon-durable-runtime-state-owner-shard-global-allocator.md) の owner shard）と、
active generation の admission fence / routing ledger 配線が揃った。残っているのは
**検証済み standby を active へ昇格させる handoff を起動する経路**だけである。

- `authority::rollover::execute_gated_rollover` に production の呼び出し元が無い（grep で 0 件）。
- `replacement::seamless_refusal` は `standby not admitted` を返す。これは「検証済み standby は居るが、
  この build には serve 中の standby を admit する lifecycle が無い」という**正確な**現状の記述である。
- `usecase::client::DaemonRequest` に lifecycle 系の verb が無い。

## 設計上の確定事項（この issue の前提）

**handoff を駆動するのは old active daemon process 自身**である。CLI process ではない。理由は 1 つで、
`execute_gated_rollover` が待つ barrier（`AdmissionGate`）が **process local** だからである。

- barrier は registry commit より**前**に閉じなければならない（[admission fence](../../document/05-daemon.md#admission-fence)）。
  CLI が registry を commit してから old daemon が「あとで気づく」形にすると、その間 old active は control を
  admit し続ける。これは #559 の受入条件「rollover 中も control/new spawn は active generation だけ」を破る。
- したがって `usagi daemon restart` は **old active へ IPC で rollover を要求**し、old active が
  standby の readiness を確認 → 自分の gate で barrier を閉じ → `execute_gated_rollover` を実行する。

standby process を**起動**するのはどちらでもよい（CLI が `ServeLauncher` を持つ / daemon が spawn する）。
どちらにするかはこの issue で決める。CLI 側に置くと daemon が argv を組まずに済み、handoff 前の失敗で
standby を止めるのも CLI の仕事になる。

## やること

1. `DaemonRequest` に lifecycle verb を 1 つ追加する（`operation_id` 付き。durable operation は
   `replacement::manual_operation_id` / `build_rollover_trigger` が既に導く）。
2. daemon の dispatch を新しい usecase へ繋ぐ。usecase は
   `authority::standby::verify_readiness` で successor の hello を証明し、
   `RolloverPlan`（ledger + successor hello + planned revision）を組み、
   `execute_gated_rollover` を自 process の gate で実行する。
3. `SeamlessRollover` を `ReplacementPlan` の第 3 の結果として追加し、`plan_replacement` を
   `live > 0 かつ planned かつ seamless 可能 → SeamlessRollover` に拡張する。
   `live == 0` は従来どおり cold transition のままにする（保持すべき PTY が無く、draining process を
   残す理由が無い）。
4. `SeamlessRefusal` を実装に合わせて書き直す。`standby not admitted` は解消するので落とし、
   registry から観測できる残りの拒否理由（registry 不在 / schema / 読めない / 生存する登録済み
   active が居ない / generation 上限）を名前で示す。
5. commit 前の全 failure は old active を維持する。commit 後の partial phase は既存の
   `authority::rollover::recover` が roll-forward / fail closed へ収束させる。

## 非対象

- draining generation の**自動回収**（`collect_retired` の駆動と process exit）は
  [#573](573-feat-daemon-draining-generation-claim-0.md)。
- 2 daemon process の product E2E は [#574](574-test-daemon-seamless-rollover-product-e2e-2-daemon-process-pty.md)。
- daemon crash / SIGKILL 後の旧 PTY master fd 回収は #221。

## 受入条件

- [ ] shipping `usagi daemon restart` が new active の readiness 後に authority を handoff し、live terminal を
      持つ old daemon を draining として残す。provider resume argv は一度も実行されない。
- [ ] routing capability を広告しない client が 1 つでも接続している、旧 build、registry revision mismatch の
      いずれでも rollover を開始せず、old active / current / live PTY を維持した typed refusal になる。
- [ ] start / hydrate / bind / readiness と authority commit **前**の failure では old active を維持し、
      staged した standby を止める。
- [ ] observable commit 後の registry / locator partial phase は roll-forward / repair または fail closed へ
      収束し、二重 active・二重 spawn・state split-brain を起こさない。
- [ ] rollover 中も control / new spawn は active generation だけ、terminal operation は exact owner generation
      だけが実行し、late / stale request と event は effect zero になる。
- [ ] generation 上限と連続 restart を fail closed に扱う。
- [ ] `daemon stop` の live-resource refusal（#507）は変わらない。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の
      [planned replacement](../../document/05-daemon.md#planned-replacement) と
      [document/04-ipc.md](../../document/04-ipc.md) を実装済みの現在形で更新する。

## 必須回帰テスト・計測

- `cargo test -p usagi-core`（新しい wire verb の (de)serialize）
- `cargo test -p usagi-daemon`（`usecase::authority` / 新 usecase の判定）
- `cargo test -p usagi --bin usagi`（合成ルートの配線）
- fake registry + fake standby で「readiness failure」「revision mismatch」「routing 非対応 client」
  「generation 上限」の 4 つの refusal を固定し、いずれも effect zero であることを durable state の
  byte 比較で示す。
- Rust 差分を含むため fmt / check / clippy / 推奨 test を通し、full gate は PR CI で確認する。
