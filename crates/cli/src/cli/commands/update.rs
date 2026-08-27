//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{InstallerRequest, Run, RunOutcome};

/// この digest は `scripts/install.sh` と同じ build に固定する。変更時は unit test が
/// 不一致を検出するため、review 可能な identity 更新として同時に変更する。
const INSTALLER_SHA256: [u8; 32] = [
    0x0b, 0xad, 0x2d, 0xda, 0x85, 0x66, 0x62, 0x50, 0x3f, 0x5a, 0xe4, 0x19, 0x7f, 0xa4, 0xc5, 0x34,
    0xea, 0x8a, 0x0c, 0x4e, 0x39, 0x8f, 0x16, 0x4e, 0x9b, 0xf4, 0x0f, 0xf0, 0x5f, 0xd9, 0x65, 0x65,
];
const INSTALLER: &[u8] = include_bytes!("../../../../../scripts/install.sh");

/// `usagi update` のハンドラ。実際の subprocess は合成ルートが実行する。
pub struct Update {
    pub select_version: bool,
}

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        if self.select_version {
            writeln!(out, "select a usagi release to install...")?;
        } else {
            writeln!(
                out,
                "downloading and installing the latest usagi release..."
            )?;
        }
        Ok(RunOutcome::SelfUpdate(InstallerRequest::new(
            INSTALLER,
            INSTALLER_SHA256,
            self.select_version,
        )))
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{INSTALLER, INSTALLER_SHA256, Update};
    use crate::cli::{Run, RunOutcome};

    #[test]
    fn embedded_installer_matches_its_immutable_digest_and_never_looks_up_main() {
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(INSTALLER)),
            INSTALLER_SHA256
        );
        let script = String::from_utf8_lossy(INSTALLER);
        assert!(!script.contains("raw.githubusercontent.com"));
        assert!(!script.contains("/main/scripts/install.sh"));
    }

    #[test]
    fn handler_requests_a_self_update_from_the_composition_root() {
        let mut out = Vec::new();
        let outcome = Update {
            select_version: false,
        }
        .run(&mut out)
        .unwrap();
        assert!(
            matches!(outcome, RunOutcome::SelfUpdate(request) if !request.select_version() && request.verified_script() == Some(INSTALLER))
        );
        assert!(String::from_utf8(out).unwrap().contains("downloading"));
    }

    #[test]
    fn version_selection_requests_the_interactive_installer_mode() {
        let mut out = Vec::new();
        let outcome = Update {
            select_version: true,
        }
        .run(&mut out)
        .unwrap();
        assert!(matches!(outcome, RunOutcome::SelfUpdate(request) if request.select_version()));
    }

    #[test]
    fn output_failure_prevents_both_update_modes_from_reaching_the_runtime() {
        for select_version in [false, true] {
            let mut output = &mut [][..];
            let error = Update { select_version }.run(&mut output).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        }
    }
}
