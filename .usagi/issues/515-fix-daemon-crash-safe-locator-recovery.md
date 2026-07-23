---
number: 515
title: fix(daemon): crash 後の locator temporary を回復して状態を fail-closed に判定する
status: done
priority: high
labels: [v2, daemon, lifecycle, ipc, safety]
dependson: [514]
related: [341, 507, 513, 516, 518, 528]
created_at: 2026-07-23T09:24:39+00:00
updated_at: 2026-07-23T09:47:00+00:00
---

## 問題・影響

daemon の current locator は private temporary を write / fsync して `current.json` へ atomic rename するが、
process crash が create 後かつ rename 前に発生すると固定名 `.current.json.tmp` が残る。後続 daemon は
`create_new` に失敗して endpoint を publish できず、正しい owner process identity を確認できても復旧できない。

一方、crash 後に残った `current.json` と generation socket は、locator の状態だけを根拠に削除してはならない。
endpoint が到達不能でも owner process が生存している可能性があり、所有者を推測して回収すると split-brain を起こす。
#516 の generation registry / standby handoff は、#514 の exact process identity と組み合わせられる、
副作用のない crash-safe locator condition を必要とする。

## 修正方針

- locator publish の cross-process lock を取得した後、atomic rename 前の crash で残った固定 temporary を回収してから
  `O_NOFOLLOW | create_new` で作り直す。
- temporary が symlink の場合もリンク先を辿らず directory entry だけを unlink する。
- current locator を `absent` / `live` / `stale` に分類する副作用のない primitive を Unix transport adapter に置く。
- locator / endpoint の型、owner、mode、symlink、generation directory confinement は従来どおり fail-closed に検証する。
- `stale` は回収許可ではない。#516 は #514 の exact owner process identity と組み合わせて初めて migration /
  replacement を判断する。本 issue は locator や socket を自動削除せず、generation CAS / handoff ordering も担当しない。

## 受入条件

- [x] crash が残した `.current.json.tmp` が次回 publish を阻害せず、新 locator を atomic publish できる。
- [x] stale temporary symlink のリンク先を変更・削除しない。
- [x] locator absence、到達可能 endpoint、missing / unreachable endpoint をそれぞれ `absent` / `live` / `stale`
  と判定できる。
- [x] malformed / unsafe locator と generation directory 外または unsafe permission の endpoint は error になり、
  `stale` へ弱めない。
- [x] condition 判定は locator / endpoint を変更せず、owner death を推測して回収しない。
- [x] Unix transport と daemon data directory の v2 正本 docs を実装へ整合する。

## 必須回帰テスト

- crash temporary file を事前配置して publish、locator 読取、接続が成功する。
- temporary symlink を事前配置し、リンク先の内容を保持したまま publish が成功する。
- absent / live / probe failure / missing endpoint の condition と非変更性を固定する。
- unsafe locator、unsafe endpoint、generation directory 外 endpoint が fail-closed になる。

## docs / gate

`document/04-ipc.md` の Unix transport と `document/05-daemon.md` の daemon data directory を更新する。
Rust / filesystem / Unix IO に影響するため、selected daemon tests、fmt、workspace check / clippy を実行し、
full test / coverage 100% は PR CI の必須 gate とする。
