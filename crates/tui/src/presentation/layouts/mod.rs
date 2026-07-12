//! 画面領域のレイアウト。ペイン分割（ホーム画面の左右ペインなど）と chrome
//! （枠・ヘッダ・フッタ・ステータス/キーヒント行）の配置計算を置く。
//! [`super::views`] が layout で領域を割り、その領域へ [`super::widgets`] を
//! 配置する。
//!
//! [`mascot_screen`] は、マスコット＋タイトルを頂きボディを垂直中央寄せしフッタを最下行に
//! 固定する全画面 view（welcome / config …）の共通 chrome で、各画面がマスコットを同じ配置で
//! 出せるようにする。

pub mod mascot_screen;
