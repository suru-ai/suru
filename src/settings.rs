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
//!
//! The server is also the only writer. A typed mutation becomes a
//! format-preserving CST edit of the winning Config Document, so a user's key
//! order, spacing, and comments survive an edit Suru makes; the file it leaves
//! behind is then reloaded, which keeps the file — not an in-memory shadow of
//! it — the source of truth for what every client is told.

use std::{
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstRootNode},
};
use serde_json::Value;

use crate::protocol::{
    EffectiveSettings, SettingMutation, SettingScope, SettingsDiagnostic,
    SettingsDiagnosticSeverity, SettingsSnapshot,
};

/// The Config Document Suru prefers when both accepted names exist.
pub const PRIMARY_CONFIG_FILE: &str = "suru.jsonc";
/// Accepted when the primary file is absent; parsed just as leniently.
pub const FALLBACK_CONFIG_FILE: &str = "suru.json";

// The dotted key of each Setting, named once so the schema and the typed
// mutations that edit it can never drift apart.
const TRANSCRIPT_DEFAULT_FOLD_POSTURE: &str = "transcript.defaultFoldPosture";
const PROVIDER_CODEX_REASONING_SUMMARY: &str = "provider.codex.reasoningSummary";

/// What a Config Document that does not exist yet is edited as.
const EMPTY_DOCUMENT: &str = "{}\n";

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
/// this one table: a Setting arrives as its typed field on
/// [`EffectiveSettings`], its entry here, and the [`SettingMutation`] variant
/// through which a client edits it — the key each of them spells is a constant
/// above, so the three can never drift apart.
pub const SCHEMA: &[SettingDescriptor] = &[
    SettingDescriptor {
        key: TRANSCRIPT_DEFAULT_FOLD_POSTURE,
        scope: SettingScope::Client,
        expected: "one of \"folded\" or \"expanded\"",
        apply: |settings, value| {
            apply_value(value, |posture| {
                settings.transcript.default_fold_posture = posture;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CODEX_REASONING_SUMMARY,
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

/// The Config Document stack the server reads and — as the file's only writer
/// — edits. Cloneable so every request handler shares the one root and the one
/// writer, which makes each read–edit–write of the document the atomic unit
/// even while handlers run concurrently.
#[derive(Clone)]
pub struct ConfigDocuments {
    /// `None` when no config root resolved: Suru runs on built-in defaults and
    /// has nowhere to pin a Setting.
    root: Option<PathBuf>,
    writer: Arc<Mutex<()>>,
}

impl ConfigDocuments {
    pub fn new(root: Option<&Path>) -> Self {
        Self {
            root: root.map(Path::to_path_buf),
            writer: Arc::new(Mutex::new(())),
        }
    }

    /// Collapses the stack into the effective-settings snapshot.
    pub fn load(&self) -> SettingsSnapshot {
        load(self.root.as_deref())
    }

    /// Applies one typed mutation to the winning Config Document — creating
    /// the document, and any intermediate objects, when they are missing — and
    /// returns the snapshot the reloaded stack now yields.
    pub fn mutate(
        &self,
        mutation: &SettingMutation,
    ) -> Result<SettingsSnapshot, SettingsMutationError> {
        let Some(root) = self.root.as_deref() else {
            return Err(SettingsMutationError::NoConfigRoot);
        };
        let _writer = self
            .writer
            .lock()
            .expect("Config Document writer lock is not poisoned");
        let (key, value) = pin_for(mutation);
        write_pin(root, key, value.as_ref())?;
        Ok(load(Some(root)))
    }
}

/// Why a typed mutation could not reach the Config Document.
#[derive(Debug)]
pub enum SettingsMutationError {
    /// No config root resolved, so there is nowhere to pin a Setting.
    NoConfigRoot,
    /// The document on disk cannot take a surgical edit: it does not parse, or
    /// something that is not an object stands where a Setting's group belongs.
    /// Suru never rewrites a document to fix it.
    NotEditable { path: PathBuf, reason: String },
    /// The filesystem refused the read or the write the edit needed.
    Io { path: PathBuf, message: String },
}

impl fmt::Display for SettingsMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigRoot => write!(
                formatter,
                "no config root is configured, so there is nowhere to pin a Setting"
            ),
            Self::NotEditable { path, reason } => {
                write!(formatter, "Config Document {path:?} {reason}")
            }
            Self::Io { path, message } => {
                write!(formatter, "Config Document {path:?} {message}")
            }
        }
    }
}

impl std::error::Error for SettingsMutationError {}

/// The Setting a mutation targets, spelled as the schema's dotted key, and the
/// JSON that pins it — `None` to remove the pin.
fn pin_for(mutation: &SettingMutation) -> (&'static str, Option<Value>) {
    fn pinned<T: serde::Serialize>(value: &Option<T>) -> Option<Value> {
        value
            .as_ref()
            .map(|value| serde_json::to_value(value).expect("Setting values always serialize"))
    }
    match mutation {
        SettingMutation::TranscriptDefaultFoldPosture { value } => {
            (TRANSCRIPT_DEFAULT_FOLD_POSTURE, pinned(value))
        }
        SettingMutation::ProviderCodexReasoningSummary { value } => {
            (PROVIDER_CODEX_REASONING_SUMMARY, pinned(value))
        }
    }
}

/// Reads the winning Config Document, edits it, and writes it back. Parsing,
/// editing, and serializing happen in this one scope because CST handles are
/// not `Send`; an edit that changes nothing writes nothing.
fn write_pin(
    config_dir: &Path,
    key: &str,
    value: Option<&Value>,
) -> Result<(), SettingsMutationError> {
    // The document a pin lands in is the one the loader reads back: the
    // highest-precedence document of the stack, or the primary name when the
    // user has no Config Document yet.
    let path = discover_documents(config_dir, &mut Vec::new())
        .pop()
        .unwrap_or_else(|| config_dir.join(PRIMARY_CONFIG_FILE));
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(SettingsMutationError::Io {
                path,
                message: format!("could not be read: {error}"),
            });
        }
    };
    // A document that does not exist yet is edited as if it were an empty
    // object, so the first pin has somewhere to land. An edit that leaves that
    // object as empty as it found it — an unset of something never pinned —
    // writes nothing, so a reset on a fresh install creates no file.
    let before = existing.as_deref().unwrap_or(EMPTY_DOCUMENT);
    let edited = edited_document(before, &path, key, value)?;
    if edited == before {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| SettingsMutationError::Io {
            path: path.clone(),
            message: format!("could not be created: its directory does not exist ({error})"),
        })?;
    }
    fs::write(&path, edited).map_err(|error| SettingsMutationError::Io {
        path,
        message: format!("could not be written: {error}"),
    })
}

