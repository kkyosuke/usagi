use std::path::{Path, PathBuf};

const FORBIDDEN_LAYERS: [&str; 2] = ["presentation", "usecase"];
// #496 owns the existing session setup port inversion. Keep that one explicit
// while preventing this boundary from accumulating any additional exceptions.
const TRACKED_EXCEPTIONS: [(&str, &str); 2] = [
    ("agent_start_store.rs", "usecase"),
    ("setup_runner.rs", "usecase"),
];

#[test]
fn infrastructure_does_not_depend_on_upper_layers() {
    let infrastructure = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/infrastructure");
    let mut rust_files = Vec::new();
    collect_rust_files(&infrastructure, &mut rust_files);

    let mut violations = Vec::new();
    for path in rust_files {
        let source = std::fs::read_to_string(&path).unwrap();
        for layer in forbidden_dependencies(&source) {
            if TRACKED_EXCEPTIONS
                .iter()
                .any(|(suffix, allowed_layer)| path.ends_with(suffix) && layer == *allowed_layer)
            {
                continue;
            }
            violations.push(format!("{} -> {layer}", path.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "infrastructure must not depend on upper layers:\n{}",
        violations.join("\n")
    );
}

#[test]
fn forbidden_dependency_fixture_is_rejected() {
    let fixture = r#"
        // crate::presentation::ignored_comment
        const EXAMPLE: &str = "crate::usecase::ignored_string";
        use crate::presentation::mcp;
        fn load() { crate::usecase::settings::effective_for(todo!()); }
    "#;

    assert_eq!(
        forbidden_dependencies(fixture),
        vec!["presentation", "usecase"]
    );
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn forbidden_dependencies(source: &str) -> Vec<&'static str> {
    let code = strip_comments_and_literals(source);
    FORBIDDEN_LAYERS
        .into_iter()
        .filter(|layer| references_crate_module(&code, layer))
        .collect()
}

fn references_crate_module(code: &str, layer: &str) -> bool {
    let tokens = tokens(code);
    tokens
        .windows(3)
        .any(|window| window == ["crate", "::", layer])
}

fn tokens(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_ascii_alphanumeric() || chars[index] == '_' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            result.push(chars[start..index].iter().collect());
            continue;
        }
        if chars[index] == ':' && chars.get(index + 1) == Some(&':') {
            result.push("::".to_string());
            index += 2;
            continue;
        }
        index += 1;
    }
    result
}

fn strip_comments_and_literals(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment(usize),
        String,
        Character,
    }

    let chars: Vec<char> = source.chars().collect();
    let mut output = String::with_capacity(source.len());
    let mut state = State::Code;
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        match state {
            State::Code if current == '/' && next == Some('/') => {
                state = State::LineComment;
                output.push(' ');
                index += 1;
            }
            State::Code if current == '/' && next == Some('*') => {
                state = State::BlockComment(1);
                output.push(' ');
                index += 1;
            }
            State::Code if current == '"' => {
                state = State::String;
                output.push(' ');
            }
            State::Code
                if current == '\''
                    && (next == Some('\\') || chars.get(index + 2) == Some(&'\'')) =>
            {
                state = State::Character;
                output.push(' ');
            }
            State::Code => output.push(current),
            State::LineComment if current == '\n' => {
                state = State::Code;
                output.push('\n');
            }
            State::LineComment => output.push(' '),
            State::BlockComment(depth) if current == '/' && next == Some('*') => {
                state = State::BlockComment(depth + 1);
                output.push(' ');
                index += 1;
            }
            State::BlockComment(depth) if current == '*' && next == Some('/') => {
                state = if depth == 1 {
                    State::Code
                } else {
                    State::BlockComment(depth - 1)
                };
                output.push(' ');
                index += 1;
            }
            State::BlockComment(_) => output.push(if current == '\n' { '\n' } else { ' ' }),
            State::String | State::Character if current == '\\' => {
                output.push(' ');
                if next.is_some() {
                    output.push(' ');
                    index += 1;
                }
            }
            State::String if current == '"' => {
                state = State::Code;
                output.push(' ');
            }
            State::Character if current == '\'' => {
                state = State::Code;
                output.push(' ');
            }
            State::String | State::Character => {
                output.push(if current == '\n' { '\n' } else { ' ' });
            }
        }
        index += 1;
    }
    output
}
