//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{InstallerRequest, Run, RunOutcome};

/// この digest は `scripts/install.sh` と同じ build に固定する。変更時は unit test が
/// 不一致を検出するため、review 可能な identity 更新として同時に変更する。
const INSTALLER_SHA256: [u8; 32] = [
    0x6b, 0xde, 0x05, 0xad, 0xdd, 0x2c, 0x4e, 0x32, 0xab, 0x93, 0x3d, 0x4a, 0xe5, 0x43, 0x7b, 0x29,
    0x99, 0x84, 0xc0, 0x0c, 0xcf, 0x0a, 0x13, 0x8b, 0x0f, 0x24, 0xdd, 0xbd, 0x28, 0xed, 0x80, 0x76,
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
