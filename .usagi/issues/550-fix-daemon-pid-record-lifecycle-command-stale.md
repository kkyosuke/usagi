---
number: 550
title: fix(daemon): PID 再利用 record を lifecycle command からも stale として回収する
status: done
priority: medium
labels: [v2, daemon, lifecycle, recovery, process-safety]
dependson: []
related: [507, 513, 515, 516]
created_at: 2026-07-25T13:20:20.168166+00:00
updated_at: 2026-07-26T06:25:11.316240+00:00
---

## 問題・影響

daemon owner の exact process identity fence は `3150ac3a`（#1241）と `f1e68538`（#1234）で main に landed した
（経緯は [#516 の依存の再判定](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md#依存の再判定)）。
その受入条件のうち **「Gone / Reused の stop と start stale reclaim」だけが未達**で、同じ観測に対して
lifecycle command と通常 client bootstrap が別の判断をする非対称が残っている。

- `usagi-core` domain の `classify` は `DaemonProcessObservation::Gone` だけを `DaemonState::Stale` にし、
  `IdentityMismatch`（= PID が再利用され、別 incarnation が占有している＝**owner は確定的に消滅している**）を
  `Unverified` に落とす。
- そのため `daemon stop` は `daemon owner identity is unverified; refusing to signal or reclaim the record` を返し、
  `daemon start` は `refusing to start a replacement` を返し、`restart` は stop 段階で失敗する。record・locator・
  旧 generation socket はいずれも残る。
- 一方で合成ルートの `recover_stale_client_endpoint_with` は `Gone | IdentityMismatch` の両方を reclaimable として
  扱い、`daemon.lock` 下で record を再照合してから endpoint を retire し exact CAS clear する。

結果として、daemon が crash した後にその PID が再利用されると、**明示 lifecycle command だけが wedge する**。
利用者は `daemon start` / `stop` / `restart` が一貫して失敗するのに、無関係な daemon-backed CLI 要求
（`session list` 等）を一度実行すると回復する、という説明できない挙動を見る。回復は再利用 process の終了か
ordinary bootstrap 待ちに依存する。

`IdentityMismatch` は「所有権が不明」ではなく「所有者が確定的に別 incarnation に置き換わった」という
**肯定的な証拠**である。これを `Unknown`（真に観測できなかった場合）と同じ扱いにしているのが根因である。

## 修正方針

- domain の `classify` で `IdentityMismatch` を `Unknown` と区別する。`Gone` と `IdentityMismatch` はどちらも
  「recorded owner incarnation は存在しない」ことが OS により確定した状態であり、reclaimable stale として扱う。
  `Unknown`（legacy identity、observation failure、unsupported platform）だけを `Unverified` に残す。
- `Stale` を細分するか `DaemonState::Stale` に集約するかは、`status` の表示文言が両者を区別できることを条件に選ぶ。
  `status` は `Gone` と `Reused` を利用者に判別可能なまま出す（reclaim 可能である事実は同じでも、原因が違う）。
- reclaim 経路は既存の primitive を再利用し、複製しない。`stop` は `StaleDaemonCleanup::cleanup_if`、bootstrap は
  `recover_stale_client_endpoint_with` を通り、どちらも `daemon.lock` 保持 → 観測 record の再照合 →
  #515 の socket-first / locator-last conditional retire → exact record CAS clear の順序を保つ。
- reclaim 判定が変わっても **signal 経路は変えない**。`signal_exact_process` は `Exact` 以外に signal を送らない
  （Linux は identity 検証済み pidfd、macOS は kill 直前の再検証）。本 issue で raw PID fallback を導入しない。
- record 境界の numeric PID 検証を追加する。`pid` が 0 / 1、`pid_t` 範囲外の record は deserialize / registration で
  拒否する。現状は identity 照合により結果的に fail-closed だが、`kill(0, …)` が caller の process group を
  対象にする形の値を durable record に載せられる余地を残さない。
- 陳腐化した pointer を消す。`src/runtime/daemon.rs` の `recover_stale_client_endpoint` doc comment は
  「Future exact identity fields added by #514」と、存在しない issue を将来作業として指しているが、その直下の
  `ExactProcessControl.observe` が既にその field を使っている。

## 非対象

- cross-process generation registry / standby handoff（#516）
- shipping planned restart の active/draining rollover（#507）
- Agent / generic terminal child の `ProcessIdentity` を実 OS identity にする作業（#518）

## 受入条件

- [ ] `IdentityMismatch` の record に対して `daemon stop` が endpoint retire と exact record clear を完了し、
      成功を報告する。signal は 0 回で、PID を占有している無関係 process は生存する。
- [ ] `IdentityMismatch` の record に対して `daemon start` / `restart` が replacement を一度だけ起動し、
      新 record が別 process-start identity を持つ。二重 daemon を起動しない。
- [ ] `Unknown`（legacy identity 欠落、observation failure、unsupported platform）は従来どおり
      `Unverified` として effect zero で拒否し、signal・record clear・endpoint retire・replacement spawn を
      いずれも行わない。
- [ ] `daemon status` が `Gone` 由来の stale と `Reused` 由来の stale を利用者に区別できる文言で報告し、
      どちらも reclaimable であることを示す。
- [ ] lifecycle command の reclaim と通常 client bootstrap recovery が、同じ観測に対して同じ結論に達する。
- [ ] `pid` が 0 / 1 / `pid_t` 範囲外の record は deserialize / registration 境界で拒否され、いかなる
      signal 経路にも到達しない。
- [ ] reclaim 中に replacement record が保存された場合は replacement・locator・socket を保持し、
      cleanup failure では record を残す（#515 の failure invariant を回帰させない）。

## 必須テスト

- domain unit: `classify` の `Exact` / `Gone` / `IdentityMismatch` / `Unknown` × record 有無の全組み合わせ。
- domain/store unit: PID boundary（0 / 1 / 範囲外 / 負表現）と legacy / malformed schema の拒否。
- usecase fake: `stop` / `start` / `restart` が `IdentityMismatch` で reclaim → replacement へ進み、`Unknown` では
  effect zero に留まること。cleanup failure、record CAS race、replacement preservation を含む。
- 合成ルート: `IdentityMismatch` を作るために実 child を起動して record の identity だけを差し替え、
  `SigtermTerminator` が signal を送らないこと、reclaim 経路が socket-first / locator-last の順序を守ることを固定する。
- production lifecycle test: crash owner の record を PID 再利用相当に加工したうえで `daemon stop` → `daemon start`
  が回復し、以後の通常 CLI bootstrap が追加の daemon を起動しないことを検証する。
- `status` の表示文言 regression。

## docs / gate

`document/05-daemon.md` の lifecycle record / stale reclaim 節（`running` / `stale` / `unverified` / `absent` の
判定表）を新しい分類に合わせて更新する。Rust・durable schema・process/signal に影響するため、fmt / check / clippy、
`scripts/recommend-tests.sh` の selected tests、full test、coverage 100%、Markdown link check を必須とする。
