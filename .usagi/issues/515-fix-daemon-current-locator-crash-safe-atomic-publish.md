---
number: 515
title: fix(daemon): current locator の crash-safe atomic publish を復旧可能にする
status: in-progress
priority: high
labels: [review, v2, daemon, ipc, recovery, security, durability]
dependson: []
related: [216, 341, 507, 513]
created_at: 2026-07-22T00:07:06.965391+00:00
updated_at: 2026-07-22T00:09:36.755624+00:00
---

## 問題・影響

v2 Unix transport の `write_locator` は固定 `.current.json.tmp` を `create_new` し、write / fsync / rename failure または process crash 後に temporary file を回収しない。残骸が一つあるだけで以後の `SecureUnixListener::bind` と daemon autostart が毎回 `AlreadyExists` となり、利用者が手動削除するまで復旧できない。

さらに `OpenOptionsExt::mode(0o600)` は umask の影響を受ける。restrictive umask では publish 後の `current.json` が exact `0600` にならず、client の ownership/mode 検証に拒否される。locator 検証は symlink 拒否だけで regular file を保証していない。

## 成立条件・再現

指定基点かつ最新 `origin/main e047610b` の未修正実装に、private daemon directory 内へ既存 `.current.json.tmp` を置いてから locator publish する回帰テストを先行追加した。次の command は `Os { code: 17, kind: AlreadyExists }` で失敗し、固定 orphan が後続 publication を阻害することを確認した。

`cargo test -p usagi-daemon pre_existing_orphan_locator_temp_does_not_block_publication -- --nocapture`

## 重複・依存監査

既存 issue store を `current.json` / `.current.json.tmp` / locator / stale recovery / Unix transport で検索し、本件を扱う open issue はなかった。open PR #1225（issue #513、最新 head `5f704131`）は planned stop 時の endpoint retire と `current.lock` による publish/retire 直列化を扱うが、issue 本文で panic/crash stale recovery を scope 外としている。最新差分にも固定 temp、failure cleanup、umask 問題は残るため非重複である。

本件は `origin/main` から独立して修正可能なので #513 を blocking dependency にはせず related とする。writer ordering / late stale generation の replacement fence は #513 の `current.lock` が正本であり、本件では古い generation socket や `current.json` を推測削除しない。#1225 rebase 時は corrected publish を lock の内側に置き、新規 `current.lock` にも同じ secure file create/verification primitive を適用できる構造にする。

## 修正方針

- 固定 temp を writer ごとに一意な daemon-directory 内 private temp へ置換する。
- fresh temp は `create_new | O_NOFOLLOW | O_CLOEXEC` で開き、fd を `fchmod(0600)` してから regular file / effective UID owner / exact mode を fd metadata で検証する。
- bytes の write と file fsync 後だけ `current.json` へ atomic rename する。rename 前の create / write / sync / rename failure は既存 locator を保持し、その試行が所有する temp だけを必ず cleanup する。
- fchmod / fd 検証済みの temp inode だけを final locator へ rename し、discovery は final locator を secure-open した同じ fd 上で再検証する。parent directory fsync は post-commit ambiguity を error にしない best-effort とする。
- pre-existing fixed orphan は後続 publish を阻害しないが、所有権を推測して削除しない。
- late writer ordering、旧 generation socket/current の推測 cleanup、planned-stop retire は変更しない。

## 受入条件

- [ ] pre-existing `.current.json.tmp` orphan があっても新 locator を publish できる。
- [ ] write / sync / rename の各 injected failure で old `current.json` の bytes/locatorを保持し、当該 writer temp を残さない。
- [ ] 各 failure 後の retry が成功し、成功時にも writer temp leak がない。
- [ ] restrictive umask 下でも fresh temp と final `current.json` が symlink でない regular file、effective UID owner、exact `0600` になる。
- [ ] temp open は `O_NOFOLLOW | O_CLOEXEC` を使い、fresh fd を chmod 後に検証する。
- [ ] atomic rename 後の parent fsync failure は committed publication を failure と報告しない。
- [ ] concurrent writer / late stale writer の契約を #513 と競合させず、古い generation socket/current を推測削除しない。
- [ ] `document/04-ipc.md` を locator publish の SSoT として更新し、`document/05-daemon.md` は data-directory entry から参照する。

## 必須回帰テスト・gate

failpoint test で pre-existing orphan、write / sync / rename failure、old locator preservation、retry success、no temp leak を固定する。restrictive umask は process-global state を他 test と競合させない subprocess または同等の隔離で検証する。Unix IO・永続化・security boundary の変更なので fmt、workspace check/clippy、selected/full tests、coverage 100%、Markdown link check を必須とする。
