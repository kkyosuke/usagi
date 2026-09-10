---
number: 735
title: "fix(daemon): PTY child へ実 effective UID 由来の USER を注入する"
status: done
priority: high
labels: [v2, daemon, env, agent, terminal]
dependson: []
related: []
created_at: 2026-09-11T00:00:00+00:00
updated_at: 2026-09-11T01:00:00+00:00
---

## 問題

daemon が所有する PTY child は起動時に集めた public terminal environment（`SHELL` / `TERM` / `PATH` …）だけを
受け取り、親環境を無差別に継承しない。この allowlist に `USER` が無いため、usagi が起動した Agent /
terminal の child には `USER` が存在しない。

macOS の Claude Code はログイン credential を Keychain へ保存するとき、account の索引に `USER` を使う。
実機で確認した挙動は次のとおりである。

| 起動元 | child の `USER` | Keychain entry |
|---|---|---|
| 端末から直接起動した daemon 環境 | `kyosuke` | 既存の `kyosuke` entry を選ぶ |
| usagi の Agent pane | 未設定 | `unknown` の別 entry を新規作成する |

その結果、usagi の Agent pane で起動した Claude Code (2.1.267) は端末で済ませた認証を再利用できず、
毎回別 entry を作る。

## 方針

- 親 terminal の全環境コピーは**採用しない**。`GH_TOKEN` などの secret が Agent / terminal child へ漏れる。
  既存の明示的 allowlist と secret 分離をそのまま維持する。
- 値の出どころは継承した `USER` ではなく、**daemon 自身の実 effective UID** とする。daemon 起動時に一度だけ
  `getpwuid_r` で OS ユーザー名へ解決し、launch ごとに `id` 等の subprocess を起動しない。
- 解決値を public terminal environment に加える。generic terminal と Claude / Codex / sakana Agent は同じ
  PTY spawn 境界を通るため、root scope と全 managed session の両方へ同じ経路で届く。
- 解決に失敗した場合は fail-safe を既存方針と揃える: 継承した `USER` が妥当ならそれを使い、どちらも無ければ
  その変数だけを落として pane は開く（解決できない binding を落とす env の方針と同じ）。
- 設定 env は従来どおり端末特性より優先するため、利用者が `USER` を明示的に上書きできる。

## 受入条件

- [x] public terminal environment に `USER` が含まれ、値は daemon の実 effective UID から解決した OS ユーザー名である。
- [x] 解決は daemon process ごとに一度で、launch ごとに subprocess を起動しない。
- [x] 解決値は継承した `USER` より優先し、解決失敗時は妥当な継承値へ fallback し、双方無効なら変数を落とす。
- [x] root scope・全 managed session、generic terminal と Agent の共通 PTY 境界の双方へ届く。
- [x] 親環境の無差別コピーを行わず、secret（`GH_TOKEN` / `OP_SERVICE_ACCOUNT_TOKEN` など）は child へ渡らない。
- [x] unit test と実 PTY の integration test があり、coverage 100% を維持する。
- [x] `document/05-daemon.md` の terminal launch environment と `document/09-env.md` を更新する。
