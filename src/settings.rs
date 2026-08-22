//! Settings and the Config Documents that pin them.
//!
//! The schema is compile-time: every Setting declares its dotted camelCase
//! key, its scope, and how a pinned JSON value becomes a typed field of
//! [`EffectiveSettings`]. The loader collapses an ordered stack of Config
//! Documents (depth one today; project-level documents later are additive)
//! into one [`SettingsSnapshot`] and never lets a configuration problem stop
//! the server: a file that fails to parse is ignored whole, an unknown or
//! mistyped key is ignored alone, and every ignore becomes a diagnostic
//! naming the file, the key path, and the reason.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use jsonc_parser::ParseOptions;
use serde_json::Value;

use crate::protocol::{
    EffectiveSettings, SettingScope, SettingsDiagnostic, SettingsDiagnosticSeverity,
    SettingsSnapshot,
};

/// The Config Document Suru prefers when both accepted names exist.
pub const PRIMARY_CONFIG_FILE: &str = "suru.jsonc";
/// Accepted when the primary file is absent; parsed just as leniently.
pub const FALLBACK_CONFIG_FILE: &str = "suru.json";

/// One Setting's compile-time definition.
pub struct SettingDescriptor {
    /// Dotted camelCase path of the Setting in a Config Document.
    pub key: &'static str,
    pub scope: SettingScope,
    /// The accepted values, phrased for a diagnostic's "why" clause.
    expected: &'static str,
    /// Writes a pinned JSON value into the typed field it governs, or reports
    /// that the value is not one of the accepted ones.
    apply: fn(&mut EffectiveSettings, &Value) -> bool,
}

/// Every defined Setting. The panel, the loader, and future overlays all read
/// this one table, so adding a Setting is adding its typed field to
/// [`EffectiveSettings`] and its entry here.
pub const SCHEMA: &[SettingDescriptor] = &[
    SettingDescriptor {
        key: "transcript.defaultFoldPosture",
        scope: SettingScope::Client,
        expected: "one of \"folded\" or \"expanded\"",
        apply: |settings, value| {
            apply_value(value, |posture| {
                settings.transcript.default_fold_posture = posture;
            })
        },
    },
    SettingDescriptor {
        key: "provider.codex.reasoningSummary",
        scope: SettingScope::Server,
        expected: "one of \"auto\", \"concise\", \"detailed\", or \"none\"",
        apply: |settings, value| {
            apply_value(value, |detail| {
                settings.provider.codex.reasoning_summary = detail;
            })
        },
    },
];

fn apply_value<T: serde::de::DeserializeOwned>(value: &Value, write: impl FnOnce(T)) -> bool {
    match serde_json::from_value(value.clone()) {
        Ok(value) => {
            write(value);
            true
        }
        Err(_) => false,
    }
}

/// Resolves the config root the way the state directory resolves: an explicit
/// `SURU_CONFIG_DIR` wins, then `$XDG_CONFIG_HOME/suru/`, then the literal
/// `~/.config/suru/` on every platform — raw XDG, replicating opencode,
/// deliberately not the platform-native dirs used for state and data.
pub fn resolve_config_root(
    suru_config_dir: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = suru_config_dir.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = xdg_config_home.filter(|dir| !dir.is_empty()) {
        let xdg = Path::new(xdg);
        // The XDG base-directory spec says a relative XDG_CONFIG_HOME is
        // invalid and must be ignored.
        if xdg.is_absolute() {
            return Some(xdg.join("suru"));
        }
    }
    home_dir.map(|home| home.join(".config").join("suru"))
}

/// Loads the Config Document stack under `config_dir` and collapses it into
/// the effective-settings snapshot. `None` — no configured root — loads pure
/// defaults, which is also what any broken document degrades to.
pub fn load(config_dir: Option<&Path>) -> SettingsSnapshot {
    let mut snapshot = SettingsSnapshot {
        settings: EffectiveSettings::default(),
        pinned: Vec::new(),
        diagnostics: Vec::new(),
    };
    let Some(config_dir) = config_dir else {
        return snapshot;
    };
    for document in discover_documents(config_dir, &mut snapshot.diagnostics) {
        apply_document(&document, &mut snapshot);
    }
    snapshot
}

