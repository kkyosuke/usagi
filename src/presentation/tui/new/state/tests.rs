use super::*;

fn type_str(state: &mut FormState, s: &str) {
    for c in s.chars() {
        state.insert_char(c);
    }
}

/// Move focus to a specific field from anywhere, via repeated `focus_next`.
fn focus_to(state: &mut FormState, field: Field) {
    while state.focus() != field {
        state.focus_next();
    }
}

#[test]
fn typing_url_auto_fills_directory() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo.git");
    assert_eq!(state.directory(), "repo");
}

#[test]
fn editing_directory_stops_auto_derivation() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo");
    assert_eq!(state.directory(), "repo");

    focus_to(&mut state, Field::Directory);
    type_str(&mut state, "-fork");
    assert_eq!(state.directory(), "repo-fork");

    // Further URL edits must not clobber the user's directory.
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "2");
    assert_eq!(state.directory(), "repo-fork");
}

#[test]
fn clearing_directory_restores_auto_derivation() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo");
    focus_to(&mut state, Field::Directory);
    for _ in 0.."repo".len() {
        state.backspace();
    }
    // Cleared, and not immediately refilled, so a custom name is possible.
    assert_eq!(state.directory(), "");
    // Back on the URL field, typing should re-derive again.
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "-x");
    assert_eq!(state.directory(), "repo-x");
}

#[test]
fn editing_the_location_field() {
    let mut state = FormState::new();
    state.set_location("/base");
    assert_eq!(state.location(), "/base");

    focus_to(&mut state, Field::Location);
    state.insert_char('x');
    assert_eq!(state.location(), "/basex");
    state.backspace();
    assert_eq!(state.location(), "/base");
}

#[test]
fn focus_cycles_through_clone_fields_including_mode() {
    let mut state = FormState::new();
    assert_eq!(state.focus(), Field::Mode);
    state.focus_next();
    assert_eq!(state.focus(), Field::Url);
    state.focus_next();
    assert_eq!(state.focus(), Field::Location);
    state.focus_next();
    assert_eq!(state.focus(), Field::Directory);
    state.focus_next();
    assert_eq!(state.focus(), Field::Branch);
    state.focus_next();
    // Wraps back to the mode selector.
    assert_eq!(state.focus(), Field::Mode);
    state.focus_prev();
    assert_eq!(state.focus(), Field::Branch);
}

#[test]
fn focus_cycles_through_existing_fields() {
    let mut state = FormState::new();
    state.toggle_mode();
    assert_eq!(state.mode(), Mode::Existing);
    assert_eq!(state.focus(), Field::Mode);
    state.focus_next();
    assert_eq!(state.focus(), Field::Path);
    state.focus_next();
    assert_eq!(state.focus(), Field::Name);
    state.focus_next();
    assert_eq!(state.focus(), Field::Mode);
}

#[test]
fn toggle_mode_switches_between_the_two_modes() {
    let mut state = FormState::new();
    assert_eq!(state.mode(), Mode::Clone);
    state.toggle_mode();
    assert_eq!(state.mode(), Mode::Existing);
    // Focus returns to the mode selector so repeated toggles work.
    assert_eq!(state.focus(), Field::Mode);
    state.toggle_mode();
    assert_eq!(state.mode(), Mode::Clone);
}

#[test]
fn typing_on_the_mode_selector_is_ignored() {
    let mut state = FormState::new();
    assert_eq!(state.focus(), Field::Mode);
    state.insert_char('x');
    state.backspace();
    assert_eq!(state.url(), "");
    assert_eq!(state.location(), "");
}

#[test]
fn typing_path_auto_fills_name() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/home/me/projects/my-app");
    assert_eq!(state.name(), "my-app");
}

#[test]
fn editing_name_stops_auto_derivation_and_clearing_restores_it() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/home/me/app");
    assert_eq!(state.name(), "app");

    focus_to(&mut state, Field::Name);
    type_str(&mut state, "-x");
    assert_eq!(state.name(), "app-x");

    // Path edits no longer clobber the custom name.
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "y");
    assert_eq!(state.name(), "app-x");

    // Clearing the name re-enables derivation.
    focus_to(&mut state, Field::Name);
    for _ in 0.."app-x".len() {
        state.backspace();
    }
    assert_eq!(state.name(), "");
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "z");
    assert_eq!(state.name(), "appyz");
}

#[test]
fn validate_clone_succeeds_with_derived_directory() {
    let mut state = FormState::new();
    state.set_location("/base");
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "git@github.com:owner/repo.git");
    assert!(matches!(
        state.validate().unwrap(),
        NewProject::Clone(spec)
            if spec.url.as_str() == "git@github.com:owner/repo.git"
                && spec.location == std::path::Path::new("/base")
                && spec.directory == "repo"
                && spec.branch.is_none()
    ));
}

#[test]
fn validate_clone_keeps_explicit_branch_and_directory() {
    let mut state = FormState::new();
    state.set_location("/base");
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo.git");
    // Clear the auto-filled directory, then type a custom one.
    focus_to(&mut state, Field::Directory);
    for _ in 0.."repo".len() {
        state.backspace();
    }
    type_str(&mut state, "my-dir");
    focus_to(&mut state, Field::Branch);
    type_str(&mut state, "develop");
    assert!(matches!(
        state.validate().unwrap(),
        NewProject::Clone(spec)
            if spec.directory == "my-dir" && spec.branch.as_deref() == Some("develop")
    ));
}

