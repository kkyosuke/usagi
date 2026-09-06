//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{InstallerRequest, Run, RunOutcome};

/// この digest は `scripts/install.sh` と同じ build に固定する。変更時は unit test が
/// 不一致を検出するため、review 可能な identity 更新として同時に変更する。
const INSTALLER_SHA256: [u8; 32] = [
    0xa1, 0x3f, 0xd1, 0xb0, 0x37, 0xbc, 0xba, 0x98, 0xd6, 0xc8, 0x45, 0x36, 0x1b, 0x74, 0x38, 0x78,
    0x5d, 0x16, 0x03, 0x4f, 0x3b, 0xe4, 0xad, 0x48, 0xa9, 0xd4, 0x74, 0xa1, 0xe6, 0xa7, 0x2b, 0x31,
];
const INSTALLER: &[u8] = include_bytes!("../../../../../scripts/install.sh");

/// `usagi update` のハンドラ。実際の subprocess は合成ルートが実行する。
pub struct Update {
    pub select_version: bool,
}

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        if self.select_version {
            writeln!(out, "インストールする usagi のリリースを選んでね！ぴょん")?;
        } else {
            writeln!(out, "最新の usagi リリースをインストール中だよ！ぴょん")?;
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
        assert!(String::from_utf8(out).unwrap().contains("インストール中"));
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