/// Emits every diagnostic to the Log at the severity it carries.
pub fn log_diagnostics(diagnostics: &[SettingsDiagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.severity {
            SettingsDiagnosticSeverity::Error => tracing::error!(
                file = %diagnostic.file.display(),
                key = diagnostic.key.as_deref(),
                "configuration problem: {}",
                diagnostic.message
            ),
            SettingsDiagnosticSeverity::Warning => tracing::warn!(
                file = %diagnostic.file.display(),
                key = diagnostic.key.as_deref(),
                "configuration problem: {}",
                diagnostic.message
            ),
        }
    }
}

/// The ordered Config Document stack under one root, lowest precedence first.
/// Depth one today: `suru.jsonc`, or `suru.json` when it is the only file,
/// with a diagnostic when both exist and the fallback is ignored.
fn discover_documents(
    config_dir: &Path,
    diagnostics: &mut Vec<SettingsDiagnostic>,
) -> Vec<PathBuf> {
    let primary = config_dir.join(PRIMARY_CONFIG_FILE);
    let fallback = config_dir.join(FALLBACK_CONFIG_FILE);
    match (primary.is_file(), fallback.is_file()) {
        (true, true) => {
            diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Warning,
                file: fallback,
                key: None,
                message: format!("ignored because {PRIMARY_CONFIG_FILE} exists and wins"),
            });
            vec![primary]
        }
        (true, false) => vec![primary],
        (false, true) => vec![fallback],
        (false, false) => Vec::new(),
    }
}

fn apply_document(path: &Path, snapshot: &mut SettingsSnapshot) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: format!("ignored because it could not be read: {error}"),
            });
            return;
        }
    };
    // JSONC per the spec: comments and trailing commas, nothing looser.
    let options = ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    };
    let root = match jsonc_parser::parse_to_serde_value(&text, &options) {
        Ok(root) => root,
        Err(error) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: format!("ignored because it is not valid JSONC: {error}"),
            });
            return;
        }
    };
    match root {
        None | Some(Value::Null) => {}
        Some(Value::Object(entries)) => {
            for (name, value) in &entries {
                apply_key(path, "", name, value, snapshot);
            }
        }
        Some(_) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: "ignored because its top level is not an object".to_owned(),
            });
        }
    }
}

fn apply_key(
    path: &Path,
    prefix: &str,
    name: &str,
    value: &Value,
    snapshot: &mut SettingsSnapshot,
) {
    let key = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    };
    // A property name containing a dot would collide with the nested spelling
    // the schema defines; admitting it as a second spelling would leave pins
    // the future format-preserving editor cannot target.
    if name.contains('.') {
        snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because Settings nest as objects, not dotted names".to_owned(),
        });
        return;
    }
    if let Some(descriptor) = SCHEMA.iter().find(|descriptor| descriptor.key == key) {
        if (descriptor.apply)(&mut snapshot.settings, value) {
            if !snapshot.pinned.contains(&key) {
                snapshot.pinned.push(key);
            }
        } else {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Warning,
                file: path.to_path_buf(),
                key: Some(key),
                message: format!("ignored because its value is not {}", descriptor.expected),
            });
        }
        return;
    }
    let is_group = SCHEMA
        .iter()
        .any(|descriptor| descriptor.key.starts_with(&format!("{key}.")));
    if !is_group {
        snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because it is not a known Setting".to_owned(),
        });
        return;
    }
    match value {
        Value::Object(entries) => {
            for (name, value) in entries {
                apply_key(path, &key, name, value, snapshot);
            }
        }
        _ => snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because it should be an object of Settings".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suru_config_dir_overrides_every_other_config_root() {
        let root = resolve_config_root(
            Some(OsStr::new("/tmp/override")),
            Some(OsStr::new("/home/user/.xdg")),
            Some(Path::new("/home/user")),
        );
        assert_eq!(root, Some(PathBuf::from("/tmp/override")));
    }

    #[test]
    fn xdg_config_home_hosts_the_suru_directory() {
        let root = resolve_config_root(
            None,
            Some(OsStr::new("/home/user/.xdg")),
            Some(Path::new("/home/user")),
        );
        assert_eq!(root, Some(PathBuf::from("/home/user/.xdg/suru")));
    }

    #[test]
    fn relative_or_empty_xdg_config_home_falls_back_to_the_home_config_directory() {
        for invalid in ["relative/config", ""] {
            let root = resolve_config_root(
                None,
                Some(OsStr::new(invalid)),
                Some(Path::new("/home/user")),
            );
            assert_eq!(root, Some(PathBuf::from("/home/user/.config/suru")));
        }
    }

    #[test]
    fn without_any_environment_there_is_no_config_root() {
        assert_eq!(resolve_config_root(None, None, None), None);
    }
}
