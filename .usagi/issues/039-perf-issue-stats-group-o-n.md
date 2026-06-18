---
number: 39
title: perf: issue stats の group バケット振り分け O(n²) を解消する
status: todo
priority: medium
labels: [perf, core]
dependson: []
related: []
created_at: 2026-06-17T22:50:45.207897+00:00
updated_at: 2026-06-17T22:50:45.207897+00:00
---

## 背景

`src/usecase/issue/stats.rs:94-109` の `group` は、各 item ごとに `buckets.iter_mut().find(...)` でバケットを線形検索する。

```rust
for item in items {
    let (key, label) = group_key(&item, axis);
    match buckets.iter_mut().find(|(k, _, _)| *k == key) {  // 各 item で線形走査
        Some((_, _, group)) => group.push(item),
        None => buckets.push((key, label, vec![item])),
    }
}
```

`GroupBy::Milestone`/`Parent` のようにキー種類が多い（最悪 n に近い）場合 O(n²) になる。

あわせて `group_key`（`stats.rs:122-138`）が各 issue ごとに `format!("{rank}")` / `format!("0{p:08}")` などソートキー用の String を確保している。status/priority は固定値なのに毎回ヒープ確保。

## 改善方針

- `BTreeMap<String, (String, Vec<ListedIssue>)>` でキー → バケットを引く。BTreeMap ならキー順に取り出せるので末尾のソートも不要。
- ソートキーは `(u8, ...)` の数値タプルや enum discriminant にして文字列確保をなくす。表示ラベルだけ String にする。

## 確認方法

- 大量 issue でのグループ集計が線形時間になること。
- 既存テストが通ること（カバレッジ 100% 維持）。
