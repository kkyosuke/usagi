# 8. coverage exclusion inventory

> [ドキュメント目次](README.md) ｜ ← 前へ [7. MCP サーバ](07-mcp.md)

v2 の `#[coverage(off)]` の移行 inventory。許可条件と更新手順の正本は
[6. 開発規約](06-conventions.md#coverageoff-例外)、symbol 単位の機械可読な正本は
[`coverage-off-allowlist.json`](../coverage-off-allowlist.json) である。

## 目次

- [基準値](#基準値)
- [領域別返済順序](#領域別返済順序)
- [root・CLI の内訳](#rootcli-の内訳)

## 基準値

2026-07-21 時点の v2 には 892 件ある。issue 起票時の概数 854 件から増えていたため、現在値を symbol と同名 symbol
内の occurrence で固定した。全件を一度 `migration_debt` として扱い、コメントだけから正当性を推測して恒久許可には
しない。registry の全 entry は 2027-01-31 に期限切れとなる。

| owner | 件数 | 返済先 |
|---|---:|---|
| TUI | 487 | #487 |
| core | 220 | #485 |
| daemon | 147 | #486 |
| root・CLI | 38 | 下表で許可候補と削除対象を review |
| **合計** | **892** | `coverage-off-allowlist.json` に全 symbol を列挙 |

## 領域別返済順序

返済は business regression を隠す範囲が広い順に行う。

1. #485 で core の domain reducer、parser、persistence error path を coverage 対象へ戻す。
2. #486 で daemon の reducer、reconcile、routing、error path を戻し、real socket / PTY syscall だけを再審査する。
3. #487 で TUI controller、Effect routing、presentation 分岐を戻し、real terminal IO だけを再審査する。
4. root・CLI は下表の順序で pure decision を削除対象、薄い実 IO / composition を許可候補として再審査する。

各返済 PR は fake / integration test を先に追加し、属性と registry entry を同じ PR で削除する。例外を残す場合は
`migration_debt` を許可理由へ変更し、`tests` に代替テスト名を記録する。件数を減らしても registry entry を消し忘れると
stale symbol として CI が失敗する。

## root・CLI の内訳

| path | 件数 | review 先 |
|---|---:|---|
| `crates/cli/src/**` | 22 | command/MCP parser・error mapping は削除対象。stdio / process 境界だけ `real_io` 候補 |
| `src/main.rs` | 1 | ロジックを持たない composition なら `composition` 候補 |
| `src/runtime/bootstrap.rs` | 6 | 設定判断を coverage 対象へ戻し、process composition だけ候補 |
| `src/runtime/cli.rs` | 1 | CLI routing を coverage 対象へ戻し、実行面の束縛だけ候補 |
| `src/runtime/clipboard.rs` | 3 | platform process IO だけ `real_io` 候補 |
| `src/runtime/launchd.rs` | 5 | plist 生成・判断は削除対象、launchd process IO だけ `real_io` 候補 |
| **合計** | **38** | owner `root-cli`、期限 2027-01-31 |
