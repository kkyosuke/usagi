---
number: 729
title: feat(orchestration): Verified Done と Evidence Bundle を提供する
status: todo
priority: high
labels: [product, orchestration, supervisor, verification, evidence, differentiation]
dependson: []
related: [324, 327, 730, 731, 732]
created_at: 2026-09-02T23:10:15+00:00
updated_at: 2026-09-02T23:35:47.566271+00:00
---

## 目的

usagi の差別化軸を「Agent が完了を申告した」から「設定された品質条件を daemon が検証し、
証拠付きで完了を示せる」へ進める。

workspace ごとの verification policy、supervisor の verification gate、PR / CI 状態、実行結果を
exact run / task / commit に束縛し、最終結果を人間と Agent の双方が読める **Evidence Bundle** として提供する。

## 背景

#324 と #327 により durable supervisor、execution policy、artifact verification gate の基盤は実装済みである。
一方、現在の利用者向け surface は run の進捗・停止理由・retry / cancel / fail が中心であり、次が一つの
product flow になっていない。

- repository が要求する test / lint / coverage / CI context の宣言
- Agent の自己申告とは独立した daemon-authoritative verification
- どの worktree / commit / PR に対して何を検証したかの provenance
- 完了根拠をまとめた機械可読・人間可読な出力
- verification failure を bounded retry または human escalation へ戻す経路

Orca の editor / browser / mobile を全面的に追うのではなく、terminal-native な Agent control plane として、
検証可能性・統制・復旧性を製品上の強みにする。

## スコープ

### Verification policy

- repository が version 管理できる、versioned な verification policy の正本を定義する。
- policy は最低限、名前付き local command gate、必要な PR / CI context、各 gate の必須 / 任意を表現できる。
- Agent が任意の raw command を「検証済み」と自己申告する形にせず、repository が許可した gate ID を
  daemon が解決する。
- policy 未設定時は既存 workflow と後方互換にする。
- malformed / future version / scope mismatch は新規 verification admission を fail closed にし、既存 run の
  read / status / recovery を壊さない。

### Evidence の収集と束縛

- command result、PR state、CI context、artifact verification result を共通 evidence model に正規化する。
- evidence を supervisor run、task、dispatch、workspace / session / worktree、commit SHA、PR head SHA、
  policy revision に束縛する。
- stale commit、別 worktree、別 retry generation、別 PR head の結果は現在 task の完了条件を満たさない。
- command の exit status、開始・終了時刻、bounded な要約、検証対象 identity を保存する。
- stdout / stderr、environment、credential、secret reference の値を無制限または平文で durable state に保存しない。

### Supervisor との統合

- required evidence がすべて成功した場合だけ task / run を Verified Done にできる。
- pending evidence は成功として扱わない。
- failure は既存 execution policy の範囲で bounded retry へ戻し、上限到達または回復不能時は理由付き
  escalation にする。
- daemon restart、connection replay、重複 result で同じ gate を二重実行・二重完了させない。
- Agent の completion report は verification の契機または参考情報にはできるが、それ自体を required evidence の
  成功として扱わない。
- PR の自動 merge は行わない。

### Evidence Bundle

- run の最終状態について JSON と Markdown の safe projection を提供する。
- bundle は少なくとも Goal、task / worker の結果、対象 commit / PR、要求 gate、各 outcome、未確認事項、
  停止理由を含む。
- MCP から取得でき、TUI の Work Run 詳細から閲覧・コピーできる。
- terminal output や provider-native ID をそのまま露出せず、件数・サイズ・行数を bound する。
- failed / cancelled / escalated run も、確認できた evidence と未達条件を bundle として残す。

## 受け入れ条件

- workspace の versioned policy から required gate が決定され、同じ policy revision と対象 identity に対する
  再実行は冪等に収束する。
- Agent が成功を報告しても required gate が pending / failed なら run は成功にならない。
- commit または PR head が変わると古い evidence は stale になり、新しい対象の成功として再利用されない。
- daemon restart 後も evidence、未完了 gate、retry / escalation 状態を復元し、二重 effect を起こさない。
- gate failure は policy 上限内の retry、上限外の escalation、明示 cancel のいずれかへ deterministic に収束する。
- JSON / Markdown bundle が同じ authoritative state から生成され、secret、raw credential、provider-native ID を含まない。
- TUI と MCP が同じ run revision の進捗・失敗理由・Evidence Bundle を表示する。
- policy 未設定の既存 classic / goal-driven workflow、既存 supervisor MCP、session lifecycle を破壊しない。
- 実装と同じ変更で仕様ドキュメントと README の利用者向け説明を更新する。
- unit / integration / restart-replay test を追加し、workspace coverage 100% を維持する。

## 非目標

- Monaco editor、embedded browser、mobile client の実装。
- PR の自動 merge。
- Agent attribution の行単位記録。
- role ごとの tool / path / secret / cost budget 全般。これは verification policy と接続可能な別 issue とする。
- 複数 Agent の自動比較・勝者選定（Verified Agent Tournament）。
- 複数 repository をまたぐ release train。
