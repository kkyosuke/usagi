---
number: 512
title: fix(v2/cli): daemon/MCP argv を副作用前に厳格検証する
status: done
priority: medium
labels: [review, v2, cli, daemon, mcp]
dependson: []
related: [481]
parent: 453
created_at: 2026-07-21T21:34:02.710474+00:00
updated_at: 2026-07-21T21:46:19.564076+00:00
---

## 問題・影響

root/v2 の `src/main.rs` は clap より前に `args[1]` だけで daemon/MCP 面を手動選択する。daemon は `args[2]` だけを文字列で渡して後続 token を捨て、MCP は tail 全体を無視する。さらに `crates/daemon/src/presentation/mod.rs` は未知 verb を stdout に案内して `Ok(())` を返す。

そのため `usagi daemon bogus` は exit 0、`usagi daemon status extra` は `extra` を黙殺して status を実行し、`usagi mcp extra` は usage error にせず daemon bootstrap と stdio server を開始する。引数誤りを automation が成功と誤認し、typo でも data directory・process・socket 等の副作用へ到達する。`document/02-architecture.md` の「引数解析エラーは exit 2、失敗時 stdout 空」という契約とも不整合である。

## 成立条件 / 再現フロー

最新 main の実バイナリを fresh な `USAGI_HOME` で起動する。

- `usagi daemon bogus`: exit 0、stdout に unknown subcommand、daemon directory 作成。
- `usagi daemon status extra`: exit 0、stdout に status、daemon directory 作成。
- initialize を stdin に渡した `usagi mcp extra`: exit 0、MCP initialize 応答を返し daemon を起動。

通常の未知トップレベル command は clap が exit 2、stderr usage、stdout 空で拒否するため、特殊入口だけが契約外である。

## 対象責務と非対象

完全な process argv を副作用前に clap で解析し、合成ルートへ typed な daemon/MCP 起動要求を返す入口面を対象とする。daemon presentation は閉じた verb 型だけを受け取り、未知 verb の成功経路を持たない。daemon lifecycle、MCP JSON-RPC wire semantics、daemon reply failure の exit 1 mapping（#481）の再設計は非対象である。

## 受入条件

- [ ] `daemon` / `mcp` を含む完全な argv が、data directory 解決・daemon bootstrap/process 起動・socket/stdio serve 等の副作用より前に clap で検証される。
- [ ] 未知 daemon verb、引数を取らない valid daemon verb への extra、`mcp` の extra は exit 2、stderr usage、stdout 空で拒否され、副作用を起こさない。
- [ ] daemon presentation の dispatch は閉じた typed verb を受け、未知文字列を stdout＋`Ok` に変換しない。
- [ ] `usagi daemon` / `serve` / `start` / `status` / `stop` / `restart` / `install-service` / `uninstall-service` と、引数なしの MCP stdio server は従来どおり動作する。
- [ ] 通常 CLI と引数なし TUI、`--help` / `--version` の既存契約を壊さない。
- [ ] clap 解析後の daemon failure は #481 の契約どおり exit 1、stderr safe message、stdout 空を維持する。

## 必須回帰テスト

実 `CARGO_BIN_EXE_usagi` process の table-driven test で `daemon bogus`、`daemon status extra`、`mcp extra` を実行し、exit 2・stdout 空・stderr usage・fresh `USAGI_HOME` に runtime side effect が無いことを固定する。typed parser/daemon dispatch の unit test と、既存 daemon lifecycle/MCP stdio success test も通す。

## docs / 移行影響

`document/02-architecture.md` を argv contract の正本として daemon/MCP を含む parse-before-effect と exit/output 契約へ更新し、必要な daemon/MCP 章から参照する。wire/data migration はない。従来余剰引数や未知 daemon verb を成功扱いしていた呼び出しは usage error を処理する必要がある。
