//! Publish a review from the authenticated session's work directory.
use crate::mcp::tool::Tool;

pub(super) struct Publish;

impl Tool for Publish {
    fn name(&self) -> &'static str {
        "artifact_publish"
    }

    fn description(&self) -> &'static str {
        "現在の session の .work/ に用意した確認用ファイルを Artifact 一覧に公開し、リンク先の path を返す。purpose は一覧でユーザーに示す確認点。assets は source の親ディレクトリ基準の相対パス（例 assets/chart.png）。"
    }

    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"source":{"type":"string","minLength":1},"purpose":{"type":"string","minLength":1,"x-maxUtf8Bytes":512},"assets":{"type":"array","items":{"type":"string"},"maxItems":256}},"required":["source","purpose"],"additionalProperties":false}"#
    }
}
