---
number: 569
title: fix(daemon): 死んだ standby の registry entry を activation が回収する
status: done
priority: high
labels: [v2, daemon, lifecycle, durability, recovery]
dependson: []
related: [516, 561, 568]
parent: 559
created_at: 2026-07-27T11:01:14.672468+00:00
updated_at: 2026-07-27T11:09:45.471031+00:00
---

## 問題・根拠（コード調査と再現テストで確定）

[#561](561-refactor-daemon-serve-role-aware-standby-process.md) が `usagi daemon serve --standby` を
起動できるようにしたことで、**standby が SIGKILL された data directory は以後 daemon を起動できなくなる**。

- `RegistryDocument::activate_first` は `self.handoff.is_some() || self.retained() > 0` で
  `authority_retained` を返す。`retained()` は role が `retired` でない entry を**すべて**数える。
- `handoff::plan_recovery` の steady state が調停するのは **active と current locator の対**だけである。
  `document.active()` が `None`（active が正常終了して retire 済み）で locator も無い状態は
  `RecoveryOutcome::Consistent` になり、`standby` のまま残った entry は誰も触らない。
- したがって production の active 起動経路（`claim_authority` → `recover` → `activate_first`）は、
  死んだ standby の entry を回収しないまま `authority_retained` で失敗する。

再現手順（実プロセス）:

1. `usagi daemon start`（active 登録）
2. `usagi daemon serve --standby`（standby 登録・readiness 通過）
3. standby を **SIGKILL**（`generations.json` に `role: standby` の entry が残る）
4. `usagi daemon stop`（active は正常に retire。`current` は null、active entry は `retired`）
5. `usagi daemon start` → **失敗**。以後 `generations.json` を手で消すまで復旧しない

pure な再現（`claim_authority` 単体）も確認済みで、`RegistryError::AuthorityRetained` を返す。

## なぜ #561 の範囲外だったか

#561 の受入条件は「crash した **active** の stale registry entry が回収され、その後に新しい active が
起動できる」であり、それは `plan_recovery` の `ActiveGone` fail-closed が満たしている。standby が
registry に載るようになったのは #561 が初めてなので、**standby / draining の entry を crash 後に誰が
回収するか**が未定義のまま残った。

standby 側の custody（`authority::standby::evaluate_custody`）は「active が消えた standby は自主終了する」
経路であり、**standby 自身が死んだ場合**には走らない。

## やること

`activation` の active 起動経路に、**retained だが exact process identity で生存を証明できない generation を
retire する**段を足す。判定基準は `plan_recovery` が active に対して使うものと同じ
（`ProcessObservation::VerifiedAlive` でなければ回収）にして、契約を 1 つに保つ。

- 対象は `standby` / `draining` / `active` の別を問わない non-retired entry すべて。draining でも同じ wedge が起きる。
- 生存を証明できた generation が 1 つでも残るなら、これまでどおり `authority_retained` で effect zero に拒否する。
- 回収で live な standby の entry を落としてしまった場合は、standby 側の custody（`EntryAbsent`）が
  その standby を自主終了させるので収束する。

## 受入条件

- [ ] SIGKILL された standby の entry が次の activation で回収され、`daemon start` が成功する。
- [ ] 生存を証明できる generation（active / standby のいずれも）が残っている間は、これまでどおり
      `authority_retained` で拒否し、registry と locator に触れない。
- [ ] 死んだ draining generation でも同じく回収される。
- [ ] カバレッジ 100% を維持し、[document/05-daemon.md](../../document/05-daemon.md) の
      [first activation](../../document/05-daemon.md#first-activation) に回収の契約を書く。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::authority::activation` / `handoff` の退行が無いこと）
- **実プロセスの結合テスト**: standby を SIGKILL → active を stop → `daemon start` が成功し、
  registry の entry が 1 件（active）に戻ること。起動は必ず
  [`tests/support/daemon.rs` 経由](../../document/06-conventions.md#結合テストからの-daemon-起動)。
- Rust 差分を含むため fmt / check / clippy / 推奨 test を通し、full gate は PR CI で確認する。
