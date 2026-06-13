use usagi::usecase::doctor::check_dependencies;

#[test]
fn test_check_dependencies_reports_all_tools() {
    let checks = check_dependencies();
    let names: Vec<_> = checks.iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["git", "bash"]);
}
