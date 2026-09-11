//! Welcome startup animation playback policy.

use std::io;

use crate::presentation::views::splash;
use crate::usecase::application::{Key, Terminal};

/// Welcome 起動エフェクトを再生し、実際に描いたフレーム数を返す。
///
/// **打鍵で中断できる**。フレーム間の待機は [`Terminal::wait_for_key`] で行い、
/// キーが届いた時点で残りのフレームを捨てて抜ける。中断に使ったキーは
/// **スキップとして消費する**（「何かキーを押すと飛ばせる」の標準的な契約）。
/// これは splash 中に紛れ込んだ端末由来のバイトを次の画面へ流し込まないという
/// 意味でもあり、入力を読まなかった以前の実装よりも取り違えが起きにくい。
/// 起こし待ちの tick と端末リサイズは打鍵ではないため、アニメーションの速度を保つ。
///
/// # Errors
///
/// 端末サイズの取得、描画、フレーム間待機のいずれかに失敗した場合、そのエラーを返す。
pub fn play_startup_splash(term: &mut dyn Terminal) -> io::Result<usize> {
    for frame in 0..splash::FRAMES {
        let (height, width) = term.size()?;
        term.draw(&splash::render(height, width, frame))?;
        match term.wait_for_key(splash::ANIM_TICK)? {
            // 起こし待ちの tick とリサイズは入力ではない。次のフレームは先頭で
            // 端末サイズを読み直すので、リサイズもそのまま追従する。
            None | Some(Key::Other | Key::Resize) => {}
            // それ以外の打鍵は残りのアニメーションをスキップする。
            Some(_) => return Ok(frame + 1),
        }
    }
    Ok(splash::FRAMES)
}

/// 起動スプラッシュの再生権。**1 プロセスで 1 回だけ**再生する。
///
/// workspace を離れて戻ってきた Welcome は「起動」ではないため、2 回目以降の
/// [`Self::play`] は 0 フレームで何も描かない。プロセス内で workspace を切り替える
/// たびに 1.5 秒のアニメーションを見せないための policy であり、合成ルートの都合では
/// なくこの層が持つ（#556）。
#[derive(Debug, Default)]
pub struct StartupSplash {
    played: bool,
}

impl StartupSplash {
    /// まだ再生していない splash を作る。
    #[must_use]
    pub const fn new() -> Self {
        Self { played: false }
    }

    /// 初回だけ splash を再生し、描いたフレーム数を返す。2 回目以降は 0 を返す。
    ///
    /// # Errors
    ///
    /// 再生中の端末操作に失敗した場合、そのエラーを返す。
    pub fn play(&mut self, term: &mut dyn Terminal) -> io::Result<usize> {
        if std::mem::replace(&mut self.played, true) {
            return Ok(0);
        }
        play_startup_splash(term)
    }
}
