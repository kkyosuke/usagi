//! Observe native helper exits without blocking the UI or terminating GUI apps.

use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use usagi_tui::usecase::application::controller::ArtifactOpenOutcome;

use super::artifact_opener::ArtifactViewer;
use super::platform_child_reaper::{HelperExit, PlatformChildReaper};

const HELPER_TIMEOUT: Duration = Duration::from_secs(2);
type OpenCommand = Box<dyn FnMut(&str) -> Result<Command, String> + Send>;

pub(crate) struct PlatformArtifactViewer {
    reaper: PlatformChildReaper,
    command: OpenCommand,
    timeout: Duration,
}

fn helper_result(
    result: Result<HelperExit, RecvTimeoutError>,
) -> Result<ArtifactOpenOutcome, String> {
    match result {
        Ok(HelperExit::Success) => Ok(ArtifactOpenOutcome::Sent),
        Ok(HelperExit::Failure) => Err("native helper failed".to_owned()),
        Ok(HelperExit::Unconfirmed) | Err(_) => Ok(ArtifactOpenOutcome::Unconfirmed),
    }
}

impl PlatformArtifactViewer {
    pub(crate) fn new(reaper: PlatformChildReaper) -> Self {
        Self {
            reaper,
            command: Box::new(super::tui::platform_open_command),
            timeout: HELPER_TIMEOUT,
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=native_viewer_observes_helpers_and_preserves_unconfirmed_launches
impl ArtifactViewer for PlatformArtifactViewer {
    fn open(&mut self, path: &str) -> Result<ArtifactOpenOutcome, String> {
        let mut command = (self.command)(path)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let exit = self
            .reaper
            .spawn_observed(&mut command)
            .map_err(|_| "native helper could not start".to_owned())?;
        helper_result(exit.recv_timeout(self.timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::artifact_opener::ArtifactOpener;
    use crate::runtime::preview_authority::AllowPreview;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Instant;
    use usagi_core::domain::id::{RequestId, SessionId};
    use usagi_tui::usecase::application::controller::{
        AppEvent, BackendEvent, PreviewFileFilter, Target,
    };
    use usagi_tui::usecase::application::daemon_backend::Completions;

    #[test]
    fn helper_results_distinguish_failure_from_unconfirmed_dispatch() {
        assert_eq!(
            helper_result(Ok(HelperExit::Success)),
            Ok(ArtifactOpenOutcome::Sent)
        );
        assert!(helper_result(Ok(HelperExit::Failure)).is_err());
        for result in [
            Ok(HelperExit::Unconfirmed),
            Err(RecvTimeoutError::Timeout),
            Err(RecvTimeoutError::Disconnected),
        ] {
            assert_eq!(helper_result(result), Ok(ArtifactOpenOutcome::Unconfirmed));
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "helper did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn request(opener: &ArtifactOpener, root: &std::path::Path) -> mpsc::Receiver<AppEvent> {
        let (completion, events) = Completions::channel();
        opener.request(
            root.to_path_buf(),
            Target::Session(SessionId::new()),
            RequestId::new(),
            "artifacts/report.md".into(),
            PreviewFileFilter::Artifacts,
            completion,
        );
        events
    }

    #[test]
    fn native_viewer_observes_helpers_and_preserves_unconfirmed_launches() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("artifacts")).unwrap();
        fs::write(root.path().join("artifacts/report.md"), "private report").unwrap();
        let reaper = PlatformChildReaper::default();
        for code in [0, 7] {
            let (delivered, received) = mpsc::channel();
            let viewer = PlatformArtifactViewer {
                reaper: reaper.clone(),
                timeout: Duration::from_secs(2),
                command: Box::new(move |path| {
                    delivered.send(PathBuf::from(path)).unwrap();
                    let mut command = Command::new("/bin/sh");
                    command
                        .args([
                            "-c",
                            "printf helper-stdout; printf helper-stderr >&2; exit \"$1\"",
                            "fixture",
                        ])
                        .arg(code.to_string());
                    Ok(command)
                }),
            };
            let opener = ArtifactOpener::spawn(Box::new(viewer), Box::new(AllowPreview));
            let events = request(&opener, root.path());
            let copy = received.recv_timeout(Duration::from_secs(5)).unwrap();
            let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(opener);
            if code == 0 {
                assert!(matches!(
                    event,
                    AppEvent::Backend(BackendEvent::ArtifactOpenFinished {
                        result: Ok(ArtifactOpenOutcome::Sent),
                        ..
                    })
                ));
                assert_eq!(fs::read_to_string(&copy).unwrap(), "private report");
                fs::remove_dir_all(copy.parent().unwrap()).unwrap();
            } else {
                assert!(
                    matches!(event, AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(error), .. }) if error.error_id == "artifact-viewer-open")
                );
                assert!(!copy.exists());
            }
        }
        let command = super::super::tui::platform_open_command("/fixture/report.html").unwrap();
        assert!(
            command
                .get_args()
                .any(|argument| argument == "/fixture/report.html")
        );
        wait_until(|| reaper.tracked() == Some(0));
    }

    #[test]
    fn unconfirmed_helpers_keep_their_copy_and_continue_after_worker_shutdown() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("artifacts")).unwrap();
        fs::write(root.path().join("artifacts/report.md"), "private report").unwrap();
        let reaper = PlatformChildReaper::default();
        let marker = root.path().join("late-helper-read");
        let late_marker = marker.clone();
        let release = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        let helper_release = release.path().to_path_buf();
        let (delivered, received) = mpsc::channel();
        let viewer = PlatformArtifactViewer {
            reaper: reaper.clone(),
            timeout: Duration::from_millis(1),
            command: Box::new(move |path| {
                delivered.send(PathBuf::from(path)).unwrap();
                let mut command = Command::new("/bin/sh");
                command
                    .args([
                        "-c",
                        "while test -f \"$3\"; do sleep 0.01; done; cat \"$1\" > \"$2\"; exit 7",
                        "fixture",
                    ])
                    .arg(path)
                    .arg(&late_marker)
                    .arg(&helper_release);
                Ok(command)
            }),
        };
        let opener = ArtifactOpener::spawn(Box::new(viewer), Box::new(AllowPreview));
        let events = request(&opener, root.path());
        let copy = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished {
                result: Ok(ArtifactOpenOutcome::Unconfirmed),
                ..
            })
        ));
        drop(opener);
        assert!(
            !marker.exists(),
            "the helper must still be awaiting release"
        );
        drop(release);
        wait_until(|| reaper.tracked() == Some(0));
        assert_eq!(fs::read_to_string(marker).unwrap(), "private report");
        fs::remove_dir_all(copy.parent().unwrap()).unwrap();
    }

    #[test]
    fn native_viewer_reports_command_and_spawn_failures() {
        let mut viewer = PlatformArtifactViewer::new(PlatformChildReaper::default());
        viewer.command = Box::new(|_| Err("unsupported platform fixture".into()));
        assert!(viewer.open("unused").is_err());
        viewer.command = Box::new(|_| Ok(Command::new("/nonexistent/usagi-native-helper")));
        assert!(viewer.open("unused").is_err());
    }
}