/// The document's text after the edit: the pin written where the schema's key
/// path says it belongs, or removed from there, with every other byte of the
/// document left as its author wrote it.
fn edited_document(
    text: &str,
    path: &Path,
    key: &str,
    value: Option<&Value>,
) -> Result<String, SettingsMutationError> {
    let not_editable = |reason: &str| SettingsMutationError::NotEditable {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    };
    let document = CstRootNode::parse(text, &parse_options())
        .map_err(|error| not_editable(&format!("is not valid JSONC: {error}")))?;
    let mut object = match document.object_value() {
        Some(object) => object,
        // A document holding nothing but comments still has room for a pin.
        None if document.value().is_none() => document.object_value_or_set(),
        None => return Err(not_editable("does not hold an object at its top level")),
    };
    let mut names = key.split('.').collect::<Vec<_>>();
    let name = names.pop().expect("every schema key names a Setting");
    // Each group on the way in, with the object holding it, so an unset can
    // take the groups that existed only for the pin it removes.
    let mut groups: Vec<(CstObject, &str, CstObject)> = Vec::new();
    for group in names {
        let holder = object.clone();
        object = match value {
            Some(_) => holder.object_value_or_create(group).ok_or_else(|| {
                not_editable(&format!(
                    "holds something other than an object at {group:?}, where this Setting belongs"
                ))
            })?,
            // Nothing to unset below a group the document never wrote.
            None => match holder.object_value(group) {
                Some(group) => group,
                None => return Ok(text.to_owned()),
            },
        };
        groups.push((holder, group, object.clone()));
    }
    match value {
        Some(value) => match object.get(name) {
            Some(property) => property.set_value(input_value(value)),
            None => {
                object.append(name, input_value(value));
            }
        },
        None => {
            let Some(property) = object.get(name) else {
                return Ok(text.to_owned());
            };
            property.remove();
            // A group that held only the removed pin goes with it, so the
            // document stays as sparse as the user's own hand would keep it. A
            // group still holding a comment stays: that comment is the
            // author's to remove, not Suru's.
            for (holder, group_name, group) in groups.into_iter().rev() {
                let is_spent = group.properties().is_empty()
                    && !group.children().iter().any(CstNode::is_comment);
                if !is_spent {
                    break;
                }
                if let Some(property) = holder.get(group_name) {
                    property.remove();
                }
            }
        }
    }
    Ok(document.to_string())
}

fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input_value).collect()),
        Value::Object(entries) => CstInputValue::Object(
            entries
                .iter()
                .map(|(name, value)| (name.clone(), input_value(value)))
                .collect(),
        ),
    }
}

/// JSONC per the spec: comments and trailing commas, nothing looser. The
/// loader and the editor read every document the same way.
fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Loads the Config Document stack under `config_dir` and collapses it into
/// the effective-settings snapshot. `None` — no configured root — loads pure
/// defaults, which is also what any broken document degrades to.
fn load(config_dir: Option<&Path>) -> SettingsSnapshot {
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
    let root = match jsonc_parser::parse_to_serde_value(&text, &parse_options()) {
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
