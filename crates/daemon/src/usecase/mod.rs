//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （セッション監視ティック・委譲 queue の消化＝autostart・waiting/done 通知の調停・
//! 孤児端末 adopt の判定）。domain は usagi-core を再利用し、実 IO は注入する。
//! v2 では必要になった時点で実装を追加する。
