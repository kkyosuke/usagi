//! usagi の端末カラーテーマ — UI が塗る色の *集合* を 1 か所で持つ。
//!
//! view や widget は具体的な色（cyan / green …）ではなく**意味的な役割**（accent /
//! success / danger …）で色を要求し、このモジュールがその役割を具体色へ写す。ここの
//! 対応を変えれば UI 全体の見た目が追随する（色の単一情報源）。
//!
//! 出力は ANSI SGR エスケープを直接組み立てた文字列で、UI フレームワークや `console`
//! 等の外部クレートに依存しない。生成される色付き行は [`super::widgets::display_width`]
//! / [`super::widgets::clip_to_width`] が SGR を 0 桁として読み飛ばすので、幅計算・
//! 切り詰めと矛盾しない。色を実際に出すか（非 TTY では素のテキストにするか）は端末
//! バックエンド（infrastructure）の判断で、この層は常に色付き文字列を組み立てる。

/// `info` 役割の ANSI-256 インデックス。生の bright-blue のようなギラつきなく
/// ハイパーリンクとして読める柔らかい空色。[`LINK_RGB`] に最も近い 256 キューブ色。
const INFO_256: u8 = 75;

/// `feature` 役割（マスコット）の ANSI-256 インデックス。うさぎを表すはっきりしたピンク
/// （`#ff87af` 相当）。16 色の magenta より柔らかく、ピンクとして読める。
const FEATURE_PINK_256: u8 = 211;

/// 端末色。ANSI 16 色の名前付き色と、256 色キューブのインデックスを表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// ANSI cyan（前景 SGR `36`）。
    Cyan,
    /// ANSI green（前景 SGR `32`）。
    Green,
    /// ANSI red（前景 SGR `31`）。
    Red,
    /// ANSI yellow（前景 SGR `33`）。
    Yellow,
    /// ANSI magenta（前景 SGR `35`）。
    Magenta,
    /// 256 色キューブのインデックス指定（前景 SGR `38;5;{n}`）。
    Ansi256(u8),
}

impl Color {
    /// この色を前景に選ぶ SGR パラメータ（`\u{1b}[` と `m` の間に入る文字列）。
    #[must_use]
    fn fg_params(self) -> String {
        match self {
            Color::Cyan => "36".to_string(),
            Color::Green => "32".to_string(),
            Color::Red => "31".to_string(),
            Color::Yellow => "33".to_string(),
            Color::Magenta => "35".to_string(),
            Color::Ansi256(n) => format!("38;5;{n}"),
        }
    }
}

/// 文字装飾のビットマスク。SGR パラメータ（bold=1 / dim=2 / italic=3 / underline=4）と
/// 対応づけて `(bit, "sgr")` の表で持ち、[`Style::paint`] が SGR の昇順で出力する。
const ATTRS: [(u8, &str); 4] = [(1 << 0, "1"), (1 << 1, "2"), (1 << 2, "3"), (1 << 3, "4")];

/// 前景色と文字装飾の組。色そのものは [`Color`]、色でない属性（bold / dim / italic /
/// underline）はビットマスクで持つ。[`Style::paint`] でテキストを SGR で包む。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    fg: Option<Color>,
    /// [`ATTRS`] のビットの論理和。
    attrs: u8,
}

impl Style {
    /// 装飾なし・無色のスタイル。[`Style::paint`] はテキストをそのまま返す。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 前景色を設定する。
    #[must_use]
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    /// 太字にする。
    #[must_use]
    pub fn bold(mut self) -> Self {
        self.attrs |= ATTRS[0].0;
        self
    }

    /// 淡色（dim）にする。
    #[must_use]
    pub fn dim(mut self) -> Self {
        self.attrs |= ATTRS[1].0;
        self
    }

    /// 斜体にする。
    #[must_use]
    pub fn italic(mut self) -> Self {
        self.attrs |= ATTRS[2].0;
        self
    }

    /// 下線を引く。
    #[must_use]
    pub fn underline(mut self) -> Self {
        self.attrs |= ATTRS[3].0;
        self
    }

    /// `text` を SGR エスケープで包んだ文字列を返す。属性も色も無いスタイルでは
    /// `text` をそのまま返す（無駄な `ESC[m` を出さない）。末尾は必ずリセット
    /// (`ESC[0m`) で閉じ、色が後続へ滲まないようにする。
    #[must_use]
    pub fn paint(&self, text: &str) -> String {
        let mut params: Vec<String> = ATTRS
            .iter()
            .filter(|(bit, _)| self.attrs & bit != 0)
            .map(|(_, sgr)| (*sgr).to_string())
            .collect();
        if let Some(color) = self.fg {
            params.push(color.fg_params());
        }
        if params.is_empty() {
            return text.to_string();
        }
        format!("\u{1b}[{}m{text}\u{1b}[0m", params.join(";"))
    }
}

/// ANSI パレット上に写した意味的な色役割。UI は役割で色を要求し、[`Role::color`] /
/// [`Role::style`] が具体色を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 主アクセント: 選択中の行ラベル・見出し・編集可能な値・タブ。
    Accent,
    /// 肯定・成功: 実行中の agent・保存完了・"live" マーカー。
    Success,
    /// エラー・危険: 失敗・破壊的な確認・入力キャレット・選択カーソル（`>`）。
    Danger,
    /// 警告・注意: 通知・待機状態・一時的なヒント。
    Warning,
    /// 装飾アクセント: マスコット・遊びのハイライト・副次的なカウント。
    Feature,
    /// 情報: 新着アイテム・ハイパーリンク。
    Info,
}

