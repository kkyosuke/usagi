---
number: 511
title: fix(core): v2 issue CRUD を重複番号で fail-closed にする
status: in-progress
priority: high
labels: [review, v2, core, issue, persistence, safety]
dependson: []
related: [335, 471]
created_at: 2026-07-21T21:29:10.408119+00:00
updated_at: 2026-07-22T12:37:31+00:00
---

## 問題・影響

v2 の `IssueStore` は、同じ番号を持つ `NNN-*.md` が複数ある場合でも point CRUD の identity を一意に検証しない。`read_locked` は directory iteration 順の先頭を返し、`write_locked` は選ばれなかった sibling を stale filename として削除し、`remove_with_outcome` は同番号の全 sibling を削除する。現行 backlog には #323 と #390 の番号衝突が実在するため、任意読取と不可逆な誤更新・誤削除が現実に起こり得る。

初回修正 #1226 のマージ後も、point read の一意性判定より先に derived repair が走り得る lock 範囲、search が3回の source snapshot を混在させる競合、`session_delegate_issue` が typed ambiguity を失う error mapping、missing store の read が lock directory を作る副作用が残った。本 follow-up は #1226 マージ後の main を基点に、この4点を同じ受入条件の未達として完了させる。

さらに実運用の sibling worktree から v2 MCP `issue_create` を呼ぶと、v2 独自の `<git-common-dir>/usagi-issue-sequence/next` が production/v1 の `<git-common-dir>/usagi/issue-numbers/sequence.json` と reservation journal を見ず、未 merge の #514/#515 を再採番した。誤 source の削除後も high-water authority の分裂が残るため、本 follow-up で v1/v2 の authority を統一し、旧 v2 state を gap 非再利用で移行する。

follow-up branch は #1226 を含む `origin/main` 910d7ffa を基点とし、既存 issue #511 の未達 acceptance として完了する。新しい issue は作らず、merge 済み PR #1226 へ追加 push しない。

## 対象責務

- v2 `crates/core/src/infrastructure/store/issue.rs` の point read/write/remove を、同番号 source が複数あるとき typed ambiguity error で fail-closed にする。
- ambiguity error は issue number と衝突した全 exact path を辞書順で保持し、get/update/delete と MCP adapter まで安全に伝播させる。
- list/search は repair のため source set を観測できる契約を保ち、point CRUD のように任意の sibling を選ばないことを明記する。
- v2 採番を v1 と同じ Git-common `usagi/issue-numbers` authority、version 1 JSON、durable reservation marker、共通 lock へ統一し、旧 v2 sequence と全 worktree source 最大値を移行時に fold する。
- 旧 v2 writer と new/v1 authority を両 lock で直列化し、全 durable floor が見える live 側だけを残す durable sentinel または回復 floor 付き atomic `u32::MAX` blocker を最初に公開する。両 live 側が既存 fenced floor より遅れていれば write 前に停止する。
- v2 の正本 docs に番号 identity、point CRUD の fail-closed、明示 repair の境界を反映する。

## 受入条件

