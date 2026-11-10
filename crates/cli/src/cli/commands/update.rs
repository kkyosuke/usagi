//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{InstallerRequest, Run, RunOutcome};

/// この digest は `scripts/install.sh` と同じ build に固定する。変更時は unit test が
/// 不一致を検出するため、review 可能な identity 更新として同時に変更する。
const INSTALLER_SHA256: [u8; 32] = [
    0xad, 0xe0, 0xfc, 0x9d, 0x8f, 0xf2, 0x9b, 0xfd, 0x47, 0xab, 0x2f, 0x66, 0x77, 0x9e, 0x26, 0xb9,
    0x8a, 0x63, 0xe4, 0xc4, 0x2e, 0x7b, 0x8d, 0x8e, 0xa4, 0x0e, 0x7f, 0x47, 0xff, 0xf5, 0x72, 0x8a,
];
const INSTALLER: &[u8] = include_bytes!("../../../../../scripts/install.sh");

/// The same verified installer owns publication and deferred daemon synchronization.
#[must_use]
pub const fn installer_request(select_version: bool) -> InstallerRequest {
    InstallerRequest::new(INSTALLER, INSTALLER_SHA256, select_version)
}

/// `usagi update` のハンドラ。実際の subprocess は合成ルートが実行する。
pub struct Update {
    pub select_version: bool,
    pub restart_agents: bool,
}

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        if self.select_version {
            writeln!(out, "インストールする usagi のリリースを選んでね！ぴょん")?;
        } else {
            writeln!(out, "最新の usagi リリースをインストール中だよ！ぴょん")?;
        }
        Ok(RunOutcome::SelfUpdate(
            installer_request(self.select_version).with_agent_restart(self.restart_agents),
        ))
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
            restart_agents: false,
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
            restart_agents: true,
        }
        .run(&mut out)
        .unwrap();
        assert!(
            matches!(outcome, RunOutcome::SelfUpdate(request) if request.select_version() && request.restart_agents())
        );
    }

    #[test]
    fn output_failure_prevents_both_update_modes_from_reaching_the_runtime() {
        for select_version in [false, true] {
            let mut output = &mut [][..];
            let error = Update {
                select_version,
                restart_agents: true,
            }
            .run(&mut output)
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        }
    }
}
