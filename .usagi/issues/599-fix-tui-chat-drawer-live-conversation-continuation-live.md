---
number: 599
title: fix(tui): chat drawer の live conversation が continuation 欠落で消え、live 出力まで捨てられる
status: done
priority: medium
labels: [tui, bug, agent]
dependson: []
related: [577, 597]
created_at: 2026-07-31T11:06:29.292562+00:00
updated_at: 2026-07-31T12:23:46.434065+00:00
---

## 症状

chat drawer（指示モード）に live な root Agent が居るのに、conversation selector が
`Conversation [No conversations]`、本文が `No chat conversations yet` / `Chat inventory is not connected.` に
なることがある。このとき PTY 自体は生きていて入力も届いているため、「動いているのに、動いていない表示」になる。

## 原因

`presentation/mod.rs` の `workspace_agent_drawer_projection` は Live tab を次のように投影する。

```rust
PaneTab::Live(live) if live.kind == PaneKind::Agent => {
    let Some(continuation) = ui.agent_continuation_for(&live.terminal) else {
        continue;   // conversation が 1 つ消える
    };
    ...
}
```

`agent_continuation_for` は `AgentTabIntent` の保存済み slot と `visible_agents` からしか continuation を引かない。
intent context が無い / まだ observe できていない / CAS 後の投影が遅れている場合は `None` になり、**live tab が
conversation 一覧から丸ごと落ちる**。

さらに `views/workspace_agent_drawer.rs` の `drawer_body` は本文の分岐を `projection.conversations.is_empty()` で
決めている。conversation が空だと terminal 出力（`terminal_rows`）は描かず empty state を出す。つまり
「continuation が引けない」だけで **live PTY 出力の表示が失われる**。

## 変更方針

- Live tab の投影を continuation の有無に依存させない。continuation が引けないときも conversation 行は出し、
  label は safe な fallback（Agent の起動 profile など、既存の安全な表示語彙から選ぶ）にする。
  `AgentTabIntent::safe_label` に渡せる continuation が無いケースの表示語彙を 1 か所で決めること。
- 本文の分岐を `conversations.is_empty()` から切り離す。**terminal view があるなら常に terminal を描く**。
  empty state は「terminal view も conversation も無い」ときだけにする。
- selected 判定も同様に、continuation ではなく `TabSelection::Live(terminal)` の fence で決める（現状も terminal
  fence を見ているので、continuation 依存の早期 `continue` を外すだけでよい）。

## 対象ファイル

- `crates/tui/src/presentation/mod.rs`（`workspace_agent_drawer_projection`）
- `crates/tui/src/presentation/views/workspace_agent_drawer.rs`（`drawer_body` の分岐）
- `document/03-tui.md`（drawer の conversation 投影に、continuation 欠落時の fallback を明記）

## 受け入れ条件

- intent context が無い / continuation が引けない状態でも、live root Agent tab が conversation として 1 行出る。
- terminal view があるフレームでは、conversation 一覧が空でも terminal 出力が描かれる。
- empty state は terminal view も conversation も無いときだけ出る。
- fallback label に生の path / argv / secret を出さない（既存の safe label 語彙のみ）。

## テスト方針

- `cargo test -p usagi-tui presentation`（continuation を返さない fake intent での投影 test）
- `cargo test -p usagi-tui presentation::views::workspace_agent_drawer`（conversation 空 + terminal view ありで
  出力が描かれる render test）

## 非目標

- `AgentTabIntent` の observe / CAS 設計の変更。
- conversation の並び順・selection 復元規則の変更。