- [x] 同番号 sibling が2件以上ある場合、get/update/delete は同じ deterministic な ambiguity error を返す。
- [x] ambiguity 判定は dirty marker、target write、remove より前に行われ、失敗後も全 sibling が byte-for-byte 不変である。
- [x] 通常の0件/1件 CRUD、title rename、derived refresh/repair の既存契約を維持する。
- [x] list/search と MCP adapter の挙動・説明が fail-closed 契約と整合する。
- [x] filename claim の corrupt / unreadable source は original error を伝播し、write/remove 後も source/index/dirty marker を byte-for-byte 不変に保つ。
- [x] `007-*.md` が `number: 8` を宣言する場合、canonical #8 の有無にかかわらず #7/#8 双方の get/update/delete/write/remove を typed mismatch で拒否し、prefixless declared claim も同じ契約にする。
- [x] v1 authority の `last_reserved=515` から v2 が 516 を予約し、旧 v2 high-water、abandoned reservation、全 source のfilename prefix / parse可能なfrontmatter宣言の最大値のいずれも後退・gap 再利用しない。source read / parse errorは宣言floorを推測せずfail-closedにする。
- [x] fresh Normal migrationでは全old-v1 callerが共有するsequence/journal floorがsource等を含む全durable floorならsole legacy sentinel、sole live legacyが全durable floorならMAX+floor blockerを最初のatomic writeとする。caller依存sourceはv1-visibleと推測せず、どちらも遅れていればfail-closedにする。既存blocker回復ではlegacyを先にfenceし、blocker保証 → 全sentinel → reservation → Git marker → normal sequence → sourceの順を固定する。全crash boundaryとsource write failure後も予約番号を再利用しない。
- [x] normal/exhausted sequence、MAX+floor blocker、active legacy、sentinel、migration marker、reservation marker の invalid schema/version/canonical body/read failure、および non-exhausted normal sequence 下の Git marker/shared-sentinel mismatch は新しい予約や source write 前に fail-closed になる。marker未作成のsentinel-first境界、blocker下のvalid crash floor差、および`Normal(u32::MAX)` recovery tag下のpartial terminal sentinel/marker差だけはmaxをfoldして回復する。
- [x] new authority lock → sorted observed legacy lock の順で既起動旧 process と直列化し、先行旧予約を fold、new allocator を待つ列挙済み旧 v2 writer を sentinel parse error で fence し、blocker 中の旧 v1を副作用ゼロで停止する。両high-water方向は、直前HEADのresolver/reserveを再現するold-v2 compatibility emulatorを実subprocessで固定する。加えてpre-fix commit `677405d31267e9205b76a26fe8b31098b6086852`の実MCP processを生存させたまま旧create #1 → fixed create #2 → 同じ旧processの再createを実行し、最後がsentinel parse error、source/index/sentinel/marker/sequence/reservationのSHA-256が失敗前後不変になることをrelease acceptanceで確認する。
- [x] durable floorが`u32::MAX`でもsafe first-write条件を満たすなら低いnumeric legacyを残さず、v1-visible MAXならsentinel(MAX)、sole legacy MAXならnormal sequence(MAX)を先に公開して両old writerを停止する。両live側がMAXを見ないsource-only exhaustionはzero-write failure、完了後は追加reservation/sourceなしでexhaustionを返す。
- [x] Git common、未作成でもcurrent nested、既存root/workspace/direct-session、および登録済み全Git worktreeのpathspecで見つかるmaterialize済みnested legacy/sourceを毎回列挙する。nested sourceからstore-local legacy pathを`next`未作成でも導出してlock/fenceする。非Gitはroot/current/存在する全direct sessionを未作成でも列挙し、global migration markerを公開・更新せず、後発sessionを既存markerで隠さない。既存markerはcanonical recovery floorとしてのみfoldする。known Missingも未封鎖として数え、複数ならblocker/sentinel/sequence/reservation/sourceのauthoritative write前（lock materialization除く）に停止する。
- [x] 列挙結果に現れないGitの未materialize arbitrary nested cwd、非Gitのroot/current/direct-session外nested cwd、およびsnapshot後に初めてmaterializeするpathはsentinelだけで保証したと主張しない。全pre-fix processのcwd/path inventory後に列挙対象へmaterializeしてfenceするか停止・再起動禁止にする外部safe rollout前提を明記する。
- [x] inherited Git scoping envを除去してnested main/linked worktree、real separate-git-dir、empty/non-repository `.git`、stale/dangling gitfile/commondirを検証し、split authorityを作らない。dangling sequence/legacy/migration/reservations/sessions/issue-storeとsession symlinkも推測・追跡せずfail-closedにする。
- [x] non-Git予約後の`git init`はcaller/current/登録済み全worktree rootのmaterialize済みfallback authorityを検出し、Git authorityに書かずoffline reconciliationを要求する。authority classification変更はcached allocatorのquiescenceを外部gateとし、absence checkだけでlive writerをfenceしたとは主張しない。
- [x] 実 Git sibling worktree の concurrent create と nested production MCP linked-worktree create が同じ authority を使う。
- [ ] full required gates / coverage 100% / link check / 独立 review を完了する。

## 必須回帰テスト

- seeded duplicate に対する store read/write/remove の typed error、sorted exact paths、source/derived state 不変。
- core usecase get/update/delete の ambiguity 伝播と全 sibling byte-for-byte 不変。
- MCP `issue_get` / `issue_update` / `issue_delete` が実行エラーとして ambiguity を返し、source を変更しないこと。
- list/search が sibling を暗黙に collapse せず観測可能であること。
- v1 authority preseed `last_reserved=515` → v2 next 516、旧 v2 next がより大きい migration、abandoned marker、marker 後の crash point。
- same-content retry は marker/sequence を進めず、source write failure は先行予約を消費すること。
- 実 Git sibling worktree の並行 create と production MCP が Git-common authority を共有すること。
- 実subprocess上のold-v2 compatibility emulatorがlegacy lockを先行取得する順序と、new allocatorのlockを待つ逆順、new/v1先行時のsentinel-first、legacy先行時のblocker-first、blocker中の旧v1、移行後の旧v1→fixed v2連番、全write/crash boundary。emulatorは直前HEADのraw-cwd resolver / filename最大 / lock / reserveを再現し、historical binaryそのものとは表現しない。
- pre-fix commit `677405d31267e9205b76a26fe8b31098b6086852`からbuildした実`usagi 2.6.0` MCP（binary SHA-256 `0df8893ed74bab59f92714db72728452569a17f746958bcac2b0fbcc934e3b77`）を同一processで維持するrollout試験。旧create #1の後にfixed create #2がlegacyを`migrated-to-usagi-issue-numbers:2`へfenceし、同じ旧processの再createが`invalid issue sequence`で失敗し、全source/index/authority artifactがbyte-for-byte不変であること。
- 非 Git session A/B と後発 C の store-local high-water、known missing/複数未封鎖 authority の byte-for-byte failure、別nested cwdのtracked/untracked/ignored legacy discovery、nested main/linked、real separate-git-dir、inherited Git env、empty/stale/dangling Git indirection とauthority/source pathの zero-effect failure。
- corrupt/unreadable direct claim と filename/declared 両側 mismatch の store/usecase effect-zero。

## スコープ外

現存する #323/#390 の renumber/delete や自動修復は行わない。履歴監査なしに正しい identity を推測しない。production code change は v2 allocator/IssueStore に限定するが、v1 authority format と live v1 writer の compatibility/fencing は本 issue の受入範囲とする。過去 cleanup の #335 と v1 CRUD 修正 #471 は related として参照し、v1 CRUD 自体は変更しない。
