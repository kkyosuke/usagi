//! daemon 専用の infrastructure 層。各面が共有する接続（IPC プロトコル型・
//! `state.json` などの永続化・git）は usagi-core が持ち、ここには daemon だけが
//! 使う外部接続を置く（agent/シェルの PTY 所有＝`TerminalPool`・Unix domain socket の
//! IPC サーバ・プロセスグループ管理と単一インスタンスロック・daemon lifecycle の
//! 永続化（`daemon.json` / `sessions.json` / `terminals.json`））。
//! 実 IO そのもの（socket accept・PTY fork・ファイル書き込み）は合成ルートが束ね、
//! この層はそれを注入で受けて純粋に振る舞う。v2 では必要になった時点で実装を追加する。

/// The daemon's concrete pseudo-terminal adapter.  Presentation surfaces only
/// ever receive terminal stream data through IPC; they do not own this IO.
pub mod pty;
pub mod unix_transport;
