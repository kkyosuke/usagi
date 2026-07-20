---
number: 461
title: fix(update): verified staging と atomic replace で self-update を保護する
status: done
priority: high
labels: [review, v1, v2, security, release]
dependson: []
related: []
parent: 453
created_at: 2026-07-20T12:06:19.993200+00:00
updated_at: 2026-07-20T13:29:59.918154+00:00
---

## 問題・影響

共有 `scripts/install.sh` は起動 CWD に `./usagi` / `./usagi.exe` があると download を省略し、そのファイルを `~/.usagi/bin` へ移動してから `--version` を実行する。攻撃者が置いた binary を実行・install でき、download archive に checksum 検証もなく、検証前 replacement の失敗で既存 binary を失う。出荷中 `v1/src/usecase/self_update.rs::install_command` と root/v2 `crates/cli/src/cli/commands/update.rs::install_command` が同じ script を inherited CWD で実行するため同時修正が必要である。

## 成立条件 / 再現フロー

偽 `usagi` を含む directory から installer/self-update を実行すると network asset を使わず偽 binary が移動・実行される。checksum 不一致、壊れた archive、version mismatch、replace failure でも旧 binary を確実に保持する transaction 境界がない。

## 対象責務と非対象

installer、v1/v2 self-update caller、release checksum/signature artifact、private staging と atomic replace を対象とする。package manager 配布や自動 rollback daemon は非対象。local/bundled install は暗黙 CWD ではなく明示 option としてのみ許可する。

## 受入条件

- [ ] implicit CWD の binary を一切参照・移動・実行しない。
- [ ] mode 0700 相当の private staging に exact platform asset を download し、公開 checksum/signature、archive shape、path traversal/symlink、単一 expected binary を検証する。
- [ ] staged candidate の version を検証後、同一 filesystem の atomic replace を行い、全失敗で旧 bytes/mode を保持する。
- [ ] v1/v2 caller が installer failure を非 0 で伝え、並行 update を serialize する。

## 必須回帰テスト

悪意ある CWD sentinel、成功、bad checksum、truncated archive、unexpected/symlink entry、wrong version、rename failure、同時実行を hermetic test で固定し、release workflow が検証 artifact を発行することも検査する。

## docs / 移行影響

`v1/README.md`、`document/02-architecture.md`、release 手順を更新する。verification artifact のない旧 release は無検証 fallback せず明示的に失敗させる。データ migration はない。
