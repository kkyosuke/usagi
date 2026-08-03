use std::{fs, path::Path};

#[test]
fn usecase_production_modules_do_not_import_presentation() {
    fn visit(path: &Path, violations: &mut Vec<String>) {
        for entry in fs::read_dir(path).expect("usecase source directory is readable") {
            let entry = entry.expect("usecase source entry is readable");
            let path = entry.path();
            if path.is_dir() {
                visit(&path, violations);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = fs::read_to_string(&path).expect("usecase source is UTF-8");
                for (index, line) in source.lines().enumerate() {
                    let code = line.trim_start();
                    if code.starts_with("use crate::presentation")
                        || code.starts_with("pub use crate::presentation")
                    {
                        violations.push(format!("{}:{}: {code}", path.display(), index + 1));
                    }
                }
            }
        }
    }

    let usecase = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/usecase");
    let mut violations = Vec::new();
    visit(&usecase, &mut violations);
    assert!(
        violations.is_empty(),
        "daemon usecase must not import presentation:\n{}",
        violations.join("\n")
    );
}
