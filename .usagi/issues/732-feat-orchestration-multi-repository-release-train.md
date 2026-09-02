---
number: 732
title: feat(orchestration): dependency-aware な Multi-repository Release Train を統括する
status: todo
priority: medium
labels: [product, orchestration, supervisor, release, multi-repository, differentiation]
dependson: [729]
related: [324, 327, 730]
created_at: 2026-09-03T08:34:08+09:00
updated_at: 2026-09-03T08:34:08+09:00
---

## 目的

複数 repository にまたがる互換変更・version bump・検証・PR・承認を dependency-aware な一つの
durable Release Train として計画・観測し、部分失敗から安全に再開できるようにする。

## 背景

library、CLI、service、documentation などが別 repository に分かれると、個別 PR が green でも、依存先の
release identity、更新順序、consumer verification が揃っている保証はない。人手のチェックリストは
どの commit / PR / artifact で何を確認したかが曖昧になり、途中失敗・担当交代・daemon restart に弱い。

usagi の durable supervisor と #729 の Evidence Bundle を repository graph へ拡張し、単なる複数 worktree の
同時実行ではなく、依存関係と承認点を持つ release coordination を提供する。

## スコープ

### Release Train specification

- train ID、参加 repository の canonical identity、base / target branch、dependency edge、期待 version / artifact、
  required gate、approval point を持つ versioned specification を定義する。
- repository graph を検証し、cycle、重複 identity、未解決 dependency、権限不足、上限超過を開始前に拒否する。
- dry-run で実行順序、fan-out、required approval、想定する PR / artifact、未充足 capability を表示する。
- repository ごとに独立した workspace / managed session / supervisor task を使い、scope と credential を混在させない。

### Dependency-aware orchestration

- upstream の verified artifact / release candidate identity が確定してから dependent repository の更新・検証を開始する。
- 各 node を exact repository / commit / PR head / policy revision / dependency artifact identity に束縛する。
- parallel に実行可能な node は policy の範囲で並行化し、dependency 未充足 node は pending のまま effect を実行しない。
- PR 作成、approval、tag / publish など irreversible または外部影響のある step は明示 policy と human approval を要求する。
- provider-specific な PR / CI / package registry 操作は capability adapter とし、未対応 provider を安全に報告する。

### Failure / recovery / evidence

- node failure、approval timeout、PR head 変更、artifact 失効を train 全体へ伝播し、影響 node を blocked / stale にする。
- retry は失敗 node とその downstream に限定し、成功済みの独立 node や外部 effect を無条件に再実行しない。
- cancel は新規 effect を止め、既に作成した PR / tag / artifact を列挙する。自動で破壊的 rollback しない。
- daemon restart、重複 webhook / completion、late CI result から train state を復元し、二重 PR / tag / publish を防ぐ。
- #729 の repository 単位 Evidence Bundle を集約し、dependency graph、全 gate、approval、未達条件、
  最終 release identities を JSON / Markdown で出力する。

### 利用者 surface

- TUI に train graph、critical path、repository ごとの state、stale / blocked reason、approval、Evidence Bundle を表示する。
- MCP から validate / dry-run / start / get / list / cancel / resume と node detail を操作・取得できる。
- secret、credential、private repository URL、raw CI output、provider-native ID を safe projection に含めない。

## 受け入れ条件

- 同じ specification revision と repository snapshot から同じ DAG・実行順序・approval point が得られる。
- dependency の verified identity が確定するまで downstream effect が実行されない。
- commit / PR head / artifact identity の変更で依存する evidence が stale になり、再検証なしに train が完了しない。
- restart、retry、duplicate / late event で PR、tag、publish、budget consumption が二重実行されない。
- 部分失敗時に影響範囲と安全な retry 対象が示され、成功済みの独立 node は保持される。
- cancel / failure 時に残存する外部 artifact と必要な手動 cleanup が Evidence Bundle に記録される。
- TUI、MCP、JSON / Markdown bundle が同じ train revision と node state を表示する。
- 単一 repository の既存 session / supervisor / release workflow を破壊しない。
- 仕様ドキュメントと README の利用例を更新し、DAG / adapter fake / restart-replay integration test と
  workspace coverage 100% を維持する。

## 非目標

- monorepo build system や package manager の置き換え。
- あらゆる registry / deployment provider の初期実装。
- 破壊的な自動 rollback や既存 release / tag の自動削除。
- repository 間で credential / secret value を同期すること。
- production deployment platform 全般。
