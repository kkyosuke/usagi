//! Non-interactive presentation runners.

use std::io::{self, Write};
use std::path::Path;

use usagi_core::domain::AppInfo;

use crate::usecase::application::ScreenRunner;
use crate::usecase::doctor::{CheckStatus, DoctorReport};

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_banner(out: &mut impl Write, info: &AppInfo) -> io::Result<()> {
    writeln!(out, "{}", info.describe())
}

/// 選ばれた非対話画面を出力する runner。
///
/// 通常 entry は識別行を、Doctor は注入された診断結果を出力する。出力先とアプリ情報は
/// 呼び出し側から注入するため、実 stdout を直接所有しない。
pub struct BannerScreenRunner<'a, W: Write + ?Sized> {
    out: &'a mut W,
    info: &'a AppInfo,
    doctor_report: Option<&'a DoctorReport>,
}

impl<'a, W: Write + ?Sized> BannerScreenRunner<'a, W> {
    /// 注入された出力先とアプリ情報から runner を作る。
    #[must_use]
    pub fn new(out: &'a mut W, info: &'a AppInfo) -> Self {
        Self {
            out,
            info,
            doctor_report: None,
        }
    }

    /// Doctor の診断結果を表示する runner を作る。
    #[must_use]
    pub fn with_doctor_report(out: &'a mut W, info: &'a AppInfo, report: &'a DoctorReport) -> Self {
        Self {
            out,
            info,
            doctor_report: Some(report),
        }
    }

    /// 画面を識別する `label` をアプリ情報とともに一行で書き出す。
    fn write_screen(&mut self, label: &str) -> io::Result<()> {
        writeln!(self.out, "{}: {label}", self.info.describe())
    }
}

impl<W: Write + ?Sized> ScreenRunner for BannerScreenRunner<'_, W> {
    fn welcome(&mut self) -> io::Result<()> {
        self.write_screen("welcome TUI")
    }

    fn workspace(&mut self, path: &Path) -> io::Result<()> {
        self.write_screen(&format!("workspace TUI ({})", path.display()))
    }

    fn config(&mut self) -> io::Result<()> {
        self.write_screen("config TUI")
    }

    fn doctor(&mut self) -> io::Result<()> {
        let report = self.doctor_report.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "doctor report is required")
        })?;
        writeln!(self.out, "{}: doctor", self.info.describe())?;
        for check in &report.checks {
            let status = match check.status {
                CheckStatus::Pass => "ok",
                CheckStatus::Warning => "warn",
                CheckStatus::Fail => "error",
            };
            writeln!(self.out, "[{status}] {}: {}", check.name, check.detail)?;
        }
        writeln!(
            self.out,
            "{}",
            if report.is_healthy() {
                "result: healthy"
            } else {
                "result: problems found"
            }
        )
    }
}
