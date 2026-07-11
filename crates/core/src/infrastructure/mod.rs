//! 共有 infrastructure 層。TUI 面・daemon 面の両方が使う外部世界との接続
//! （IPC プロトコル型・`state.json` などの永続化・git）を実装し、domain が
//! 定義する抽象に依存する（依存方向は domain ← infrastructure）。
//! 片面しか使わない infrastructure は usagi-tui / usagi-daemon 側に置く。
//! v2 では必要になった時点で実装を追加する。