impl Role {
    /// この役割に割り当てた具体色（役割→色の単一情報源）。
    #[must_use]
    pub fn color(self) -> Color {
        match self {
            Role::Accent => Color::Cyan,
            Role::Success => Color::Green,
            Role::Danger => Color::Red,
            Role::Warning => Color::Yellow,
            Role::Feature => Color::Ansi256(FEATURE_PINK_256),
            Role::Info => Color::Ansi256(INFO_256),
        }
    }

    /// この役割の色を前景に持つ [`Style`]。`Role::Feature.style().bold()` のように
    /// 装飾を重ねられる。
    #[must_use]
    pub fn style(self) -> Style {
        Style::new().fg(self.color())
    }
}

/// スプラッシュのタイトルを dim から bright へフェードインさせる ANSI-256 green ランプ。
/// 各要素が 1 段。呼び出し側はこの上に全輝度の 1 段を足す。
pub const TITLE_FADE: [u8; 4] = [22, 28, 34, 40];

/// 埋め込み端末内に描くテキストのハイパーリンク色（RGB）。`vt100` の色型から独立させる
/// ため素のタプルで持つ。console 側の [`Role::Info`] はこの色に最も近い 256 キューブ
/// ([`INFO_256`]) を使い、どちらで描いてもリンクが同じ色に見えるようにする。
pub const LINK_RGB: (u8, u8, u8) = (102, 178, 255);

#[cfg(test)]
mod tests {
    use super::{Color, FEATURE_PINK_256, INFO_256, LINK_RGB, Role, Style, TITLE_FADE};

    #[test]
    fn empty_style_returns_text_unchanged() {
        assert_eq!(Style::new().paint("hi"), "hi");
    }

    #[test]
    fn paint_wraps_with_sgr_and_resets() {
        // magenta + bold: 属性が先、色が後。末尾は reset。
        assert_eq!(
            Style::new().fg(Color::Magenta).bold().paint("x"),
            "\u{1b}[1;35mx\u{1b}[0m"
        );
    }

    #[test]
    fn paint_orders_all_attributes_before_color() {
        let styled = Style::new()
            .fg(Color::Cyan)
            .bold()
            .dim()
            .italic()
            .underline()
            .paint("y");
        assert_eq!(styled, "\u{1b}[1;2;3;4;36my\u{1b}[0m");
    }

    #[test]
    fn attribute_only_style_emits_no_color() {
        assert_eq!(Style::new().dim().paint("z"), "\u{1b}[2mz\u{1b}[0m");
    }

    #[test]
    fn roles_map_to_their_colors() {
        assert_eq!(Role::Accent.color(), Color::Cyan);
        assert_eq!(Role::Success.color(), Color::Green);
        assert_eq!(Role::Danger.color(), Color::Red);
        assert_eq!(Role::Warning.color(), Color::Yellow);
        assert_eq!(Role::Feature.color(), Color::Ansi256(FEATURE_PINK_256));
        assert_eq!(Role::Info.color(), Color::Ansi256(INFO_256));
    }

    #[test]
    fn each_named_color_has_its_ansi_foreground_code() {
        assert_eq!(
            Style::new().fg(Color::Cyan).paint("a"),
            "\u{1b}[36ma\u{1b}[0m"
        );
        assert_eq!(
            Style::new().fg(Color::Green).paint("a"),
            "\u{1b}[32ma\u{1b}[0m"
        );
        assert_eq!(
            Style::new().fg(Color::Red).paint("a"),
            "\u{1b}[31ma\u{1b}[0m"
        );
        assert_eq!(
            Style::new().fg(Color::Yellow).paint("a"),
            "\u{1b}[33ma\u{1b}[0m"
        );
        assert_eq!(
            Style::new().fg(Color::Magenta).paint("a"),
            "\u{1b}[35ma\u{1b}[0m"
        );
    }

    #[test]
    fn ansi256_uses_the_extended_foreground_sequence() {
        assert_eq!(
            Role::Info.style().paint("l"),
            format!("\u{1b}[38;5;{INFO_256}ml\u{1b}[0m")
        );
    }

    #[test]
    fn styled_output_width_and_clip_ignore_the_ansi() {
        use crate::presentation::widgets::{clip_to_width, display_width};
        let styled = Role::Accent.style().bold().paint("あいう"); // 全角 3 文字 = 6 桁
        assert_eq!(display_width(&styled), 6);
        // 色付き行を 4 桁に切っても色は保たれ reset で閉じる。
        let clipped = clip_to_width(&styled, 4);
        assert!(display_width(&clipped) <= 4);
        assert!(clipped.ends_with("\u{1b}[0m"));
    }

    #[test]
    fn constants_are_exposed_for_the_splash_and_links() {
        assert_eq!(TITLE_FADE, [22, 28, 34, 40]);
        assert_eq!(LINK_RGB, (102, 178, 255));
        // derive された Debug / Clone / PartialEq も計測対象なので触れておく。
        let s = Role::Accent.style();
        assert_eq!(s, s);
        assert!(format!("{s:?}").contains("Style"));
        assert!(format!("{:?}", Color::Cyan).contains("Cyan"));
        assert!(format!("{:?}", Role::Info).contains("Info"));
    }
}
