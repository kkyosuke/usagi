//! Update progress describes completed phases and durable Agent recovery counts.

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartStage {
    Preparing,
    Stopping,
    Switching,
    Resuming { completed: usize, total: usize },
    Verifying,
    Finished,
}

/// A missing transaction before commit is preparation, never completion.
#[must_use]
pub fn restart_stage(
    committed: bool,
    switching: bool,
    recovery: Option<(usize, usize)>,
) -> RestartStage {
    if committed {
        recovery.map_or(RestartStage::Verifying, |(completed, total)| {
            RestartStage::Resuming { completed, total }
        })
    } else if switching {
        RestartStage::Switching
    } else if recovery.is_some() {
        RestartStage::Stopping
    } else {
        RestartStage::Preparing
    }
}

pub struct RestartProgress<W> {
    writer: W,
    terminal: bool,
    update: bool,
    last: Option<RestartStage>,
}

impl<W: Write> RestartProgress<W> {
    pub const fn new(writer: W, terminal: bool, update: bool) -> Self {
        Self {
            writer,
            terminal,
            update,
            last: None,
        }
    }

    /// Render a changed stage and flush it immediately.
    ///
    /// # Errors
    /// Returns the output writer's error if writing or flushing fails.
    pub fn show(&mut self, stage: RestartStage) -> io::Result<()> {
        if self.last == Some(stage) {
            return Ok(());
        }
        let done = match stage {
            RestartStage::Preparing | RestartStage::Stopping => 0,
            RestartStage::Switching => 1,
            RestartStage::Resuming { .. } | RestartStage::Verifying => 2,
            RestartStage::Finished => 3,
        } + if self.update { 3 } else { 0 };
        let total = if self.update { 6 } else { 3 };
        let label = match stage {
            RestartStage::Preparing => "Agent の更新を準備しています".to_owned(),
            RestartStage::Stopping => "Agent を停止しています".to_owned(),
            RestartStage::Switching => "daemon を切り替えています".to_owned(),
            RestartStage::Resuming { completed, total } => {
                format!("Agent を再開しています ({completed}/{total} 件)")
            }
            RestartStage::Verifying => "Agent の再開状況を確認しています".to_owned(),
            RestartStage::Finished => "更新と再開が完了しました".to_owned(),
        };
        if self.terminal {
            write!(self.writer, "\r\x1b[2K")?;
        }
        write!(
            self.writer,
            "[{}{}] {done}/{total} {label}",
            "#".repeat(done),
            "-".repeat(total - done)
        )?;
        if !self.terminal || stage == RestartStage::Finished {
            writeln!(self.writer)?;
        }
        self.writer.flush()?;
        self.last = Some(stage);
        Ok(())
    }

    /// Finish with verified completion or an explicit incomplete notice.
    ///
    /// # Errors
    /// Returns the output writer's error if the final notice cannot be written.
    pub fn finish(&mut self, success: bool) -> io::Result<()> {
        if success {
            self.show(RestartStage::Finished)
        } else {
            if self.terminal {
                writeln!(self.writer)?;
            }
            writeln!(
                self.writer,
                "更新・再開は未完了です。usagi daemon status で状況を確認してください"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_and_recovery_counts_never_claim_completion_before_serving() {
        assert_eq!(restart_stage(false, false, None), RestartStage::Preparing);
        assert_eq!(
            restart_stage(false, false, Some((0, 4))),
            RestartStage::Stopping
        );
        assert_eq!(
            restart_stage(false, true, Some((0, 4))),
            RestartStage::Switching
        );
        assert_eq!(
            restart_stage(true, true, Some((2, 4))),
            RestartStage::Resuming {
                completed: 2,
                total: 4
            }
        );
        assert_eq!(restart_stage(true, false, None), RestartStage::Verifying);
        let mut output = Vec::new();
        let mut progress = RestartProgress::new(&mut output, false, true);
        for stage in [
            RestartStage::Preparing,
            RestartStage::Stopping,
            RestartStage::Stopping,
            RestartStage::Switching,
            RestartStage::Resuming {
                completed: 2,
                total: 4,
            },
            RestartStage::Verifying,
        ] {
            progress.show(stage).unwrap();
        }
        progress.finish(false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("6/6"));
        assert!(!text.contains('\x1b'));
        assert_eq!(text.matches("Agent を停止").count(), 1);
        assert!(text.contains("(2/4 件)"));
        assert!(text.contains("Agent の再開状況を確認しています"));
        assert!(!text.contains("(4/4 件)"));
        assert!(text.contains("未完了"));
    }

    #[test]
    fn terminals_replace_the_current_line_and_success_finishes_the_bar() {
        let mut output = Vec::new();
        let mut progress = RestartProgress::new(&mut output, true, false);
        progress.show(RestartStage::Preparing).unwrap();
        progress.finish(true).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("\r\x1b[2K"));
        assert!(text.contains("[###] 3/3"));
        assert!(text.ends_with('\n'));
        let mut broken = RestartProgress::new(&mut [][..], true, true);
        assert!(broken.show(RestartStage::Preparing).is_err());
        assert!(broken.finish(false).is_err());
        let mut broken = RestartProgress::new(&mut [][..], false, true);
        assert!(broken.show(RestartStage::Preparing).is_err());
        assert!(broken.finish(false).is_err());
    }
}
