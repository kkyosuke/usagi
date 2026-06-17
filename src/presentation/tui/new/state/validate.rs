//! Form validation: turning the entered fields into a [`NewProject`].

use super::*;

impl FormState {
    /// Validate the form into a [`NewProject`], or return a user-facing error.
    pub fn validate(&self) -> Result<NewProject, String> {
        match self.mode {
            Mode::Clone => self.validate_clone().map(NewProject::Clone),
            Mode::Existing => self.validate_existing().map(NewProject::Existing),
        }
    }

    fn validate_clone(&self) -> Result<CloneSpec, String> {
        let url = RepoUrl::parse(&self.url).map_err(|e| e.to_string())?;
        let directory = self.directory.trim();
        let directory = if directory.is_empty() {
            url.directory_name()
        } else {
            directory.to_string()
        };
        let location = self.location.trim();
        if location.is_empty() {
            return Err("choose where to create the project".to_string());
        }
        let location = PathBuf::from(location);
        let branch = match self.branch.trim() {
            "" => None,
            b => Some(b.to_string()),
        };
        Ok(CloneSpec {
            url,
            location,
            directory,
            branch,
        })
    }

    fn validate_existing(&self) -> Result<ExistingSpec, String> {
        let path = self.path.trim();
        if path.is_empty() {
            return Err("choose an existing directory".to_string());
        }
        let name = match self.name.trim() {
            "" => suggest_name(path),
            n => n.to_string(),
        };
        if name.is_empty() {
            return Err("enter a name for the workspace".to_string());
        }
        Ok(ExistingSpec {
            path: PathBuf::from(path),
            name,
        })
    }
}