#[test]
fn validate_clone_derives_directory_when_field_is_empty() {
    let mut state = FormState::new();
    state.set_location("/base");
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo.git");
    // Clear the auto-filled directory so validate falls back to the URL.
    focus_to(&mut state, Field::Directory);
    for _ in 0.."repo".len() {
        state.backspace();
    }
    assert_eq!(state.directory(), "");
    assert!(matches!(
        state.validate().unwrap(),
        NewProject::Clone(spec) if spec.directory == "repo"
    ));
}

#[test]
fn validate_clone_rejects_empty_url() {
    let state = FormState::new();
    assert!(state.validate().is_err());
}

#[test]
fn validate_clone_rejects_empty_location() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repo.git");
    // Location left blank: validation fails even with a valid URL.
    let err = state.validate().unwrap_err();
    assert!(err.contains("create"));
}

#[test]
fn validate_existing_succeeds_with_derived_name() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/home/me/my-app");
    assert!(matches!(
        state.validate().unwrap(),
        NewProject::Existing(spec)
            if spec.path == std::path::Path::new("/home/me/my-app") && spec.name == "my-app"
    ));
}

#[test]
fn validate_existing_keeps_explicit_name() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/home/me/my-app");
    focus_to(&mut state, Field::Name);
    for _ in 0.."my-app".len() {
        state.backspace();
    }
    type_str(&mut state, "custom");
    assert!(matches!(
        state.validate().unwrap(),
        NewProject::Existing(spec) if spec.name == "custom"
    ));
}

#[test]
fn validate_existing_rejects_empty_path() {
    let mut state = FormState::new();
    state.toggle_mode();
    let err = state.validate().unwrap_err();
    assert!(err.contains("directory"));
}

#[test]
fn validate_existing_rejects_a_path_with_no_final_segment() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    // The root has no final segment, so no name can be derived.
    type_str(&mut state, "/");
    let err = state.validate().unwrap_err();
    assert!(err.contains("name"));
}

#[test]
fn backspace_on_url_re_derives_the_directory() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    type_str(&mut state, "https://github.com/owner/repos");
    assert_eq!(state.directory(), "repos");
    // Deleting the trailing character re-derives the directory.
    state.backspace();
    assert_eq!(state.url(), "https://github.com/owner/repo");
    assert_eq!(state.directory(), "repo");
}

#[test]
fn backspace_edits_the_branch_field() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Branch);
    type_str(&mut state, "dev");
    state.backspace();
    assert_eq!(state.branch(), "de");
}

#[test]
fn backspace_on_path_re_derives_the_name() {
    let mut state = FormState::new();
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/home/me/apps");
    assert_eq!(state.name(), "apps");
    // Deleting the trailing character re-derives the name.
    state.backspace();
    assert_eq!(state.path(), "/home/me/app");
    assert_eq!(state.name(), "app");
}

#[test]
fn directory_fields_are_the_clone_location_and_existing_path() {
    let mut state = FormState::new();
    // Clone mode: only Location is a directory field.
    focus_to(&mut state, Field::Location);
    assert!(state.focus_is_directory());
    focus_to(&mut state, Field::Url);
    assert!(!state.focus_is_directory());

    // Existing mode: only Path is a directory field.
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    assert!(state.focus_is_directory());
    focus_to(&mut state, Field::Name);
    assert!(!state.focus_is_directory());
}

#[test]
fn directory_field_value_reads_the_focused_directory_field() {
    let mut state = FormState::new();
    state.set_location("/base");
    focus_to(&mut state, Field::Location);
    assert_eq!(state.directory_field_value(), "/base");

    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    type_str(&mut state, "/here");
    assert_eq!(state.directory_field_value(), "/here");

    // A non-directory field reports no value.
    focus_to(&mut state, Field::Name);
    assert_eq!(state.directory_field_value(), "");
}

#[test]
fn set_directory_field_updates_the_focused_directory_field() {
    let mut state = FormState::new();
    // Clone Location is set verbatim.
    focus_to(&mut state, Field::Location);
    state.set_directory_field("/chosen");
    assert_eq!(state.location(), "/chosen");

    // Existing Path is set and re-derives the name.
    state.toggle_mode();
    focus_to(&mut state, Field::Path);
    state.set_directory_field("/home/me/picked");
    assert_eq!(state.path(), "/home/me/picked");
    assert_eq!(state.name(), "picked");
}

#[test]
fn set_directory_field_is_a_noop_on_a_non_directory_field() {
    let mut state = FormState::new();
    focus_to(&mut state, Field::Url);
    state.set_directory_field("/ignored");
    // Neither the URL nor the directory paths change.
    assert_eq!(state.url(), "");
    assert_eq!(state.location(), "");
    assert_eq!(state.path(), "");
}

#[test]
fn suggest_name_handles_trailing_slash_and_empty() {
    assert_eq!(suggest_name("/a/b/c"), "c");
    assert_eq!(suggest_name("/a/b/"), "b");
    assert_eq!(suggest_name(""), "");
}
