//! Doctor 診断の実行順序と表示用 projection。
//!
//! OS の process・filesystem・daemon には直接触れず、[`DoctorPort`] から得た結果を
//! 必須項目の失敗・任意項目の警告へ分類する。実 IO は合成ルートが注入する。

use usagi_core::domain::settings::DefaultModel;

/// 1 診断項目の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// 利用可能。
    Pass,
    /// 任意機能が利用できない。
    Warning,
    /// 必須機能が利用できない。
    Fail,
}

/// Doctor 画面へ渡す 1 診断項目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticCheck {
    /// 利用者向けの項目名。
    pub name: &'static str,
    /// 判定結果。
    pub status: CheckStatus,
    /// version、成功内容、または安全に表示できる失敗理由。
    pub detail: String,
}

/// Doctor の診断結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// 実行順に並んだ診断項目。
    pub checks: Vec<DiagnosticCheck>,
}

impl DoctorReport {
    /// 必須項目がすべて利用可能か。
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != CheckStatus::Fail)
    }
}

/// Doctor が必要とする実環境の境界。
pub trait DoctorPort {
    /// executable を起動して version を返す。
    ///
    /// # Errors
    ///
    /// executable を起動できないか、version 照会が失敗した場合は安全に表示できる理由を返す。
    fn tool_version(&mut self, executable: &str) -> Result<String, String>;

    /// global 設定ストレージを初期化・読み込みできるか確認する。
    ///
    /// # Errors
    ///
    /// ストレージの初期化または設定の読み込みに失敗した場合は安全に表示できる理由を返す。
    fn settings_health(&mut self) -> Result<String, String>;

    /// daemon を起動または接続できるか確認する。
    ///
    /// # Errors
    ///
    /// daemon の起動または接続に失敗した場合は安全に表示できる理由を返す。
    fn daemon_health(&mut self) -> Result<String, String>;
}

struct ToolSpec {
    name: &'static str,
    executable: &'static str,
    required: bool,
}

const TOOLS: [ToolSpec; 4] = [
    ToolSpec {
        name: "Git",
        executable: "git",
        required: true,
    },
    ToolSpec {
        name: "Claude CLI",
        executable: DefaultModel::Claude.command(),
        required: false,
    },
    ToolSpec {
        name: "OpenAI CLI",
        executable: DefaultModel::OpenAi.command(),
        required: false,
    },
    ToolSpec {
        name: "Sakana AI CLI",
        executable: DefaultModel::SakanaAi.command(),
        required: false,
    },
];

/// 必須ツール、任意の Agent CLI、設定ストレージ、daemon を診断する。
#[must_use]
pub fn diagnose(port: &mut dyn DoctorPort) -> DoctorReport {
    let mut checks = TOOLS
        .iter()
        .map(|tool| {
            let result = port.tool_version(tool.executable);
            check_from_result(tool.name, tool.required, result)
        })
        .collect::<Vec<_>>();
    checks.push(check_from_result("Settings", true, port.settings_health()));
    checks.push(check_from_result("Daemon", true, port.daemon_health()));
    DoctorReport { checks }
}

fn check_from_result(
    name: &'static str,
    required: bool,
    result: Result<String, String>,
) -> DiagnosticCheck {
    match result {
        Ok(detail) => DiagnosticCheck {
            name,
            status: CheckStatus::Pass,
            detail,
        },
        Err(detail) => DiagnosticCheck {
            name,
            status: if required {
                CheckStatus::Fail
            } else {
                CheckStatus::Warning
            },
            detail,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};

    struct FakePort {
        tools: BTreeMap<String, Result<String, String>>,
        settings: VecDeque<Result<String, String>>,
        daemon: VecDeque<Result<String, String>>,
        calls: Vec<String>,
    }

    impl DoctorPort for FakePort {
        fn tool_version(&mut self, executable: &str) -> Result<String, String> {
            self.calls.push(executable.to_owned());
            self.tools.remove(executable).unwrap()
        }

        fn settings_health(&mut self) -> Result<String, String> {
            self.calls.push("settings".to_owned());
            self.settings.pop_front().unwrap()
        }

        fn daemon_health(&mut self) -> Result<String, String> {
            self.calls.push("daemon".to_owned());
            self.daemon.pop_front().unwrap()
        }
    }

    fn fake(
        git: Result<&str, &str>,
        claude: Result<&str, &str>,
        openai: Result<&str, &str>,
        sakana_ai: Result<&str, &str>,
        settings: Result<&str, &str>,
        daemon: Result<&str, &str>,
    ) -> FakePort {
        FakePort {
            tools: [
                (
                    "git".to_owned(),
                    git.map(str::to_owned).map_err(str::to_owned),
                ),
                (
                    "claude".to_owned(),
                    claude.map(str::to_owned).map_err(str::to_owned),
                ),
                (
                    "codex".to_owned(),
                    openai.map(str::to_owned).map_err(str::to_owned),
                ),
                (
                    "codex-fugu".to_owned(),
                    sakana_ai.map(str::to_owned).map_err(str::to_owned),
                ),
            ]
            .into_iter()
            .collect(),
            settings: [settings.map(str::to_owned).map_err(str::to_owned)].into(),
            daemon: [daemon.map(str::to_owned).map_err(str::to_owned)].into(),
            calls: Vec::new(),
        }
    }

    #[test]
    fn diagnoses_every_dependency_in_a_stable_order() {
        let mut port = fake(
            Ok("git version 2.50"),
            Ok("claude 2.0"),
            Ok("codex-cli 1.0"),
            Ok("codex-fugu 1.0"),
            Ok("settings.json is readable"),
            Ok("daemon is reachable"),
        );
        let report = diagnose(&mut port);

        assert!(report.is_healthy());
        assert_eq!(
            port.calls,
            ["git", "claude", "codex", "codex-fugu", "settings", "daemon"]
        );
        assert_eq!(report.checks.len(), 6);
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == CheckStatus::Pass)
        );
        assert_eq!(report.checks[0].detail, "git version 2.50");
        assert!(format!("{report:?}").contains("Settings"));
        assert_eq!(report.clone(), report);
    }

    #[test]
    fn distinguishes_optional_warnings_from_required_failures() {
        let mut port = fake(
            Err("not found"),
            Ok("claude 2.0"),
            Err("not found"),
            Err("not found"),
            Err("invalid JSON"),
            Err("connection refused"),
        );
        let report = diagnose(&mut port);

        assert!(!report.is_healthy());
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert_eq!(report.checks[1].status, CheckStatus::Pass);
        assert_eq!(report.checks[2].status, CheckStatus::Warning);
        assert_eq!(report.checks[3].status, CheckStatus::Warning);
        assert_eq!(report.checks[4].status, CheckStatus::Fail);
        assert_eq!(report.checks[5].status, CheckStatus::Fail);
        assert!(format!("{:?}", CheckStatus::Warning).contains("Warning"));
    }
}
