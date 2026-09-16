---
number: 756
title: fix(daemon): 削除した session の workflow record を回収する
status: todo
priority: medium
labels: [v2, daemon, workflow, retention]
dependson: []
related: [752]
created_at: 2026-09-13T00:00:00+00:00
updated_at: 2026-09-13T00:00:00+00:00
---

## 問題

`session remove` は worktree と lifecycle の記録を撤去するが、`<data dir>/daemon/workflows/<workspace>/<session>.json`
を消す経路が無い。record は永久に残り、常駐 lane は sweep のたびにそれを列挙して読む（worktree を解決できないため
読み飛ばされる）。

件数は session を作り消すたびに増えるので、長く使う workspace ほど「毎 tick 読むだけの無駄なファイル」が溜まる。

## 方針

- session の撤去（teardown 完了）で、その session の workflow record も回収する。
- あるいは retention GC / lane の sweep が「lifecycle にもう存在しない session の record」を回収する。
  PR inventory の `retain_sessions` と同じ考え方で、durable な session 集合を権威にする。
- 進行中の run を持つ record を、session が一時的に解決できないだけの状況で消さない。回収の条件は
  「session が lifecycle から消えている」ことであり、「いま worktree を解決できない」ことではない。

## 受入条件

- [ ] 削除した session の workflow record が回収され、sweep の列挙から消える。
- [ ] lifecycle に存在する session の record は、worktree が一時的に解決できなくても残る。
- [ ] 回収が他の run の進行を止めない。
- [ ] 回収を確認する test がある。
