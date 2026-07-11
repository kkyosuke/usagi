//! 合成ルート。実 IO（標準出力）をここで束ね、ロジックはすべて
//! ライブラリ側（テスト可能な層）に置く。

fn main() -> std::io::Result<()> {
    let info = usagi::usecase::app_info();
    usagi::presentation::write_banner(&mut std::io::stdout(), &info)
}
