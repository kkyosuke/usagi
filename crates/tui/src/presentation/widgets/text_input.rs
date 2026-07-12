//! 端末非依存の 1 行テキスト入力バッファ。
//!
//! [`TextInput`] は入力済みテキストとキャレット位置を持ち、あらゆる入力欄が欲しがる
//! 編集操作を実装する: キャレット位置への挿入、前後どちらかの削除、移動（←/→ で 1 文字、
//! Home/End で端）。キャレットは `char` 境界に乗るバイトオフセットなので、複数バイト文字
//! （日本語など）でも正しく動く — 移動・削除は 1 文字単位で、文字の途中に落ちない。
//!
//! 端末 IO を持たないので直接テスト可能で、全画面が append/pop を再実装せず 1 つの編集挙動を
//! 共有できる。描画側は [`TextInput::before`] / [`TextInput::after`] で行を割り、編集位置に
//! キャレットを描く。キー入力の解釈（どのキーを編集操作に写すか）は入力層が整うときに載せる。

/// キャレット付きの編集可能な 1 行テキスト。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    /// 入力済みテキスト。
    value: String,
    /// キャレット位置。`value` へのバイトオフセットで、常に `0..=value.len()` の
    /// `char` 境界に乗る。
    cursor: usize,
}

impl TextInput {
    /// 空の入力。キャレットは先頭。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `value` を初期値とし、キャレットを末尾に置いた入力（続けて打てる状態）。
    #[must_use]
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    /// 入力済みテキスト。
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// `char` 境界に乗ったバイトオフセットのキャレット位置。描画側が行を割って
    /// 編集位置にキャレットを描くのに使う。
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// まだ何も打たれていないか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// キャレットより前のテキスト（描画側は [`Self::after`] と対にして、間に
    /// キャレットのグリフを置く）。
    #[must_use]
    pub fn before(&self) -> &str {
        &self.value[..self.cursor]
    }

    /// キャレットから行末までのテキスト。
    #[must_use]
    pub fn after(&self) -> &str {
        &self.value[self.cursor..]
    }

    /// 値全体を差し替え、キャレットを末尾に置く。キーボード以外から値が入るとき
    /// （履歴呼び出し、導出された候補、picker で選んだパス）に使う。
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
    }

    /// テキストを消し、キャレットを先頭に戻す。
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// キャレット位置に 1 文字挿入し、キャレットをその後ろへ進める。
    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// キャレットの手前の 1 文字を削除し、キャレットを戻す。行頭では何もしない。
    /// 削除したかを返す。
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_boundary();
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// キャレット位置の 1 文字を削除する（Del／前方削除）。キャレットは動かない。
    /// 行末では何もしない。削除したかを返す。
    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.value.len() {
            return false;
        }
        let next = self.next_boundary();
        self.value.replace_range(self.cursor..next, "");
        true
    }

    /// キャレットを 1 文字左へ。
    pub fn move_left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    /// キャレットを 1 文字右へ。
    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary();
    }

    /// キャレットを行頭へ。
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// キャレットを行末へ。
    pub fn move_end(&mut self) {
        self.cursor = self.value.len();
    }

    /// キャレット直前の `char` 境界のバイトオフセット（先頭なら 0）。
    fn prev_boundary(&self) -> usize {
        self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// キャレット直後の `char` 境界のバイトオフセット（末尾なら現在位置）。
    fn next_boundary(&self) -> usize {
        self.value[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::TextInput;

    #[test]
    fn new_input_is_empty_with_the_caret_at_the_start() {
        let input = TextInput::new();
        assert!(input.is_empty());
        assert_eq!(input.value(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn with_value_places_the_caret_at_the_end() {
        let input = TextInput::with_value("hi");
        assert_eq!(input.value(), "hi");
        assert_eq!(input.cursor(), 2);
        assert!(!input.is_empty());
        // derive された Clone / PartialEq / Debug も計測対象なのでここで実行する。
        assert_eq!(input.clone(), input);
        assert!(format!("{input:?}").contains("hi"));
    }

    #[test]
    fn typing_inserts_at_the_caret_and_advances_it() {
        let mut input = TextInput::new();
        input.insert('a');
        input.insert('c');
        input.move_left();
        input.insert('b');
        assert_eq!(input.value(), "abc");
        assert_eq!(input.before(), "ab");
        assert_eq!(input.after(), "c");
    }

    #[test]
    fn backspace_deletes_before_the_caret() {
        let mut input = TextInput::with_value("abc");
        input.move_left();
        assert!(input.backspace());
        assert_eq!(input.value(), "ac");
        assert_eq!(input.before(), "a");
        assert_eq!(input.after(), "c");
    }

    #[test]
    fn backspace_at_the_start_is_a_noop() {
        let mut input = TextInput::with_value("a");
        input.move_home();
        assert!(!input.backspace());
        assert_eq!(input.value(), "a");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn delete_forward_removes_the_character_at_the_caret() {
        let mut input = TextInput::with_value("abc");
        input.move_home();
        assert!(input.delete_forward());
        assert_eq!(input.value(), "bc");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn delete_forward_at_the_end_is_a_noop() {
        let mut input = TextInput::with_value("ab");
        assert!(!input.delete_forward());
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn caret_movement_clamps_at_both_edges() {
        let mut input = TextInput::with_value("ab");
        input.move_right();
        assert_eq!(input.cursor(), 2);
        input.move_home();
        input.move_left();
        assert_eq!(input.cursor(), 0);
        input.move_end();
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn editing_steps_whole_multibyte_characters() {
        let mut input = TextInput::with_value("あい");
        assert_eq!(input.cursor(), 6); // 3 バイト文字 2 つ
        input.move_left();
        assert_eq!(input.cursor(), 3); // あ と い の境目
        input.insert('ん');
        assert_eq!(input.value(), "あんい");
        input.move_home();
        assert!(!input.backspace());
        assert_eq!(input.value(), "あんい");
        input.move_end();
        assert!(input.backspace()); // 末尾の い を消す
        assert_eq!(input.value(), "あん");
    }

    #[test]
    fn set_value_replaces_text_and_parks_the_caret_at_the_end() {
        let mut input = TextInput::with_value("old");
        input.move_home();
        input.set_value("brand new");
        assert_eq!(input.value(), "brand new");
        assert_eq!(input.cursor(), 9);
    }

    #[test]
    fn clear_empties_the_buffer_and_resets_the_caret() {
        let mut input = TextInput::with_value("text");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }
}
