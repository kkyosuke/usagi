//! 時間の読み取り port。
//!
//! v2 が注入する時計は 3 種類だけである。どれも「何を測るか」が違うので統合せず、
//! 逆に同じ意味の時計を層ごとに別 trait で宣言しない。実装（実時計・process
//! uptime・論理カウンタ）は infrastructure と合成ルートが持ち、この module は
//! 語彙だけを定義する。
//!
//! | trait | 単位 | 原点 | 使いどころ |
//! |---|---|---|---|
//! | [`MonotonicClock`] | ミリ秒 | 任意（差分だけが意味を持つ） | deadline・retry budget・refresh cadence |
//! | [`WallClock`] | [`DateTime<Utc>`] | UNIX epoch | 永続化する時刻・retention の経過判定 |
//! | [`LogicalClock`] | 単調増加カウンタ | 任意 | operation ledger の順序と世代 |
//!
//! 待機（sleep）は時刻の読み取りではないため、この module ではなく
//! `infrastructure::daemon` の `Sleeper` が持つ。

use chrono::{DateTime, Utc};

/// 単調増加するミリ秒の時刻源。観測どうしの差分だけが意味を持ち、原点は任意で
/// 壁時計ではない。deadline 状態機械を制御可能な fake で決定的にし、無関係な
/// 進捗から試行予算がリセットされないように注入する。
pub trait MonotonicClock {
    /// 現在の単調ミリ秒。
    fn now_ms(&self) -> u64;
}

/// 壁時計。永続化する時刻と、経過時間で満了を決める予算が読む。
///
/// daemon の retention authority のように複数 thread が共有する時計があるため
/// `Send + Sync` を要求する。
pub trait WallClock: Send + Sync {
    /// 現在の UTC 時刻。
    fn now(&self) -> DateTime<Utc>;
}

/// 単調増加する論理時刻。production は粗いカウンタか時計に束ね、テストは fake を
/// 注入して各 phase 境界を決定的にする。
pub trait LogicalClock {
    /// 現在の論理時刻。
    fn now(&self) -> u64;
}
