---
number: 499
title: fix(v1/config): CLI editor を private copy と CAS で保存する
status: done
priority: medium
labels: [review, v1, cli, config, concurrency]
dependson: []
related: [15, 22, 29, 153]
parent: 453
created_at: 2026-07-20T12:07:08.687712+00:00
updated_at: 2026-07-21T14:01:19.616704+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/presentation/cli/config.rs::edit_config` は live `settings.json` を直接 editor に開き、事前 backup を保持する。編集中の別 process 更新を検出せず、invalid/remove 時には stale backup を `fs::write` して concurrent valid change まで巻き戻す。

## 成立条件 / 再現フロー

CLI editor を開いている間に別 process が settings を更新し、editor を invalid JSON にするか削除する。rollback が editor 開始前の backup を live file に書き戻し、別 process の変更を失う。

## 対象責務と非対象

CLI editor の private temp copy、validate、base revision CAS、conflict/invalid handling を対象とする。TUI/env flow は #498、editor command parsing と settings schema は非対象。

## 受入条件

- [ ] live config ではなく mode 0600 相当の private copy を editor に渡し、編集完了まで shared file を変更しない。
- [ ] candidate を完全 validate 後、base revision が一致する場合だけ atomic/CAS commit する。
- [ ] concurrent change は conflict とし、stale backup/candidate で live file を上書きしない。
- [ ] invalid/remove/editor failure で live file は開始後の最新 valid stateを保持する。

## 必須回帰テスト

editor中 concurrent valid update、invalid candidate、temp削除、same/disjoint field、editor failure、CAS retry を barrier fake editor と実 file で検証する。

## docs / 移行影響

v1 `config --edit` docs に private-copy/conflict workflow を追記する。revision metadata を導入する場合は legacy settings の migration を定義する。
