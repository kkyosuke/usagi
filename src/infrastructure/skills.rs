//! Skills usagi ships to every agent it launches.
//!
//! usagi embeds a set of Claude Code skills in its binary (from `assets/skills/`
//! at build time). They are materialised once under the global data dir
//! (`<data-dir>/skills/`, the single on-disk source of truth — see
//! [`materialize`]), and every session worktree gets a per-skill
//! `.claude/skills/<name>` symlink pointing into that directory (see [`link`]),
//! so the agent launched in the worktree discovers them without a per-worktree
//! copy and they coexist with any skills the project ships itself.
//!
//! Rebuilding usagi with an edited skill set re-materialises the files, and
//! every existing symlink then sees the new content at once — the symlink, not a
//! copy, is what keeps the worktrees in sync with the shipped skills.
//!
//! To add a skill, drop `assets/skills/<name>/SKILL.md` in the repo and add an
//! entry to [`SKILLS`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::infrastructure::storage;

/// A skill compiled into the binary: its directory name under `skills/` and the
/// `SKILL.md` body written there.
struct Embedded {
    name: &'static str,
    body: &'static str,
}

/// The skills shipped with usagi, embedded at build time.
const SKILLS: &[Embedded] = &[Embedded {
    name: "usagi-session",
    body: include_str!("../../assets/skills/usagi-session/SKILL.md"),
}];

/// The git exclude patterns for the skill symlinks usagi creates in a worktree,
/// each anchored to the worktree root with a leading `/`. Sessions add these to
/// the worktree's local git exclude so the untracked symlinks never mark the
/// session dirty — see
/// [`git::ensure_excluded`](crate::infrastructure::git::ensure_excluded).
///
/// One pattern per skill (not the whole `.claude/skills` directory) so a
/// project's own skills sitting alongside usagi's stay visible to git.
pub fn git_exclude_patterns() -> Vec<String> {
    SKILLS
        .iter()
        .map(|skill| format!("/.claude/skills/{}", skill.name))
        .collect()
}

/// The skills directory under `data_dir`: `<data-dir>/skills`. This is both the
/// [`materialize`] write target and the parent of each [`link`] symlink target.
fn skills_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}

/// The skills directory under the resolved global data dir — the parent each
/// worktree's `.claude/skills/<name>` symlink points into.
fn target() -> Result<PathBuf> {
    Ok(skills_dir(&storage::data_dir()?))
}

/// Write every embedded skill to `<data_dir>/skills/<name>/SKILL.md`,
/// overwriting any stale copy so a rebuilt binary's skills win. Idempotent.
/// Returns the skills directory.
pub fn materialize(data_dir: &Path) -> Result<PathBuf> {
    let dir = skills_dir(data_dir);
    for skill in SKILLS {
        let skill_dir = dir.join(skill.name);
        fs::create_dir_all(&skill_dir)
            .context(format!("failed to create {}", skill_dir.display()))?;
        let file = skill_dir.join("SKILL.md");
        fs::write(&file, skill.body).context(format!("failed to write {}", file.display()))?;
    }
    Ok(dir)
}

/// Materialise the embedded skills under the resolved global data dir.
/// Convenience over [`materialize`] for the composition root, which has no data
/// dir in hand.
pub fn materialize_default() -> Result<PathBuf> {
    materialize(&storage::data_dir()?)
}

/// Symlink each shipped skill into `<worktree>/.claude/skills/<name>`, pointing
/// at usagi's materialised copy under [`target`], so the agent launched in
/// `worktree` discovers them. Creates `.claude/skills/` if absent.
///
/// Linked **per skill**, not as the whole `skills/` directory, so usagi's skills
/// coexist with a project's own skills in the same `.claude/skills/` directory.
/// Idempotent: an existing usagi symlink is repaired, while a *real* file or
/// directory at a skill's name — a project's own same-named skill — is left
/// untouched rather than clobbered.
pub fn link(worktree: &Path) -> Result<()> {
    let skills = worktree.join(".claude").join("skills");
    fs::create_dir_all(&skills).context(format!("failed to create {}", skills.display()))?;
    let source = target()?;
    for skill in SKILLS {
        link_one(&skills.join(skill.name), &source.join(skill.name))?;
    }
    Ok(())
}

