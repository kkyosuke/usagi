# 8. coverage exclusion inventory

> [ドキュメント目次](README.md) ｜ ← 前へ [7. MCP サーバ](07-mcp.md)

v2 の `#[coverage(off)]` の移行 inventory。許可条件と更新手順の正本は
[6. 開発規約](06-conventions.md#coverageoff-例外)、symbol 単位の機械可読な正本は
[`coverage-off-allowlist.json`](../coverage-off-allowlist.json) である。

## 目次

- [基準値](#基準値)
- [領域別返済順序](#領域別返済順序)
- [TUI の返済結果](#tui-の返済結果)
- [root・CLI の内訳](#rootcli-の内訳)

## 基準値

2026-07-21 の inventory 開始時点では v2 に 892 件あり、後続変更で 1 件が加わった。#485 で core の 220 件、
#486 で daemon の decision logic と重複 exclusion 130 件、#487 で TUI の `migration_debt` 487 件を返済した。
残る 129 件は registry 48 件（daemon 10、root・CLI 38）と、理由・代替テストを source に併記した inline metadata
81 件（daemon 6、TUI 75）である。例外はすべて 2027-01-31 に期限切れとなる。

| owner | 件数 | 返済先 |
|---|---:|---|
| TUI | 75 | #487 で返済済み。理由付き inline 例外のみ |
| daemon | 16 | #486 で返済済み。real IO / composition の理由付き例外のみ |
| root・CLI | 38 | 下表で許可候補と削除対象を review |
| **合計** | **129** | registry entry または source inline metadata に全 symbol を列挙 |

## 領域別返済順序

返済は business regression を隠す範囲が広い順に行う。

1. #485 で core の domain reducer、parser、persistence error path を coverage 対象へ戻した。
2. #486 で daemon の reducer、reconcile、routing、error path を戻し、real socket / PTY syscall と composition の理由付き例外だけを残した（完了）。
3. #487 で TUI controller、Effect routing、presentation 分岐を戻し、real terminal IO などの理由付き例外だけを残した（完了）。
4. root・CLI は下表の順序で pure decision を削除対象、薄い実 IO / composition を許可候補として再審査する。

各返済 PR は fake / integration test を先に追加し、属性と registry entry を同じ PR で削除する。例外を残す場合は
`migration_debt` を許可理由へ変更し、`tests` に代替テスト名を記録する。件数を減らしても registry entry を消し忘れると
stale symbol として CI が失敗する。

## TUI の返済結果

TUI の残存 75 件は理由付きの `composition`、`real_io`、`generic_monomorphization` 例外である。
各属性の inline metadata が理由・owner・期限・証拠 test の正本となる。
controller reducer、Effect executor、entry selection、completion、input classifier、error projection には例外を残さない。
production graph の検査方法は [Production screen graph harness](03-tui.md#production-screen-graph-harness) を参照する。

## root・CLI の内訳

| path | 件数 | review 先 |
|---|---:|---|
| `crates/cli/src/**` | 22 | command/MCP parser・error mapping は削除対象。stdio / process 境界だけ `real_io` 候補 |
| `src/main.rs` | 1 | ロジックを持たない composition なら `composition` 候補 |
| `src/runtime/bootstrap.rs` | 6 | 設定判断を coverage 対象へ戻し、process composition だけ候補 |
| `src/runtime/cli.rs` | 2 | CLI routing を coverage 対象へ戻し、実行面の束縛だけ候補 |
| `src/runtime/clipboard.rs` | 3 | platform process IO だけ `real_io` 候補 |
| `src/runtime/launchd.rs` | 5 | plist 生成・判断は削除対象、launchd process IO だけ `real_io` 候補 |
| **合計** | **39** | owner `root-cli`、期限 2027-01-31 |
