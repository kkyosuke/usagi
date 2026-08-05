---
number: 658
title: docs(daemon): restart の seamless rollover と --force の破壊性を案内へ反映する
status: todo
priority: high
labels: [review, daemon, docs, safety, recovery]
dependson: []
related: [507, 559, 574]
parent: 654
created_at: 2026-08-05T01:16:04.999100+00:00
updated_at: 2026-08-05T01:16:04.999100+00:00
---

## 問題

`README.md` と `document/01-overview.md` は、live Agent / generic terminalがあると通常の`usagi daemon restart`を拒否し、進めるには`--force`が必要であるように説明している。

しかし現在の実装とdaemon正本は次の契約である。

- live runtime 0: planned restartはcold transition。
- live runtimeあり + seamless前提成立: standbyをstageしてgated seamless rolloverし、old generationのPTYを維持する。
- live runtimeあり + seamless不可: effect zeroでtyped refusal。
- `--force`: PTYを明示的に破棄するcold transition。

誤った案内に従って利用者が不要な`--force`を付けると、維持できたAgent/terminalを失う。

## 対象

- `README.md` のCLI表直後のstop/restart説明。
- `document/01-overview.md` の`daemon restart`行。
- 必要に応じてCLI help、refusal message、運用例。

`document/05-daemon.md#planned-replacement` を挙動のSSoTとし、別のtransition表を新設しない。

## 受け入れ条件

- `stop` と `restart` を区別する。stopはlive runtimeを既定拒否し、restartは可能ならseamless rolloverする。
- `--force` が「retryを強めるflag」ではなく、old daemonのlive PTYを破棄するcold transitionであることを明記する。
- seamless refusal時は、まず通常restartのrefusal理由を解消する案内を出し、`--force`を無条件な推奨にしない。
- `daemon replace` / development build mismatchとの違いも正本へのリンクで説明する。
- README contract testまたは同等のdocs regressionで、危険な旧文言が戻らないよう固定する。
- Markdown link checkを通す。

## 非目標

replacement実装、generation registry、rollover IPCの変更。これらは実装済み契約を維持する。