/// Symlink `link_path` at `target`, replacing a stale symlink but never a real
/// file or directory there (a project's own same-named skill).
fn link_one(link_path: &Path, target: &Path) -> Result<()> {
    match fs::symlink_metadata(link_path) {
        // A symlink already there (ours, or stale) is replaced so its target is
        // always current.
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(link_path)
                .context(format!("failed to replace {}", link_path.display()))?;
        }
        // A real file or directory there is not ours: leave it alone.
        Ok(_) => return Ok(()),
        // Absent (or unreadable) — create it below.
        Err(_) => {}
    }
    symlink_dir(target, link_path).context(format!("failed to symlink {}", link_path.display()))
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` with `$USAGI_HOME` pointed at a fresh temp dir, so the resolved
    /// data dir is hermetic. Serialised against other env-mutating tests.
    fn with_data_dir(body: impl FnOnce(&Path)) {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, home.path());
        body(home.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn materialize_writes_every_embedded_skill() {
        let home = tempfile::tempdir().unwrap();
        let dir = materialize(home.path()).unwrap();
        assert_eq!(dir, home.path().join("skills"));
        for skill in SKILLS {
            let file = dir.join(skill.name).join("SKILL.md");
            assert_eq!(fs::read_to_string(&file).unwrap(), skill.body);
        }
    }

    #[test]
    fn materialize_is_idempotent_and_refreshes_content() {
        let home = tempfile::tempdir().unwrap();
        let dir = materialize(home.path()).unwrap();
        // A stale body on disk is overwritten by the embedded one on re-run.
        let file = dir.join(SKILLS[0].name).join("SKILL.md");
        fs::write(&file, "stale").unwrap();
        materialize(home.path()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), SKILLS[0].body);
    }

    #[test]
    fn materialize_errors_when_the_skill_dir_cannot_be_created() {
        let home = tempfile::tempdir().unwrap();
        // A file where the `skills` directory must go makes create_dir_all fail.
        fs::write(home.path().join("skills"), "blocker").unwrap();
        assert!(materialize(home.path()).is_err());
    }

    #[test]
    fn materialize_default_uses_the_resolved_data_dir() {
        with_data_dir(|home| {
            let dir = materialize_default().unwrap();
            assert_eq!(dir, home.join("skills"));
            assert!(dir.join(SKILLS[0].name).join("SKILL.md").is_file());
        });
    }

    #[test]
    fn link_symlinks_each_skill_into_the_skills_dir() {
        with_data_dir(|home| {
            let wt = tempfile::tempdir().unwrap();
            link(wt.path()).unwrap();
            let skills = wt.path().join(".claude").join("skills");
            for skill in SKILLS {
                let link_path = skills.join(skill.name);
                assert!(fs::symlink_metadata(&link_path)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(
                    fs::read_link(&link_path).unwrap(),
                    home.join("skills").join(skill.name)
                );
            }
        });
    }

    #[test]
    fn link_coexists_with_a_projects_own_skills() {
        with_data_dir(|home| {
            let wt = tempfile::tempdir().unwrap();
            // A project's own skill already lives under .claude/skills/.
            let skills = wt.path().join(".claude").join("skills");
            let own = skills.join("project-skill");
            fs::create_dir_all(&own).unwrap();
            fs::write(own.join("SKILL.md"), "mine").unwrap();

            link(wt.path()).unwrap();

            // The project's skill is untouched...
            assert!(own.join("SKILL.md").is_file());
            // ...and usagi's skill sits alongside it as a symlink.
            let ours = skills.join(SKILLS[0].name);
            assert!(fs::symlink_metadata(&ours)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::read_link(&ours).unwrap(),
                home.join("skills").join(SKILLS[0].name)
            );
        });
    }

    #[test]
    fn link_replaces_a_stale_symlink_but_not_a_real_same_named_skill() {
        with_data_dir(|home| {
            let wt = tempfile::tempdir().unwrap();
            let skills = wt.path().join(".claude").join("skills");
            fs::create_dir_all(&skills).unwrap();
            // A stale usagi symlink is repaired to the current target.
            let ours = skills.join(SKILLS[0].name);
            symlink_dir(Path::new("/somewhere/else"), &ours).unwrap();

            link(wt.path()).unwrap();
            assert_eq!(
                fs::read_link(&ours).unwrap(),
                home.join("skills").join(SKILLS[0].name)
            );

            // A *real* directory at a skill's name (a user's same-named skill) is
            // left untouched: a second link() must not clobber it.
            fs::remove_file(&ours).unwrap();
            fs::create_dir_all(&ours).unwrap();
            fs::write(ours.join("SKILL.md"), "mine").unwrap();
            link(wt.path()).unwrap();
            assert!(!fs::symlink_metadata(&ours)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read_to_string(ours.join("SKILL.md")).unwrap(), "mine");
        });
    }

    #[test]
    fn link_errors_when_the_skills_path_cannot_be_created() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // A file at `.claude` makes create_dir_all of `.claude/skills` fail.
            fs::write(wt.path().join(".claude"), "blocker").unwrap();
            assert!(link(wt.path()).is_err());
        });
    }

    #[test]
    fn git_exclude_patterns_anchors_each_skill_under_claude_skills() {
        let patterns = git_exclude_patterns();
        assert_eq!(patterns.len(), SKILLS.len());
        assert!(patterns.contains(&format!("/.claude/skills/{}", SKILLS[0].name)));
    }
}
