//! MCP resource 面。tool（振る舞い）と違い resource は**静的テキスト**（uri / name /
//! description / mimeType / text）で、agent が `resources/list` で発見し `resources/read` で
//! 本文を取得する。orchestration の利用ガイドのように「実行はしないが agent に読ませたい」
//! 導線をここで配信する。
//!
//! lookup と応答 `Value` の組み立て（`list_result` / `read_result`）は純関数としてここに閉じ、
//! serve ループ側は薄い glue に保つ（テスト可能性のため）。

use serde_json::{Value, json};

/// 1 つの MCP resource（静的テキスト）。
pub struct Resource {
    /// resource を一意に指す URI（例: `usagi://guides/orchestration`）。
    pub uri: &'static str,
    /// 人間可読の名前（`resources/list` に載る）。
    pub name: &'static str,
    /// いつ読むかの短い説明（`resources/list` に載る）。
    pub description: &'static str,
    /// 本文の MIME タイプ（例: `text/markdown`）。
    pub mime_type: &'static str,
    /// 本文（`resources/read` が返す）。
    pub text: &'static str,
}

/// orchestration 利用ガイドの本文（クレート同梱アセット）。
const ORCHESTRATION_GUIDE: &str = include_str!("guides/orchestration.md");

/// 公開する全 resource のレジストリ。
#[must_use]
pub fn registry() -> Vec<Resource> {
    vec![Resource {
        uri: "usagi://guides/orchestration",
        name: "usagi orchestration guide",
        description: "セッションの委譲・観測・完了報告の使い方（agent 向け）",
        mime_type: "text/markdown",
        text: ORCHESTRATION_GUIDE,
    }]
}

/// `resources/list` の結果（各 resource の uri / name / description / mimeType）。
#[must_use]
pub fn list_result() -> Value {
    let resources: Vec<Value> = registry()
        .iter()
        .map(|resource| {
            json!({
                "uri": resource.uri,
                "name": resource.name,
                "description": resource.description,
                "mimeType": resource.mime_type,
            })
        })
        .collect();
    json!({ "resources": resources })
}

/// `resources/read` の結果（`contents` に uri / mimeType / text）。未知の URI なら `None`。
#[must_use]
pub fn read_result(uri: &str) -> Option<Value> {
    registry()
        .iter()
        .find(|resource| resource.uri == uri)
        .map(|resource| {
            json!({
                "contents": [{
                    "uri": resource.uri,
                    "mimeType": resource.mime_type,
                    "text": resource.text,
                }],
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{list_result, read_result, registry};

    #[test]
    fn registry_exposes_the_orchestration_guide_with_nonempty_body() {
        let reg = registry();
        assert_eq!(reg.len(), 1);
        let guide = &reg[0];
        assert_eq!(guide.uri, "usagi://guides/orchestration");
        assert_eq!(guide.mime_type, "text/markdown");
        assert!(!guide.name.is_empty());
        assert!(!guide.description.is_empty());
        assert!(guide.text.contains("orchestration"));
    }

    #[test]
    fn every_resource_has_a_unique_uri() {
        let reg = registry();
        let mut seen = std::collections::HashSet::new();
        for resource in &reg {
            assert!(seen.insert(resource.uri));
        }
    }

    #[test]
    fn list_result_lists_each_resource_with_metadata() {
        let value = list_result();
        let resources = value["resources"].as_array().unwrap();
        assert_eq!(resources.len(), registry().len());
        let guide = &resources[0];
        assert_eq!(guide["uri"], "usagi://guides/orchestration");
        assert_eq!(guide["name"], "usagi orchestration guide");
        assert_eq!(guide["mimeType"], "text/markdown");
        assert!(!guide["description"].as_str().unwrap().is_empty());
    }

    #[test]
    fn read_result_returns_contents_for_a_known_uri() {
        let value = read_result("usagi://guides/orchestration").unwrap();
        let contents = value["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["uri"], "usagi://guides/orchestration");
        assert_eq!(contents[0]["mimeType"], "text/markdown");
        let text = contents[0]["text"].as_str().unwrap();
        assert!(text.contains("session_create"));
        assert!(!text.contains("session_delegate_brief"));
    }

    #[test]
    fn read_result_is_none_for_an_unknown_uri() {
        assert!(read_result("usagi://guides/does-not-exist").is_none());
    }
}
