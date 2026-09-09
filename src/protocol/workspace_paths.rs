//! Origin-owned facts used to label Workspace paths on any Client platform.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStyle {
    Unix,
    Windows,
}

/// The owning Server's resolved home and path syntax. Paths stay strings here:
/// a Client's native `Path` parser cannot interpret another platform's roots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePaths {
    pub home: Option<String>,
    pub style: PathStyle,
}

impl WorkspacePaths {
    /// Resolve once on the owning machine, never against a Remote's paths on
    /// the Client. A home that cannot be resolved is unknown for abbreviation.
    pub fn discover() -> Self {
        Self::from_home(dirs::home_dir().as_deref())
    }

    /// Resolve an explicit home on this Server's filesystem. Keeping discovery
    /// outside this boundary also permits isolated home fixtures in tests.
    pub fn from_home(home: Option<&Path>) -> Self {
        Self {
            home: home
                .and_then(|home| crate::paths::canonical(home).ok())
                .and_then(|home| home.into_os_string().into_string().ok()),
            ..Self::default()
        }
    }

    pub fn label(&self, path: &Path) -> String {
        let spelled = self.spelling(&path.to_string_lossy());
        let separator = self.separator();
        if let Some(home) = self.home.as_ref().map(|home| self.spelling(home))
            && !home.is_empty()
            // Unresolved fallback paths do not establish containment.
            && !spelled.split(separator).any(|part| matches!(part, "." | ".."))
            && let Some(rest) = spelled.strip_prefix(home.trim_end_matches(separator))
            && (rest.is_empty() || rest.starts_with(separator))
        {
            return if rest.trim_matches(separator).is_empty() {
                "~".to_owned()
            } else {
                format!("~{rest}")
            };
        }
        spelled
    }

    /// A directory's name in its owning filesystem's syntax. Roots stand for
    /// themselves, using the same label as any other Workspace path.
    pub fn name(&self, path: &Path) -> String {
        let spelled = self.spelling(&path.to_string_lossy());
        let separator = self.separator();
        let trimmed = spelled.trim_end_matches(separator);
        let windows_root = self.style == PathStyle::Windows
            && ((trimmed.len() == 2 && trimmed.ends_with(':'))
                || trimmed
                    .strip_prefix(r"\\")
                    .is_some_and(|share| share.split(separator).count() == 2));
        if trimmed.is_empty() || windows_root {
            self.label(path)
        } else {
            trimmed
                .rsplit(separator)
                .next()
                .unwrap_or(trimmed)
                .to_owned()
        }
    }

    fn separator(&self) -> char {
        match self.style {
            PathStyle::Unix => '/',
            PathStyle::Windows => '\\',
        }
    }

    fn spelling(&self, path: &str) -> String {
        match self.style {
            PathStyle::Unix => path.to_owned(),
            PathStyle::Windows => {
                let path = path.replace('/', "\\");
                if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
                    format!(r"\\{share}")
                } else {
                    path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
                }
            }
        }
    }
}

impl Default for WorkspacePaths {
    fn default() -> Self {
        Self {
            home: None,
            style: if cfg!(windows) {
                PathStyle::Windows
            } else {
                PathStyle::Unix
            },
        }
    }
}
