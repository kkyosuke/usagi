//! Bounded native Artifact opening, with filesystem copies off the render thread.

use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;

use usagi_core::domain::id::RequestId;
use usagi_tui::usecase::application::controller::{
    AppEvent, ArtifactOpenOutcome, BackendEvent, PreviewFileFilter, SafeError, SafeMessage, Target,
};
use usagi_tui::usecase::application::daemon_backend::Completions;

use super::file_preview::artifact_open_view;
use super::preview_authority::PreviewAuthority;

/// Native helpers run on this worker and report exit or bounded uncertainty.
pub(crate) trait ArtifactViewer: Send {
    fn open(&mut self, path: &str) -> Result<ArtifactOpenOutcome, String>;
}

struct Request {
    root: PathBuf,
    target: Target,
    id: RequestId,
    path: String,
    filter: PreviewFileFilter,
    completions: Completions,
}

/// One active request and one queued request; every request reports a fenced result.
pub(crate) struct ArtifactOpener {
    sender: Option<SyncSender<Request>>,
    worker: Option<JoinHandle<()>>,
}

impl ArtifactOpener {
    pub(crate) fn spawn(
        mut browser: Box<dyn ArtifactViewer>,
        mut authority: Box<dyn PreviewAuthority>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            while let Ok(request) = receiver.recv() {
                open(&request, &mut *browser, &mut *authority);
            }
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    pub(crate) fn request(
        &self,
        root: PathBuf,
        target: Target,
        request_id: RequestId,
        path: String,
        filter: PreviewFileFilter,
        completions: Completions,
    ) {
        let request = Request {
            root,
            target,
            id: request_id,
            path,
            filter,
            completions,
        };
        if let Err(mpsc::TrySendError::Full(request) | mpsc::TrySendError::Disconnected(request)) =
            self.sender.as_ref().unwrap().try_send(request)
        {
            finish(
                &request,
                Err(SafeError {
                    message: SafeMessage::new("Artifact viewer is busy or unavailable. Try again."),
                    error_id: "artifact-viewer-busy".into(),
                }),
            );
        }
    }
}

fn finish(request: &Request, result: Result<ArtifactOpenOutcome, SafeError>) {
    request
        .completions
        .emit(AppEvent::Backend(BackendEvent::ArtifactOpenFinished {
            target: request.target,
            request_id: request.id,
            path: request.path.clone(),
            filter: request.filter,
            result,
        }));
}

fn open(request: &Request, browser: &mut dyn ArtifactViewer, authority: &mut dyn PreviewAuthority) {
    let view = authority
        .validate(request.target, &request.root)
        .and_then(|()| artifact_open_view(&request.root, &request.path, request.filter));
    let result = match view {
        Ok(view) => match authority.validate(request.target, &request.root) {
            Ok(()) => match browser.open(&view.path().to_string_lossy()) {
                Ok(outcome) => {
                    view.retain();
                    Ok(outcome)
                }
                Err(_) => Err(SafeError {
                    message: SafeMessage::new("Could not open this artifact in an external app."),
                    error_id: "artifact-viewer-open".into(),
                }),
            },
            Err(error) => Err(preview_error(error)),
        },
        Err(error) => Err(preview_error(error)),
    };
    finish(request, result);
}

fn preview_error(error: super::file_preview::FilePreviewError) -> SafeError {
    SafeError {
        message: SafeMessage::new(error.message()),
        error_id: error.error_id().into(),
    }
}

impl Drop for ArtifactOpener {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::mpsc::{Receiver, Sender};
    use std::time::Duration;

    use super::*;

    struct Browser {
        opened: Sender<PathBuf>,
        release: Option<Receiver<()>>,
        fail: bool,
    }

    impl ArtifactViewer for Browser {
        fn open(&mut self, path: &str) -> Result<ArtifactOpenOutcome, String> {
            self.opened.send(PathBuf::from(path)).unwrap();
            if let Some(release) = &self.release {
                release.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            if self.fail {
                Err("unavailable".into())
            } else {
                Ok(ArtifactOpenOutcome::Sent)
            }
        }
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("artifacts")).unwrap();
        fs::write(root.path().join("artifacts/report.md"), "report").unwrap();
        root
    }

