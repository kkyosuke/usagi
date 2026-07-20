---
number: 470
title: fix(v1/session): reconcile の worktree 所有権判定を fail closed にする
status: todo
priority: high
labels: [review, v1, session, git, security]
dependson: []
related: [49]
parent: 453
created_at: 2026-07-20T12:06:23.049718+00:00
updated_at: 2026-07-20T12:06:23.049718+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/session/reconcile.rs::discard_session` は branch 一致 **または** canonical path containment のどちらかで worktree を対象化し force remove する。特に branch-only arm は同じ branch を使う workspace 外の dirty worktree まで usagi 所有と誤認する。また canonicalize 失敗を raw path へ fallback するため、ownership evidence を得られない場合にも fail closed にならない。

## 成立条件 / 再現フロー

managed session と同 branch の外部 dirty worktreeを作り、対象 root/worktree の canonicalize が失敗する状態や symlink ambiguity も用意して reconcile する。branch-only 判定または raw path fallback により、十分な ownership proof がない対象へ force-remove effect が発生する。

## 対象責務と非対象

recorded repo/worktree provenance、canonical containment、branch の積集合による ownership 判定と曖昧対象の quarantine/report を対象とする。通常 remove transaction は #469、Git 自体の prune 動作変更は非対象。

## 受入条件

- [ ] branch 単独を ownership 根拠にせず、expected repo、recorded identity、canonical containment の必要条件を満たす対象だけを扱う。
- [ ] canonicalization/symlink/schema ambiguity は fail closed にし、force remove せず報告する。
- [ ] workspace 外の same-branch dirty worktree を一切変更しない。
- [ ] genuine crash-stray は十分な provenance がある場合だけ idempotent に回収する。

## 必須回帰テスト

external same-branch dirty worktree、canonicalize failure、symlink ambiguity、別 repo、正規 crash-strayを real Git test で作り、remove effect の exact target set を検証する。

## docs / 移行影響

v1 reconcile/doctor docs に ownership proof と manual cleanup path を記載する。provenance のない既存 stray は自動削除せず warning/quarantine へ移行する。
