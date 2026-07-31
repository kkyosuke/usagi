---
number: 618
title: fix(store): issue summary の file を実ファイル名で報告する
status: todo
priority: medium
labels: [store, issue, persistence, v1, v2, correctness]
dependson: []
related: [617, 117, 113]
created_at: 2026-07-31T23:45:08.418037+00:00
updated_at: 2026-07-31T23:45:08.418037+00:00
---

## Finding（correctness / SSoT）

`IssueSummary.file`（`issue_search` / `issue_get` の `file`、`.usagi/issues/index.json` の `file`）は `Issue::file_name()` ＝ `{:03}-{slugify(title)}.md` の**派生値**であり、store に実在する path ではない。

現在の backlog 554 件のうち **29 件で `file` が存在しないファイルを指している**（実測）。例:

| 番号 | `file` が返す名前 | 実ファイル |
|---|---|---|
| 605 | `605-fix-daemon-worktree-git-command-inherited-repository.md` | `605-fix-daemon-git-command-environment-confinement.md` |
| 615 | `615-ci-main-ruleset-full-test-coverage-markdown-gate.md` | `615-ci-require-all-governance-gates.md` |

`file` は `usecase::issue::view` を通って MCP / CLI の出力に出るため、agent や coordinator は実在しない path を掴む。#617 の `session_delegate_issue` false negative はこの派生値を path として使ったことが直接の原因で、#617 は delegate 側で番号解決に切り替えて回避するが、**`file` が嘘をつく構造そのもの**は残る。

構造的な問題は、`Issue::file_name()` が「write 先を決める canonical 名」と「読み手に報告する名前」の 2 役を兼ねていることである。store は読み取り時に実 path を知っている（`MarkdownStore::files_for_key` / `entry_files` の scan path）のに、summary 生成時に捨てている。

派生名と実ファイル名がずれる経路は次の 2 つで、どちらも正当な運用である。

- store を経由せず markdown を直接書いて起票する（bulk backlog PR。#1373 など）。
- 起票後に title を変更する（次の書き込みまで実ファイル名は古いまま）。

## rename 運用についての判断

「実装 session が issue ファイルを rename するのは妥当か」という問いに対する調査結果:

- **session が手で rename しているのではない**。`IssueStore::write_locked` が、書き込みのたびに canonical 名へ書き直して同番号の stale sibling を削除する。session が `issue_update --status in-progress` を 1 回実行するだけで rename が起きる。実装 PR の diff に現れる `{...-old.md => NNN-....md}` はこれである。
- canonical 化そのものは「番号で解決できる」限り無害なので**維持を推奨**する。title を変えたのに永久に古い名前が残るほうが読みにくい。
- その代わり、**読み手には常に実 path を返す**ようにして、canonical 化が起きるまでの間の嘘をなくす。

## 最小修正方針

- summary 生成を「entry から derive」ではなく「読み取った path を持ち回る」形に変え、`IssueSummary.file` に実ファイル名を入れる。`MarkdownStore` の scan / `files_for_key` は既に path を持っているため、`E::summary(entry)` に path を渡す形へ広げる。
- `Issue::file_name()` は **write target を決める canonical 名**として domain に残す（削除しない）。
- `index.json` の `file` の意味が変わるが、`source_fingerprint` が file 名を hash に含めているため実ファイル名が変われば cache は自動で rebuild される。format version を上げるかは実装時に判断する（上げないなら、古い cache が derived 名を返す期間が残る点を確認する）。
- memory store も同じ derive（`Memory::file_name()` ＝ `{name}.md`）を持つ。`name` が file 名そのものなのでずれにくいが、同じ不変条件（`file` が実在する）を test で固定する。
- v2（`crates/core/src/domain/issue/mod.rs` の `summary()`、`crates/core/src/infrastructure/persistence/markdown_store.rs`）も同一の derive を持つため、同じ扱いにして v2 が同じ嘘を引き継がないようにする。

既存 29 件の一括 rename（data migration）は不要。

## テストと受け入れ条件

- title と一致しない名前の issue markdown を置いたとき、`summaries()` / `issue_search` の `file` が**実ファイル名**を返す（現行は derived 名）。
- title を変更する `issue_update` の後、実ファイルが canonical 名へ rename され、`file` も rename 後の名前を返す。古い名前のファイルは残らない。
- index cache 経路（fresh index）と rebuild 経路（index.json 削除 / fingerprint 不一致）の両方で同じ `file` を返す。
- memory store でも `file` が実在するファイルを指す。
- v1 / v2 双方の store test を更新し、カバレッジ 100% を維持する。
