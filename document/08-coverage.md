# 8. coverage exclusion inventory

> [ドキュメント目次](README.md) ｜ ← 前へ [7. MCP サーバ](07-mcp.md) ｜ 次へ → [9. 環境変数設定](09-env.md)

v2 の `#[coverage(off)]` の移行 inventory。許可条件と更新手順の正本は
[6. 開発規約](06-conventions.md#coverageoff-例外)、symbol 単位の機械可読な正本は
[`coverage-off-allowlist.json`](../coverage-off-allowlist.json) である。

## 目次

- [基準値](#基準値)
- [領域別返済順序](#領域別返済順序)
- [TUI の返済結果](#tui-の返済結果)
- [root・CLI の返済結果](#rootcli-の返済結果)

## 基準値

2026-07-21 の inventory 開始時点では v2 に 892 件あり、#485–#487 と後続の root・CLI 返済で一度は
129 件まで減った。その後の実装で例外が再び増え、2026-08-27 の source scan は 484 件である。inline metadata や
allowlist 登録は item の責務が許可理由に適合する証明ではないため、増加分を返済済みとは扱わない。

[`coverage-off-budget.json`](../coverage-off-budget.json) が owner / path ごとの現在件数を列挙する機械可読な
inventory である。`scripts/coverage-off-lint.rb` は source scan と全件一致することを検証し、属性の追加・削除・移動で
inventory が更新されていない変更を拒否する。budget の更新は件数変更を review 上で明示するためのもので、増加を
正当化するものではない。

| owner | 件数 | 返済先 |
|---|---:|---|
| core | 9 | parser / validation / persistence 判断を優先して再審査 |
| daemon | 335 | `src/runtime/daemon.rs` の composition 集中を最優先で分離・再審査 |
| root・CLI | 21 | process / stdio 合成と pure helper を分離して再審査 |
| TUI | 119 | presentation / production adapter と reducer の境界を再審査 |
| **合計** | **484** | 詳細は machine-readable inventory に全 path と件数を列挙 |

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

#487 では controller reducer、Effect routing、presentation 分岐を coverage 対象へ戻したが、その後の実装を含む
現行 scan は owner `tui` を 119 件数える。理由付き metadata があっても自動的に許可済みとはせず、controller reducer、
Effect executor、entry selection、completion、input classifier、error projection に例外を残さない方針で再審査する。
production graph の検査方法は [Production screen graph harness](03-tui.md#production-screen-graph-harness) を参照する。

## root・CLI の返済結果

#625 で `crates/cli/src/**` の `migration_debt` 22 件を返済し、CLI/MCP の parser、route selection、
schema projection、caller policy、error mapping を coverage 対象へ戻した。cwd、global/workspace settings、
runtime executable snapshot を束ねる `serve_with_client` を `composition` 例外として残す。また、shipping binary と別に
単相化される issue adapter の `cfg(test)` instance だけを `generic_monomorphization` 例外とし、shipping instance は coverage
対象に保つ。direct unit と production E2E の双方で parser・store error・projection を検証する。

#626 は root 側の `migration_debt` 14 件を返済し、判断と error mapping を coverage 対象へ戻した。現行 scan は
owner `root-cli` を 21 件数えるため、過去の返済結果を現在の残存許可数としては使わない。path 別の現在件数は
`coverage-off-budget.json` を正本とし、production の process・filesystem・環境を束ねる item と pure helper を
再分離する。
