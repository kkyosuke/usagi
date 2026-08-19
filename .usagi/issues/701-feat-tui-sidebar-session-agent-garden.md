---
number: 701
title: feat(tui): sidebar の session 行に Agent 状態行を追加し Garden と語彙を共有する
status: todo
priority: medium
labels: [tui]
dependson: []
related: []
created_at: 2026-08-19T11:16:56.297399+00:00
updated_at: 2026-08-19T11:16:56.297399+00:00
---

## 概要

1 つの session は複数の Agent runtime を持てるが、Home sidebar はそれを 1 行に集約した
lifecycle marker としてしか出しておらず、**「この session に何体の Agent がいて、それぞれ何をしているか」を
一覧から読めなかった**。個々の Agent phase が見えるのは Garden overlay（screen saver）を開いたときだけで、
普段の操作面には出ていない。

さらに、その Agent 群の投影は Garden の中だけに閉じており、sidebar が同じ事実を出そうとすると
2 つ目の projection ができて、同じ session の Agent 数・順序が画面の 2 か所で食い違いうる状態だった。

## やったこと

### 1. 表示語彙の単一情報源（`widgets::agent_status`）

Agent phase の並び順・記号・色・短縮ラベル・件数要約を 1 か所に置き、sidebar と Garden が共有する。

- 注目順 `waiting → running → ready → interrupted → idle → done`、同 phase の tie-break は stable `AgentRuntimeId`。
  phase が変わらない限り並びは frame をまたいで動かない。
- 記号は `◆ ● ○ ◌ · ◦`（BMP の幾何記号。Nerd Font に依存しない）。1 桁固定で列がずれない。
- `status_line(agents, width)` が `◆ ●  1 wait · 1 run` を組み、幅が足りなければ要約を注目度の低い項目から
  落とし、最後まで記号列（＝何体がどの phase か）を残す。記号も入らない幅は `+N` に畳む。

### 2. sidebar: session 行を 3 行にして 3 行目を Agent 行にする

1 行目 まとめ（marker / 名前 / role badge / note icon）、2 行目 変更履歴（相対時刻・Git 差分・PR badge）、
3 行目 Agent（`⚙` + 記号列 + 件数）。Agent が 0 体なら `⚙ —` を出し、「0 体」と「未観測」を空行に潰さない。

**行数は Agent の有無で変えない**。controller の pointer hit-test は runtime-local phase しか持たず、
view が重ねる daemon Agent inventory を知らないため、Agent 依存の行高は click を 1 行ずらす。
`views::workspace` の `SESSION_ROW_LINES` と `controller` の `SIDEBAR_SESSION_ROW_LINES` を
compile-time assertion で縛った（既存の `LEFT_WIDTH` / `CHROME_ROWS` と同じ形）。

### 3. Agent 投影を 1 本に束ねる（sidebar と Garden で共有）

`HomeProjection::session_agents` を `from_ordered_state` で毎 frame 1 度だけ組み、
`with_agent_inventory` が daemon inventory を重ねたあと Garden へ配り直す。
Garden の `GardenAgent` は `agent_status::AgentStatus` の型 alias にして、型ごと共有する。

### 4. Garden への反映

- 並び順を共有の注目順に統一（従来は `Waiting` を先頭にするだけだった）。
- 複数 Agent plot の status 行を共有の `status_line` に置き換えた。うさぎは幅の都合で 3 羽までだが、
  記号列は**畳まれた Agent も含めて全体**の phase を並べるため、`+N hidden` という件数だけの表現より
  「隠れたうさぎが何をしているか」が読める。
- 注目順にした結果、動いている Agent が表示上限に押し出されなくなったので、
  `session_may_animate` の「見えない Agent のために redraw する」ケース自体が消えた（テストを反転）。

## テスト・確認方法

- `cargo test -p usagi-tui`（新規 17 件を含む。`agent_status` の語彙・幅・degradation、
  sidebar 3 行 footprint、Agent なし表示、`failed` / 削除中の footprint、sidebar と Garden の一致、
  Switch の dim 規則、狭幅での桁溢れなし）
- `cargo test --workspace --quiet`
- `cargo clippy --workspace --all-targets -- -D warnings`
- golden fixture（`home_cjk` / `home_git_diffs` / `home_live_terminal`）を 3 行 footprint で更新
- controller の pointer hit-test テストを 3 行 footprint に更新
- `document/03-tui.md` の「Session sidebar rows」「session garden」を更新
