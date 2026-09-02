---
number: 730
title: feat(orchestration): role-based Policy as Code で Agent の権限と予算を統制する
status: todo
priority: high
labels: [product, orchestration, policy, security, governance, differentiation]
dependson: []
related: [327, 629, 631, 644, 729]
created_at: 2026-09-03T08:34:08+09:00
updated_at: 2026-09-03T08:34:08+09:00
---

## 目的

repository が version 管理する Policy as Code により、Agent の role ごとの権限・資源予算・承認境界を
prompt 上のお願いではなく daemon-authoritative な admission / execution policy として強制する。

## 背景

#327 で supervisor の execution policy、budget、retry、human escalation の基盤は実装済みである。
一方、workspace / session / task に起動された Agent が「どの tool を、どの path に、どの secret scope で、
どの程度の時間・並列数・費用まで使えるか」を repository 側で宣言し、実際の effect 境界で統一的に
検証する product flow はない。

#729 の verification policy が「何を満たせば完了か」を定義するのに対し、本 issue は
「完了までに何を実行してよいか」を定義する。

## スコープ

### Versioned policy schema

- repository で version 管理できる policy の正本と schema version を定義する。
- role / workspace / session / task scope ごとに、許可する MCP tool / effect class、read / write path scope、
  command profile、secret reference scope、runtime / model、wall-clock / retry / concurrency / cost budget を表現する。
- wildcard、継承、override の優先順位を deterministic にし、同じ入力から同じ effective policy を導出する。
- malformed、future version、unknown capability、scope mismatch は新規 admission を fail closed にする。
- policy 未設定の既存 workflow は後方互換とし、段階的に opt-in できる。

### Enforcement

- prompt や Agent 自身の自己申告ではなく、daemon の tool / effect admission、Agent launch、supervisor dispatch、
  secret materialization、worktree mutation の各境界で同じ effective policy を検証する。
- deny は effect 実行前に確定し、部分的な filesystem / process / network 副作用を残さない。
- path は canonical workspace / managed worktree identity に束縛し、symlink、相対 path、別 worktree、
  stale session で scope を迂回できない。
- secret は値を policy・durable state・log・IPC に保存せず、許可された reference と用途だけを扱う。
- budget は reservation と確定消費を分け、retry / restart / duplicate request で二重消費または上限回避を起こさない。
- policy で人間承認が必要な effect は既存 decision / escalation flow に接続し、承認前に実行しない。

### Explain / audit / simulation

- allow / deny / approval-required の理由を stable reason code と policy revision で返す。
- secret、raw command、絶対 path、provider-native ID を含まない bounded audit event を残す。
- policy 変更前に代表的な action を評価できる dry-run / explain API を MCP と CLI に提供する。
- TUI から現在 run の effective role、残予算、直近の拒否・承認待ちを安全に確認できる。
- #729 の Evidence Bundle には policy revision と budget summary を含められるようにする。

## 受け入れ条件

- 同じ policy revision、principal、scope、action から同じ判定と reason code が得られる。
- role ごとの tool / path / secret reference / runtime / budget 制約が実 effect 境界で強制される。
- symlink、別 worktree、restart、retry、重複 request で権限・予算を迂回できない。
- approval-required effect は durable decision の明示承認後に一度だけ実行され、deny / expire / cancel は
  副作用なしで終わる。
- policy update 後も進行中 run が参照した revision を追跡でき、新旧 policy を暗黙に混在させない。
- dry-run、MCP、TUI、audit が同じ effective policy と reason vocabulary を投影する。
- policy 未設定の既存 session / Agent / supervisor workflow を破壊しない。
- 仕様ドキュメントと README の利用者向け設定例を更新する。
- unit / integration / restart-replay / security regression test を追加し、workspace coverage 100% を維持する。

## 非目標

- OS 全体を仮想化する新しい sandbox engine。
- secret value の保管・同期機能。
- verification gate / Evidence Bundle 自体の実装（#729）。
- 複数 Agent の比較・勝者選定。
- 複数 repository の release orchestration。
