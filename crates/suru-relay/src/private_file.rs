//! Files holding a private key the Relay is given — its certificate's, and
//! its GitHub App's — which only the user it runs as should be able to read.

use std::path::Path;

/// Warns, on Unix, where users other than the owner of the file at `path`,
/// which holds `holding`, may read or change it. Elsewhere, who may read a
/// file is not told by its mode, and nothing is said.
pub(crate) fn warn_if_others_may_read(path: &Path, holding: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.permissions().mode() & 0o077 != 0
        {
            tracing::warn!(
                "{path:?}, which holds {holding}, may be read or changed by users other than its \
                 owner; make it readable by the Relay's user alone, as `chmod 600` does"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, holding);
}
