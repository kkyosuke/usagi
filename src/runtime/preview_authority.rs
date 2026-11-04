//! Session identity checks shared by asynchronous file readers and native views.

use std::path::Path;

use usagi_tui::usecase::application::controller::Target;

use super::file_preview::FilePreviewError;

/// Called on the worker before IO and again before exposing its result.
pub(crate) trait PreviewAuthority: Send {
    fn validate(&mut self, target: Target, root: &Path) -> Result<(), FilePreviewError>;
}

#[cfg(test)]
pub(crate) struct AllowPreview;

#[cfg(test)]
impl PreviewAuthority for AllowPreview {
    fn validate(&mut self, _: Target, _: &Path) -> Result<(), FilePreviewError> {
        Ok(())
    }
}
