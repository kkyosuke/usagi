use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const ARTIFACT_SCHEMA: &str = "usagi-artifact-v1";

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown-profile".to_owned());
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_owned());
    let rustc = env::var("RUSTC")
        .ok()
        .and_then(|rustc| {
            Command::new(rustc)
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
        })
        .map_or_else(String::new, |output| {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        });
    let rustflags = env::var("CARGO_ENCODED_RUSTFLAGS")
        .or_else(|_| env::var("RUSTFLAGS"))
        .unwrap_or_default();
    let mut features = env::vars()
        .filter_map(|(name, value)| name.starts_with("CARGO_FEATURE_").then_some((name, value)))
        .collect::<Vec<_>>();
    features.sort();
    let features = features
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let git_commit = git_output(&root, ["rev-parse", "--verify", "HEAD"]);
    let dirty = git_output(&root, ["status", "--porcelain=v1", "--untracked-files=all"])
        .is_some_and(|status| !status.is_empty());
    let files = git_files(&root).unwrap_or_else(|| source_files(&root));
    let tree_digest = digest_files(&root, &files);

    let commit = git_commit.map_or_else(String::new, |commit| {
        if dirty {
            format!("{commit}-dirty")
        } else {
            commit
        }
    });
    let source_identity = tree_digest.map_or_else(String::new, |tree_digest| {
        let mut digest = Sha256::new();
        for component in [
            ARTIFACT_SCHEMA.as_bytes(),
            profile.as_bytes(),
            target.as_bytes(),
            commit.as_bytes(),
            tree_digest.as_bytes(),
            rustc.as_bytes(),
            rustflags.as_bytes(),
            features.as_bytes(),
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component);
        }
        hex(digest.finalize())
    });

    println!("cargo:rustc-env=USAGI_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=USAGI_BUILD_TARGET={target}");
    println!("cargo:rustc-env=USAGI_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=USAGI_BUILD_SOURCE_ID={source_identity}");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    if let Some(index) = git_output(&root, ["rev-parse", "--git-path", "index"]) {
        println!("cargo:rerun-if-changed={index}");
    }
    if let Some(head) = git_output(&root, ["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_files(root: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect::<Vec<_>>();
    files.sort();
    Some(files)
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_source_files(root, root, &mut files);
    files.sort();
    files
}

fn collect_source_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if excluded(relative) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_source_files(root, &path, files);
        } else if file_type.is_file() || file_type.is_symlink() {
            files.push(relative.to_path_buf());
        }
    }
}

fn excluded(path: &Path) -> bool {
    path.components().next().is_some_and(|component| {
        let component = component.as_os_str();
        component == OsStr::new(".git")
            || component == OsStr::new("target")
            || component == OsStr::new(".usagi")
    })
}

fn digest_files(root: &Path, files: &[PathBuf]) -> Option<String> {
    let mut digest = Sha256::new();
    let mut hashed = 0_u64;
    for relative in files {
        let path = root.join(relative);
        let data = if path.is_symlink() {
            fs::read_link(&path)
                .ok()?
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        } else if path.is_file() {
            fs::read(&path).ok()?
        } else {
            continue;
        };
        let name = relative.to_string_lossy();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((data.len() as u64).to_be_bytes());
        digest.update(data);
        hashed += 1;
    }
    (hashed > 0).then(|| hex(digest.finalize()))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