    fn request(opener: &ArtifactOpener, root: &Path, path: &str) -> Receiver<AppEvent> {
        let (completions, events) = Completions::channel();
        opener.request(
            root.to_path_buf(),
            Target::Session(usagi_core::domain::id::SessionId::new()),
            RequestId::new(),
            path.into(),
            PreviewFileFilter::Artifacts,
            completions,
        );
        events
    }

    #[test]
    fn native_view_survives_helper_and_tui_lifetimes_and_source_removal() {
        let root = fixture();
        let (delivered, received) = mpsc::channel();
        let opener = ArtifactOpener::spawn(
            Box::new(Browser {
                opened: delivered,
                release: None,
                fail: false,
            }),
            Box::new(crate::runtime::preview_authority::AllowPreview),
        );
        let events = request(&opener, root.path(), "artifacts/report.md");
        let path = received.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(opener);
        drop(root);
        assert_eq!(fs::read_to_string(&path).unwrap(), "report");
        assert!(matches!(
            events.try_recv(),
            Ok(AppEvent::Backend(BackendEvent::ArtifactOpenFinished {
                result: Ok(ArtifactOpenOutcome::Sent),
                ..
            }))
        ));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failures_report_results_and_discard_unopened_copies() {
        let root = fixture();
        let (delivered, received) = mpsc::channel();
        let opener = ArtifactOpener::spawn(
            Box::new(Browser {
                opened: delivered,
                release: None,
                fail: true,
            }),
            Box::new(crate::runtime::preview_authority::AllowPreview),
        );
        let events = request(&opener, root.path(), "artifacts/report.md");
        let path = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(_), .. })
        ));
        let errors = request(&opener, root.path(), "artifacts/missing.md");
        assert!(matches!(
            errors.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(_), .. })
        ));
        drop(opener);
        assert!(!path.exists());
    }

    #[test]
    fn native_open_admission_is_bounded_and_never_blocks_the_caller() {
        let root = fixture();
        let (delivered, received) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let opener = ArtifactOpener::spawn(
            Box::new(Browser {
                opened: delivered,
                release: Some(gate),
                fail: true,
            }),
            Box::new(crate::runtime::preview_authority::AllowPreview),
        );
        let first = request(&opener, root.path(), "artifacts/report.md");
        let path = received.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = request(&opener, root.path(), "artifacts/missing.md");
        let rejected = request(&opener, root.path(), "artifacts/report.md");
        assert!(matches!(
            rejected.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(_), .. })
        ));
        release.send(()).unwrap();
        drop(opener);
        assert!(!path.exists());
        assert!(first.try_recv().is_ok());
        assert!(second.try_recv().is_ok());
    }

    #[test]
    fn a_stopped_native_worker_reports_a_notice() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let opener = ArtifactOpener {
            sender: Some(sender),
            worker: None,
        };
        let events = request(&opener, Path::new("/unused"), "artifacts/report.md");
        assert!(matches!(
            events.try_recv().unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(_), .. })
        ));
    }

    struct Revoked {
        checks: usize,
        deny_at: usize,
    }

    impl PreviewAuthority for Revoked {
        fn validate(
            &mut self,
            _: Target,
            _: &Path,
        ) -> Result<(), super::super::file_preview::FilePreviewError> {
            self.checks += 1;
            if self.checks == self.deny_at {
                Err(super::super::file_preview::FilePreviewError::ScopeUnavailable)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn revoked_targets_are_rejected_before_copying_and_before_external_dispatch() {
        for deny_at in [1, 2] {
            let root = fixture();
            let (delivered, received) = mpsc::channel();
            let opener = ArtifactOpener::spawn(
                Box::new(Browser {
                    opened: delivered,
                    release: None,
                    fail: false,
                }),
                Box::new(Revoked { checks: 0, deny_at }),
            );
            let events = request(&opener, root.path(), "artifacts/report.md");
            assert!(matches!(
                events.recv_timeout(Duration::from_secs(5)).unwrap(),
                AppEvent::Backend(BackendEvent::ArtifactOpenFinished { result: Err(error), .. })
                    if error.error_id == "preview-scope"
            ));
            drop(opener);
            assert!(received.try_recv().is_err());
        }
    }
}
