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
//!
//! 範囲選択も同じバッファが持つ: [`TextInput::select_left`] などが選択アンカーを立て、
//! キャレットとの間を [`TextInput::selection`] が正規化した `start..end` のバイト範囲で返す。
//! 選択中の [`TextInput::insert`] / [`TextInput::backspace`] / [`TextInput::delete_forward`] は
//! まず選択範囲を消してから通常動作へ進み、非選択移動（`move_*`）は選択を解除する。アンカーも
//! `char` 境界に乗るので、選択・置換・削除が複数バイト文字の途中に落ちることはない。

/// キャレット付きの編集可能な 1 行テキスト。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    /// 入力済みテキスト。
    value: String,
    /// キャレット位置。`value` へのバイトオフセットで、常に `0..=value.len()` の
    /// `char` 境界に乗る。
    cursor: usize,
    /// 選択の起点。`None` は選択なし。`Some(anchor)` のとき選択範囲はアンカーと
    /// キャレットの間で、`anchor == cursor` の空選択では [`Self::selection`] が `None` を
    /// 返す。アンカーも `char` 境界に乗るバイトオフセット。
    anchor: Option<usize>,
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
        Self {
            value,
            cursor,
            anchor: None,
        }
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

    /// 選択範囲（正規化した `start..end` のバイト範囲）。選択が無い、または空選択
    /// （アンカー＝キャレット）なら `None`。描画側はこの範囲を反転で塗る。
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// 選択を解除する（Esc や非選択移動から使う）。キャレット・テキストは変えない。
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// 値全体を差し替え、キャレットを末尾に置く。キーボード以外から値が入るとき
    /// （履歴呼び出し、導出された候補、picker で選んだパス）に使う。選択は解除する。
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
        self.anchor = None;
    }

    /// テキストを消し、キャレットを先頭に戻す。選択も解除する。
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.anchor = None;
    }

    /// キャレット位置に 1 文字挿入し、キャレットをその後ろへ進める。選択があれば
    /// まず選択範囲を置換する（削除してから挿入）。
    pub fn insert(&mut self, c: char) {
        self.take_selection();
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// キャレットの手前の 1 文字を削除し、キャレットを戻す。選択があれば選択範囲を
    /// まるごと削除する（キャレットは削除位置）。選択が無く行頭なら何もしない。
    /// 削除したかを返す。
    pub fn backspace(&mut self) -> bool {
        if self.take_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_boundary();
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// キャレット位置の 1 文字を削除する（Del／前方削除）。選択があれば選択範囲を
    /// まるごと削除する。選択が無くキャレットは動かない。行末では何もしない。
    /// 削除したかを返す。
    pub fn delete_forward(&mut self) -> bool {
        if self.take_selection() {
            return true;
        }
        if self.cursor >= self.value.len() {
            return false;
        }
        let next = self.next_boundary();
        self.value.replace_range(self.cursor..next, "");
        true
    }

    /// キャレットを 1 文字左へ。選択は解除する。
    pub fn move_left(&mut self) {
        self.anchor = None;
        self.cursor = self.prev_boundary();
    }

    /// キャレットを 1 文字右へ。選択は解除する。
    pub fn move_right(&mut self) {
        self.anchor = None;
        self.cursor = self.next_boundary();
    }

    /// キャレットを行頭へ。選択は解除する。
    pub fn move_home(&mut self) {
        self.anchor = None;
        self.cursor = 0;
    }

    /// キャレットを行末へ。選択は解除する。
    pub fn move_end(&mut self) {
        self.anchor = None;
        self.cursor = self.value.len();
    }

    /// 選択を 1 文字左へ広げる。アンカー未設定なら現在キャレットに立ててから移動する。
    pub fn select_left(&mut self) {
        self.ensure_anchor();
        self.cursor = self.prev_boundary();
    }

    /// 選択を 1 文字右へ広げる。アンカー未設定なら現在キャレットに立ててから移動する。
    pub fn select_right(&mut self) {
        self.ensure_anchor();
        self.cursor = self.next_boundary();
    }

    /// 選択を行頭まで広げる。アンカー未設定なら現在キャレットに立ててから移動する。
    pub fn select_home(&mut self) {
        self.ensure_anchor();
        self.cursor = 0;
    }

    /// 選択を行末まで広げる。アンカー未設定なら現在キャレットに立ててから移動する。
    pub fn select_end(&mut self) {
        self.ensure_anchor();
        self.cursor = self.value.len();
    }

    /// 選択拡張の前に、アンカーが無ければ現在キャレットへ立てる。
    fn ensure_anchor(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    /// 選択範囲があれば削除してキャレットを範囲先頭へ置き `true` を返す。選択が無い／
    /// 空選択なら削除しない。いずれの場合もアンカーを解除する。編集操作が選択を置換する土台。
    fn take_selection(&mut self) -> bool {
        let removed = if let Some((start, end)) = self.selection() {
            self.value.replace_range(start..end, "");
            self.cursor = start;
            true
        } else {
            false
        };
        self.anchor = None;
        removed
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

    #[test]
    fn a_fresh_input_has_no_selection() {
        let input = TextInput::with_value("abc");
        assert_eq!(input.selection(), None);
    }

    #[test]
    fn select_right_extends_from_an_anchor_at_the_caret() {
        let mut input = TextInput::with_value("abcd");
        input.move_home();
        input.select_right();
        input.select_right();
        // アンカーは 0、キャレットは 2。正規化した範囲は 0..2。
        assert_eq!(input.selection(), Some((0, 2)));
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn select_left_normalizes_when_the_caret_is_before_the_anchor() {
        let mut input = TextInput::with_value("abcd");
        // 末尾から左へ 2 文字選択。アンカー 4、キャレット 2 でも start<end に正規化。
        input.select_left();
        input.select_left();
        assert_eq!(input.selection(), Some((2, 4)));
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn select_home_and_end_grab_to_the_edges() {
        let mut input = TextInput::with_value("abcd");
        input.move_left(); // caret at 3
        input.select_home();
        assert_eq!(input.selection(), Some((0, 3)));
        input.select_end();
        // アンカーは最初の caret(3) のまま。end まで広げると 3..4。
        assert_eq!(input.selection(), Some((3, 4)));
    }

    #[test]
    fn a_collapsed_selection_reports_none() {
        let mut input = TextInput::with_value("ab");
        input.select_left(); // anchor 2, cursor 1
        input.select_right(); // cursor back to 2 == anchor
        assert_eq!(input.selection(), None);
    }

    #[test]
    fn non_selecting_moves_clear_the_selection() {
        for mv in [
            TextInput::move_left as fn(&mut TextInput),
            TextInput::move_right,
            TextInput::move_home,
            TextInput::move_end,
        ] {
            let mut input = TextInput::with_value("abcd");
            input.select_home();
            assert!(input.selection().is_some());
            mv(&mut input);
            assert_eq!(input.selection(), None, "move must drop the selection");
        }
    }

    #[test]
    fn clear_selection_keeps_the_text_and_caret() {
        let mut input = TextInput::with_value("abcd");
        input.select_home();
        let cursor = input.cursor();
        input.clear_selection();
        assert_eq!(input.selection(), None);
        assert_eq!(input.value(), "abcd");
        assert_eq!(input.cursor(), cursor);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut input = TextInput::with_value("abcd");
        input.move_home();
        input.select_right();
        input.select_right(); // "ab" selected
        input.insert('X');
        assert_eq!(input.value(), "Xcd");
        assert_eq!(input.cursor(), 1);
        assert_eq!(input.selection(), None);
    }

    #[test]
    fn backspace_and_delete_remove_the_whole_selection() {
        let mut backspaced = TextInput::with_value("abcd");
        backspaced.select_home(); // whole line selected, caret at 0
        assert!(backspaced.backspace());
        assert_eq!(backspaced.value(), "");
        assert_eq!(backspaced.cursor(), 0);
        assert_eq!(backspaced.selection(), None);

        let mut deleted = TextInput::with_value("abcd");
        deleted.move_home();
        deleted.select_right();
        deleted.select_right(); // "ab" selected, caret at 2
        assert!(deleted.delete_forward());
        assert_eq!(deleted.value(), "cd");
        assert_eq!(deleted.cursor(), 0);
        assert_eq!(deleted.selection(), None);
    }

    #[test]
    fn a_collapsed_selection_falls_back_to_ordinary_editing() {
        // backspace: 空選択はアンカー解除のみで、通常の 1 文字削除に進む。
        let mut input = TextInput::with_value("ab");
        input.select_left();
        input.select_right(); // collapsed at cursor 2
        assert!(input.backspace());
        assert_eq!(input.value(), "a");
        assert_eq!(input.selection(), None);

        // delete_forward: 選択が無ければ行末で no-op。
        let mut end = TextInput::with_value("a");
        assert!(!end.delete_forward());
        assert_eq!(end.value(), "a");

        // backspace: 選択が無く行頭なら no-op。
        let mut start = TextInput::with_value("a");
        start.move_home();
        assert!(!start.backspace());
        assert_eq!(start.value(), "a");
    }

    #[test]
    fn selection_steps_whole_multibyte_characters() {
        let mut input = TextInput::with_value("あい");
        input.select_left(); // select the trailing い (3 bytes)
        assert_eq!(input.selection(), Some((3, 6)));
        input.insert('ん'); // replace the selection without splitting scalars
        assert_eq!(input.value(), "あん");
        assert_eq!(input.cursor(), 3 + 'ん'.len_utf8());
    }

    #[test]
    fn set_value_and_clear_drop_the_selection() {
        let mut replaced = TextInput::with_value("abcd");
        replaced.select_home();
        replaced.set_value("new");
        assert_eq!(replaced.selection(), None);

        let mut cleared = TextInput::with_value("abcd");
        cleared.select_home();
        cleared.clear();
        assert_eq!(cleared.selection(), None);
    }
}
