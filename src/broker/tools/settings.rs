//! `list_settings`, `describe_setting` and `set_setting`: the Tools through
//! which a Sidekick reads and changes the Settings of its own Server. Settings
//! stay with that Server — a Remote shares its work, not its administration
//! (ADR 0044) — so none of the three takes an `origin`.
//!
//! All three read the Settings schema and nothing besides: a Setting is
//! listed by its key and the value in force, described by its own words, what
//! it accepts and whether it is pinned, and set by any value it accepts, each
//! value spelled as a Config Document spells it. A Setting declared in the
//! schema reaches them with no word more.
//!
//! What a Sidekick may not touch is bounded here, where the Tools are served,
//! by what each Setting declares of a Sidekick in the schema (ADR 0043): a
//! Setting governing Serving or a Pairing is neither listed, described nor
//! set, nor is its value ever told — a key in a namespace reserved for them is
//! refused whether or not a Setting has it — and a Setting bounding what
//! Agents may do or doing what cannot be undone is read but never changed.
//! Every other Setting is changed through the very operation the settings
//! panel's change goes through ([`SessionOperations::change_setting`]), so the
//! Config Document is edited in place, and the change takes effect and
//! reaches every Client exactly as the user's does.
//!
//! What a Tool answers is worded here from what the operation tells it, never
//! passed on as some other part of Suru wrote it: a failure of the Serving
//! listener names the address and port it could not take, which are the
//! values of Settings no Sidekick may read, so a Sidekick is told only that
//! Serving could not follow, and the user's own surfaces and the Log keep
//! the rest.
//!
//! A Client Setting is the same for every Client of this Server: there is one
//! Config Document, which every Client attached to this Server follows, so a
//! Sidekick changing one changes it for each of them, as the user's panel
//! does.
//!
//! Turning `broker.enabled` off is allowed, as the user asked it of their
//! Sidekick: it narrows what every Agent is offered rather than widening it,
//! and one Setting governs everything Suru serves an Agent. It switches the
//! Sidekick's own Tools off with the rest, so the answer says as much, for the
//! Sidekick to tell the user who alone can turn it back on.
//!
//! [`SessionOperations::change_setting`]: crate::server::operations::SessionOperations::change_setting

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, takes_only};
use crate::{
    protocol::{SettingMutation, SettingScope, SettingsSnapshot},
    server::operations::SettingChanged,
    settings::{
        self, SCHEMA, SettingDescriptor, SettingGroup, SettingsMutationError, SidekickAccess,
    },
};

/// What a Sidekick may and may not do with Settings, in the one sentence the
/// Tools' descriptions and the Sidekick's instructions both state it in.
macro_rules! sidekick_settings_rule {
    () => {
        "You may read but not change a Setting that bounds what Agents may do or \
         whose effect no later change can undo — an Approval Posture's, or \
         automatic Worktree Reclaim — and you can neither read nor change the \
         Settings governing Serving and Pairing; the user changes those in Suru's \
         settings panel."
    };
}

/// [`sidekick_settings_rule`], for the Sidekick's instructions.
pub(in crate::broker) const SIDEKICK_SETTINGS_RULE: &str = sidekick_settings_rule!();

pub(super) const LIST_SETTINGS_DESCRIPTION: &str = concat!(
    "List Suru's Settings on this server — what governs how Suru and its \
     Providers behave and how its Clients present it — each as its key and the \
     value in force, so you can find the one to describe or set. Takes one \
     optional argument, \"group\": one of \"appearance\", \"general\", \
     \"transcript\", \"providers\", \"source_control\" or \"experimental\", the \
     tab of Suru's settings panel by that name, to list only its Settings. \
     Answers with JSON of the shape {\"settings\": [{\"key\": \"...\", \"value\": \
     ...}, ...]}, each value as a Config Document spells it — a string, true or \
     false, a number, or an object — whether the user pinned it or it is Suru's \
     built-in default; describe_setting says which, and what else a Setting \
     accepts. ",
    sidekick_settings_rule!(),
    " So the Settings governing Serving and Pairing are never listed. A \
     \"group\" naming none of those groups is refused saying which there are, \
     and an \"origin\" is refused: Settings are this server's alone."
);

pub(super) const DESCRIBE_SETTING_DESCRIPTION: &str = concat!(
    "Describe one of Suru's Settings on this server: what it governs and what it \
     accepts, so you set it correctly. Takes \"key\", the Setting's key as \
     list_settings gives it, such as \"appearance.mode\". Answers with JSON of the \
     shape {\"key\": \"...\", \"label\": \"...\", \"description\": \"...\", \
     \"group\": \"...\", \"scope\": \"...\", \"value\": ..., \"default\": ..., \
     \"pinned\": ..., \"accepts\": \"...\", \"settable\": ...}: \"label\", what \
     Suru's settings panel calls it; \"description\", what choosing between its \
     values means; \"group\", the panel's tab it is on; \"scope\", \"client\" for \
     a Setting governing how a Client presents Suru, which is the same for every \
     Client attached to this server, or \"server\" for one governing this server \
     or its Providers; \"value\", the value in force, and \"default\", Suru's \
     built-in one, each as a Config Document spells it and set_setting takes it; \
     \"pinned\", true where the user's Config Document pins the value rather than \
     leaving the default in force; \"accepts\", what it takes, each string quoted \
     and true, false and numbers bare, as set_setting's \"value\" is typed — an \
     Agent Selection being an object {\"provider\": \"...\", \"model\": \"...\", \
     \"options\": []} naming a Provider and one of its Models as list_providers \
     gives them; and \"settable\", false for a Setting you may read but not \
     change. ",
    sidekick_settings_rule!(),
    " A key naming no Setting is refused, naming the nearest keys there are; a \
     key governing Serving or Pairing is refused too, as is an \"origin\": \
     Settings are this server's alone."
);

pub(super) const SET_SETTING_DESCRIPTION: &str = concat!(
    "Change one of Suru's Settings on this server, as the user does in Suru's \
     settings panel: the user's Config Document is edited in place, leaving the \
     rest of it — its other Settings, comments and formatting — as they wrote it, \
     and the change takes effect at once and reaches every Client attached to this \
     server. There is one Config Document for them all, so a Client Setting is \
     set for every Client of this server alike. Takes \"key\", the Setting's key \
     as list_settings gives it, and \"value\", the value to pin, typed as \
     describe_setting's \"value\" and \"accepts\" spell it: a string such as \
     \"dark\", true or false, a number such as 120, or an object for an Agent \
     Selection. A value is pinned even where it is the built-in default, so it \
     stays if the default changes; leave \"value\" out, or give null, to remove \
     the pin and put the built-in default back in force. Answers with JSON of the \
     shape {\"key\": \"...\", \"value\": ..., \"pinned\": ...}: the value in force \
     now, and whether it is pinned; and \"note\" besides, where the change did \
     something more you should tell the user. A value the Setting does not \
     accept is refused saying what to type instead, and a key naming no Setting \
     naming the nearest keys there are; neither changes anything. ",
    sidekick_settings_rule!(),
    " So setting one of those is refused, its pin left as it is. Setting \
     \"broker.enabled\" to false turns off the Broker, and with it every Tool \
     Suru offers you and every other Agent, these included, until the user turns \
     it back on; the answer's \"note\" says so. Settings are this server's alone: \
     a Remote's are its own user's, and set_setting refuses an \"origin\"."
);

/// What `list_settings` takes: the group to narrow the listing to.
const LIST_TAKES: [&str; 1] = ["group"];

/// What `describe_setting` takes: the Setting to describe.
const DESCRIBE_TAKES: [&str; 1] = ["key"];

/// What `set_setting` takes: the Setting, and the value to pin, if any.
const SET_TAKES: [&str; 2] = ["key", "value"];

/// What a Sidekick that turned the Broker off is told, for the user.
const BROKER_OFF: &str = "The Broker is off now, so Suru offers you and every other Agent none \
     of its Tools, these included, from your next call on; tell the user, who alone can turn \
     `broker.enabled` back on, in Suru's settings panel.";

/// What a Sidekick is told where the Serving listener could not follow the
/// Settings in force — and nothing of why, which would name the address and
/// port of Settings no Sidekick may read.
const SERVING_NOT_ADOPTED: &str = "The change is saved and in force, but Suru could not start \
     Serving as its own Settings ask, which this change did not touch; tell the user, whose \
     settings panel and Log say why.";

/// How much of a refused value a refusal repeats back.
const ECHOED_VALUE_CHARS: usize = 120;

/// How many keys an unknown key's refusal names at most.
const NEAREST_KEYS: usize = 3;

/// The JSON Schema of `list_settings`' arguments.
pub(super) fn list_settings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "group": {
                "type": "string",
                "enum": SettingGroup::ALL.map(SettingGroup::name),
                "description": "The tab of Suru's settings panel by this name: list only its \
                    Settings.",
            },
        },
        "additionalProperties": false,
    })
}

/// The JSON Schema of `describe_setting`'s arguments.
pub(super) fn describe_setting_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "key": key_property() },
        "required": DESCRIBE_TAKES,
        "additionalProperties": false,
    })
}

/// The JSON Schema of `set_setting`'s arguments. `value` names no type, since
/// a Setting may hold a string, a boolean, a number or an object.
pub(super) fn set_setting_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "key": key_property(),
            "value": {
                "description": "The value to pin, as describe_setting's \"accepts\" spells it: \
                    a string, true or false, a number, or an object. Leave it out, or give \
                    null, to remove the pin and put the built-in default back in force.",
            },
        },
        "required": ["key"],
        "additionalProperties": false,
    })
}

fn key_property() -> Value {
    json!({
        "type": "string",
        "description": "A Setting's key, as list_settings gives it, such as \"appearance.mode\".",
    })
}

/// What `list_settings` answers.
#[derive(Debug, Serialize)]
struct SettingListing {
    settings: Vec<ListedSetting>,
}

/// One row of a listing: a Setting's key and the value in force.
#[derive(Debug, Serialize)]
struct ListedSetting {
    key: &'static str,
    value: Value,
}

/// What `describe_setting` answers.
#[derive(Debug, Serialize)]
struct DescribedSetting {
    key: &'static str,
    label: &'static str,
    description: &'static str,
    group: &'static str,
    scope: SettingScope,
    value: Value,
    default: Value,
    pinned: bool,
    /// What the Setting takes, as the schema phrases it for a diagnostic.
    accepts: String,
    /// Whether a Sidekick may change it.
    settable: bool,
}

/// What `set_setting` answers: the Setting as it stands once changed.
#[derive(Debug, Serialize)]
struct ChangedSetting {
    key: &'static str,
    value: Value,
    pinned: bool,
    /// What the change did besides that the Sidekick should tell the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The Settings a Sidekick may read, in the schema's order.
fn readable() -> impl Iterator<Item = &'static SettingDescriptor> {
    SCHEMA
        .iter()
        .filter(|descriptor| descriptor.sidekick != SidekickAccess::Hidden)
}

/// Every Setting a Sidekick may read in `group`, or in every group, by its key
/// and the value `snapshot` holds in force.
fn listing(snapshot: &SettingsSnapshot, group: Option<SettingGroup>) -> SettingListing {
    SettingListing {
        settings: readable()
            .filter(|descriptor| group.is_none_or(|group| descriptor.group == group))
            .map(|descriptor| ListedSetting {
                key: descriptor.key,
                value: descriptor.value(&snapshot.settings),
            })
            .collect(),
    }
}

/// `descriptor` as `describe_setting` describes it while `snapshot` is in
/// force.
fn described(
    descriptor: &'static SettingDescriptor,
    snapshot: &SettingsSnapshot,
) -> DescribedSetting {
    DescribedSetting {
        key: descriptor.key,
        label: descriptor.label,
        description: descriptor.description,
        group: descriptor.group.name(),
        scope: descriptor.scope,
        value: descriptor.value(&snapshot.settings),
        default: descriptor.value(&Default::default()),
        pinned: is_pinned(descriptor, snapshot),
        accepts: descriptor.expected(),
        settable: descriptor.sidekick == SidekickAccess::ReadAndChange,
    }
}

fn is_pinned(descriptor: &SettingDescriptor, snapshot: &SettingsSnapshot) -> bool {
    snapshot
        .pinned
        .iter()
        .any(|pinned| pinned == descriptor.key)
}

/// The Setting a Sidekick may read that `key` names, or a refusal saying why
/// there is none: `key` lies in a namespace reserved for Serving and Pairing,
/// or names a Setting hidden from every Sidekick, or names no Setting, in which
/// case the refusal names the nearest keys a Sidekick may read. `done` says
/// what was not done.
fn named(key: &str, done: &str) -> Result<&'static SettingDescriptor, ToolRefusal> {
    if settings::is_reserved_for_serving_and_pairing(key) {
        let namespace = key.split('.').next().unwrap_or(key);
        return Err(ToolRefusal::new(format!(
            "The Settings keyed under `{namespace}` govern Serving and Pairing, which no Sidekick \
             reads or changes, so {done}; the user may change them in Suru's settings panel."
        )));
    }
    match SCHEMA.iter().find(|descriptor| descriptor.key == key) {
        Some(descriptor) if descriptor.sidekick == SidekickAccess::Hidden => {
            Err(ToolRefusal::new(format!(
                "`{key}` is a Setting no Sidekick reads or changes, so {done}; the user may \
                 change it in Suru's settings panel."
            )))
        }
        Some(descriptor) => Ok(descriptor),
        None => Err(ToolRefusal::new(match nearest_keys(key).as_slice() {
            [] => format!(
                "Suru has no Setting `{key}`, so {done}; list_settings lists every key, and takes \
                 a `group` to narrow them."
            ),
            nearest => format!(
                "Suru has no Setting `{key}`, so {done}; did you mean {}? list_settings lists \
                 every key.",
                alternatives(nearest)
            ),
        })),
    }
}

/// The keys a Sidekick may read nearest `key`, nearest first: one whose last
/// name is `key`, then one containing it, then one a few edits from it,
/// either whole or by its last name, whatever their case.
fn nearest_keys(key: &str) -> Vec<&'static str> {
    let wanted = key.trim().to_lowercase();
    let reach = 2.max(wanted.chars().count() / 5);
    let mut near = readable()
        .filter_map(|descriptor| {
            let candidate = descriptor.key.to_lowercase();
            let last = candidate.rsplit('.').next().unwrap_or(&candidate);
            let distance = if last == wanted {
                0
            } else if wanted.chars().count() >= 4 && candidate.contains(&wanted) {
                1
            } else {
                edits(&wanted, &candidate).min(edits(&wanted, last))
            };
            (distance <= reach).then_some((distance, descriptor.key))
        })
        .collect::<Vec<_>>();
    near.sort_by_key(|(distance, _)| *distance);
    near.into_iter()
        .take(NEAREST_KEYS)
        .map(|(_, key)| key)
        .collect()
}

/// How many single-character insertions, deletions and substitutions turn
/// `from` into `to`.
fn edits(from: &str, to: &str) -> usize {
    let to = to.chars().collect::<Vec<_>>();
    let mut row = (0..=to.len()).collect::<Vec<_>>();
    for (index, from) in from.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = index + 1;
        for (column, to) in to.iter().enumerate() {
            let above = row[column + 1];
            row[column + 1] = (above + 1)
                .min(row[column] + 1)
                .min(diagonal + usize::from(from != *to));
            diagonal = above;
        }
    }
    row[to.len()]
}

/// Keys as a refusal offers them: each quoted, the last after "or".
fn alternatives(keys: &[&str]) -> String {
    let quoted = keys
        .iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// The pin `set_setting` asks of `descriptor`: `value` pinned, or the pin
/// removed where it gave none. A Setting a Sidekick may read but not change,
/// and a value the Setting does not accept, are refused saying why.
fn pin(
    descriptor: &'static SettingDescriptor,
    value: Option<&Value>,
) -> Result<SettingMutation, ToolRefusal> {
    let key = descriptor.key;
    match descriptor.sidekick {
        SidekickAccess::ReadAndChange => {}
        SidekickAccess::ReadOnly { why } => {
            return Err(ToolRefusal::new(format!(
                "`{key}` is a Setting a Sidekick may read but not change, since {why}, so it was \
                 left as it is; tell the user, who may change it in Suru's settings panel."
            )));
        }
        SidekickAccess::Hidden => {
            return Err(ToolRefusal::new(format!(
                "`{key}` is a Setting no Sidekick reads or changes, so nothing was changed; the \
                 user may change it in Suru's settings panel."
            )));
        }
    }
    match value {
        None | Some(Value::Null) => Ok(descriptor.reset.clone()),
        Some(value) => descriptor.pin(value).ok_or_else(|| {
            ToolRefusal::new(format!(
                "`{key}` takes {}, not {}; nothing was changed.",
                descriptor.expected(),
                echoed(value)
            ))
        }),
    }
}

/// What `set_setting` answers once the operation changing `descriptor` has
/// said how the change went, worded here from what it says rather than passed
/// on in other words: so neither the detail of a document that could not be
/// edited nor why the Serving listener could not follow reaches a Sidekick.
fn answered(
    descriptor: &'static SettingDescriptor,
    outcome: Result<SettingChanged, SettingsMutationError>,
) -> Result<Value, ToolRefusal> {
    let key = descriptor.key;
    let SettingChanged { snapshot, serving } = match outcome {
        Ok(changed) => changed,
        Err(SettingsMutationError::NoConfigRoot) => {
            return Err(ToolRefusal::new(format!(
                "Nothing was changed: Suru has no config root, so there is nowhere to pin `{key}`; \
                 tell the user."
            )));
        }
        Err(SettingsMutationError::NotEditable { path, .. }) => {
            return Err(ToolRefusal::new(format!(
                "Nothing was changed: the user's Config Document at `{}` cannot be edited in \
                 place — it does not parse, or holds something other than an object where `{key}` \
                 belongs — and Suru never rewrites one; tell the user, who can mend it by hand.",
                path.display()
            )));
        }
        Err(SettingsMutationError::Io { path, .. }) => {
            return Err(ToolRefusal::new(format!(
                "Nothing was changed: the user's Config Document at `{}` could not be read or \
                 written; tell the user, whose Log says why.",
                path.display()
            )));
        }
    };
    let notes = [
        (!snapshot.settings.broker.enabled).then_some(BROKER_OFF),
        serving.is_some().then_some(SERVING_NOT_ADOPTED),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    Ok(serde_json::to_value(ChangedSetting {
        key,
        value: descriptor.value(&snapshot.settings),
        pinned: is_pinned(descriptor, &snapshot),
        note: (!notes.is_empty()).then(|| notes.join(" ")),
    })
    .expect("a changed Setting always serializes"))
}

/// Refuses a call of `tool` naming an `origin`: Settings stay with the
/// Sidekick's own Server, so no Setting is reached on a Remote.
fn takes_no_origin(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<(), ToolRefusal> {
    if arguments.contains_key("origin") {
        return Err(ToolRefusal::new(format!(
            "{} takes no `origin`: Settings are this server's alone, and a Remote's are its own \
             user's, so nothing was done.",
            tool.name()
        )));
    }
    Ok(())
}

/// A refused value as a refusal repeats it: as the JSON it was given in, so
/// a string reads quoted, cut short where it runs long.
fn echoed(value: &Value) -> String {
    let spelled = serde_json::to_string(value).expect("a JSON value always serializes");
    if spelled.chars().count() <= ECHOED_VALUE_CHARS {
        return spelled;
    }
    let cut = spelled.chars().take(ECHOED_VALUE_CHARS).collect::<String>();
    format!("{cut}…")
}

/// The key a call names, read for `tool`.
fn key_argument(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<String, ToolRefusal> {
    let name = tool.name();
    match arguments.get("key") {
        Some(Value::String(key)) if !key.trim().is_empty() => Ok(key.trim().to_owned()),
        None | Some(Value::Null) => Err(ToolRefusal::new(format!(
            "{name} needs `key`, a Setting's key as list_settings gives it, such as \
             `appearance.mode`."
        ))),
        Some(_) => Err(ToolRefusal::new(format!(
            "{name}'s `key` must be a Setting's key as list_settings gives it, a string such as \
             `appearance.mode`."
        ))),
    }
}

/// The group a `list_settings` call narrows its listing to, if any.
fn group_argument(arguments: &Map<String, Value>) -> Result<Option<SettingGroup>, ToolRefusal> {
    let groups = || {
        SettingGroup::ALL
            .map(|group| format!("`{}`", group.name()))
            .join(", ")
    };
    match arguments.get("group") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(named)) => {
            SettingGroup::named(named.trim()).map(Some).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "list_settings' `group` must be one of {}; `{named}` is none of them.",
                    groups()
                ))
            })
        }
        Some(_) => Err(ToolRefusal::new(format!(
            "list_settings' `group` must be one of {}.",
            groups()
        ))),
    }
}

impl BrokerTools {
    /// Answers `list_settings`: every Setting a Sidekick may read, or those of
    /// the group the call names, by key and the value in force.
    pub(super) fn list_settings(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_no_origin(BrokerTool::ListSettings, &call.arguments)?;
        takes_only(BrokerTool::ListSettings, &call.arguments, &LIST_TAKES)?;
        let group = group_argument(&call.arguments)?;
        let listing = listing(&self.settings.borrow(), group);
        Ok(serde_json::to_value(listing).expect("a listing of Settings always serializes"))
    }

    /// Answers `describe_setting`: the Setting the call names, described.
    pub(super) fn describe_setting(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_no_origin(BrokerTool::DescribeSetting, &call.arguments)?;
        takes_only(
            BrokerTool::DescribeSetting,
            &call.arguments,
            &DESCRIBE_TAKES,
        )?;
        let key = key_argument(BrokerTool::DescribeSetting, &call.arguments)?;
        let descriptor = named(&key, "nothing was described")?;
        let described = described(descriptor, &self.settings.borrow());
        Ok(serde_json::to_value(described).expect("a described Setting always serializes"))
    }

    /// Answers `set_setting`: pins the value the call gives the Setting it
    /// names, or removes its pin, through the operation the settings panel's
    /// change goes through, and says what is in force now.
    pub(super) async fn set_setting(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_no_origin(BrokerTool::SetSetting, &call.arguments)?;
        takes_only(BrokerTool::SetSetting, &call.arguments, &SET_TAKES)?;
        let key = key_argument(BrokerTool::SetSetting, &call.arguments)?;
        let descriptor = named(&key, "nothing was changed")?;
        let mutation = pin(descriptor, call.arguments.get("value"))?;
        answered(descriptor, self.operations.change_setting(mutation).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::{AppearanceMode, EffectiveSettings},
        server::operations::ServingNotAdopted,
    };

    fn arguments(arguments: Value) -> Map<String, Value> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        arguments
    }

    /// A snapshot holding `settings`, with `pinned` pinned.
    fn snapshot(settings: EffectiveSettings, pinned: &[&str]) -> SettingsSnapshot {
        SettingsSnapshot {
            settings,
            pinned: pinned.iter().map(|key| (*key).to_owned()).collect(),
            diagnostics: Vec::new(),
        }
    }

    fn listed_keys(listing: &SettingListing) -> Vec<&'static str> {
        listing.settings.iter().map(|row| row.key).collect()
    }

    /// Every Setting in the schema, as it reaches each Tool: one a Sidekick may
    /// read is listed and described, by its own words, and one hidden from it
    /// is neither listed nor described, nor its value told — so a Setting added
    /// to the schema is held to what it declares without a word more.
    #[test]
    fn every_setting_but_those_hidden_is_listed_and_described_by_the_schema() {
        let in_force = snapshot(EffectiveSettings::default(), &[]);
        let listing = listing(&in_force, None);
        let listed = listed_keys(&listing);
        for descriptor in SCHEMA {
            let description = named(descriptor.key, "nothing was described");
            if descriptor.sidekick == SidekickAccess::Hidden {
                assert!(!listed.contains(&descriptor.key), "{}", descriptor.key);
                let Err(refusal) = description else {
                    panic!("{} is hidden, so it is refused", descriptor.key);
                };
                assert!(
                    refusal.to_string().contains("no Sidekick reads or changes"),
                    "{}: {refusal}",
                    descriptor.key
                );
                continue;
            }
            assert!(listed.contains(&descriptor.key), "{}", descriptor.key);
            let row = &listing.settings[listed
                .iter()
                .position(|key| *key == descriptor.key)
                .expect("listed")];
            assert_eq!(row.value, descriptor.value(&in_force.settings));
            let described = described(description.expect("described"), &in_force);
            assert_eq!(
                (
                    described.label,
                    described.description,
                    described.group,
                    described.scope,
                    &described.value,
                    &described.default,
                    described.pinned,
                    described.accepts.as_str(),
                    described.settable,
                ),
                (
                    descriptor.label,
                    descriptor.description,
                    descriptor.group.name(),
                    descriptor.scope,
                    &descriptor.value(&EffectiveSettings::default()),
                    &descriptor.value(&EffectiveSettings::default()),
                    false,
                    descriptor.expected().as_str(),
                    descriptor.sidekick == SidekickAccess::ReadAndChange,
                ),
                "{} is described in the schema's own words",
                descriptor.key
            );
        }
        assert!(
            serde_json::to_string(&listing).is_ok_and(|listing| !listing.contains("serving")),
            "nothing of Serving is told"
        );
    }

    /// Every Setting a Sidekick may change is pinned by its default and every
    /// value it names, and has its pin removed when no value is given; one it
    /// may only read is neither pinned nor unpinned, the refusal saying why and
    /// that the user may change it.
    #[test]
    fn every_setting_a_sidekick_may_change_is_pinned_by_what_it_accepts_and_no_other_is() {
        for descriptor in SCHEMA {
            let Ok(found) = named(descriptor.key, "nothing was changed") else {
                assert_eq!(descriptor.sidekick, SidekickAccess::Hidden);
                continue;
            };
            let default = descriptor.value(&EffectiveSettings::default());
            match descriptor.sidekick {
                SidekickAccess::ReadAndChange => {
                    assert_eq!(pin(found, None), Ok(descriptor.reset.clone()));
                    assert_eq!(pin(found, Some(&Value::Null)), Ok(descriptor.reset.clone()));
                    assert!(pin(found, Some(&default)).is_ok(), "{}", descriptor.key);
                    for choice in descriptor.values.named() {
                        let value = serde_json::to_value(choice.mutation())
                            .expect("a pin serializes")["value"]
                            .clone();
                        assert_eq!(
                            pin(found, Some(&value)),
                            Ok(choice.mutation()),
                            "{} pins {:?}",
                            descriptor.key,
                            choice.value
                        );
                    }
                }
                SidekickAccess::ReadOnly { why } => {
                    for value in [None, Some(&Value::Null), Some(&default)] {
                        assert_eq!(
                            pin(found, value),
                            Err(ToolRefusal::new(format!(
                                "`{}` is a Setting a Sidekick may read but not change, since \
                                 {why}, so it was left as it is; tell the user, who may change \
                                 it in Suru's settings panel.",
                                descriptor.key
                            ))),
                        );
                    }
                }
                SidekickAccess::Hidden => unreachable!("a hidden Setting is never found"),
            }
        }
    }

    /// A path standing for the user's Config Document, rooted for the platform.
    fn document_path() -> std::path::PathBuf {
        std::path::PathBuf::from(if cfg!(windows) {
            r"C:\Users\ada\.config\suru\suru.jsonc"
        } else {
            "/home/ada/.config/suru/suru.jsonc"
        })
    }

    /// Whatever the operation says went wrong, a Sidekick is told in words
    /// worded here: never why the Serving listener could not follow, which
    /// names the address and port of Settings hidden from it, nor the detail
    /// a document or the filesystem gave, which no Sidekick needs.
    #[test]
    fn a_set_tells_nothing_of_serving_nor_any_detail_another_part_of_suru_gave() {
        const ADDRESS: &str = "203.0.113.7";
        const PORT: &str = "9443";
        let leaked = || anyhow::anyhow!("bind Serving listener to {ADDRESS}:{PORT}: in use");
        let mode = named("appearance.mode", "nothing was changed").expect("a Setting");
        let in_force = snapshot(
            EffectiveSettings {
                appearance: crate::protocol::AppearanceSettings {
                    mode: AppearanceMode::Dark,
                    ..Default::default()
                },
                ..Default::default()
            },
            &["appearance.mode"],
        );
        let answer = answered(
            mode,
            Ok(SettingChanged {
                snapshot: in_force.clone(),
                serving: Some(ServingNotAdopted::because(leaked())),
            }),
        )
        .expect("the change landed, so it is answered");
        assert_eq!(
            answer,
            json!({
                "key": "appearance.mode",
                "value": "dark",
                "pinned": true,
                "note": SERVING_NOT_ADOPTED,
            }),
            "the Sidekick is told the change is in force and Serving could not follow"
        );

        let broker_off = snapshot(
            EffectiveSettings {
                broker: crate::protocol::BrokerSettings {
                    enabled: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            &["broker.enabled"],
        );
        let enabled = named("broker.enabled", "nothing was changed").expect("a Setting");
        assert_eq!(
            answered(
                enabled,
                Ok(SettingChanged {
                    snapshot: broker_off,
                    serving: Some(ServingNotAdopted::because(leaked())),
                })
            )
            .expect("answered")["note"],
            json!(format!("{BROKER_OFF} {SERVING_NOT_ADOPTED}")),
            "and of everything else the change did"
        );

        for refused in [
            SettingsMutationError::NoConfigRoot,
            SettingsMutationError::NotEditable {
                path: document_path(),
                reason: format!("is not valid JSONC: unexpected {PORT} at {ADDRESS}"),
            },
            SettingsMutationError::Io {
                path: document_path(),
                message: format!("could not be written: {ADDRESS}:{PORT}"),
            },
        ] {
            let refusal = answered(mode, Err(refused))
                .expect_err("nothing landed, so it is refused")
                .to_string();
            assert!(
                refusal.starts_with("Nothing was changed: ")
                    && !refusal.contains(ADDRESS)
                    && !refusal.contains(PORT),
                "{refusal}"
            );
        }
        let not_editable = answered(
            mode,
            Err(SettingsMutationError::NotEditable {
                path: document_path(),
                reason: "does not hold an object at its top level".to_owned(),
            }),
        )
        .expect_err("refused")
        .to_string();
        assert!(
            not_editable.contains(&document_path().display().to_string())
                && not_editable.contains("tell the user, who can mend it by hand"),
            "the user is told where the document they must mend is: {not_editable}"
        );
    }

    /// Settings stay with the Sidekick's own Server, so each Tool refuses an
    /// `origin` saying as much, whatever Remote it names.
    #[test]
    fn every_settings_tool_refuses_an_origin() {
        for tool in [
            BrokerTool::ListSettings,
            BrokerTool::DescribeSetting,
            BrokerTool::SetSetting,
        ] {
            assert_eq!(
                takes_no_origin(tool, &arguments(json!({ "origin": "workstation" }))),
                Err(ToolRefusal::new(format!(
                    "{} takes no `origin`: Settings are this server's alone, and a Remote's are \
                     its own user's, so nothing was done.",
                    tool.name()
                )))
            );
            assert_eq!(
                takes_no_origin(tool, &arguments(json!({ "key": "appearance.mode" }))),
                Ok(())
            );
        }
    }

    /// The one sentence stating what a Sidekick may do with Settings stands in
    /// each Tool's description.
    #[test]
    fn every_settings_tool_states_the_rule_of_what_a_sidekick_may_do() {
        for description in [
            LIST_SETTINGS_DESCRIPTION,
            DESCRIBE_SETTING_DESCRIPTION,
            SET_SETTING_DESCRIPTION,
        ] {
            assert!(
                description.contains(SIDEKICK_SETTINGS_RULE),
                "{description}"
            );
        }
        assert!(
            SIDEKICK_SETTINGS_RULE.contains("automatic Worktree Reclaim")
                && SIDEKICK_SETTINGS_RULE.contains("Approval Posture")
                && SIDEKICK_SETTINGS_RULE.contains("Serving and Pairing")
                && SIDEKICK_SETTINGS_RULE.contains("settings panel")
        );
    }

    #[test]
    fn a_value_a_setting_does_not_accept_is_refused_saying_what_to_type_instead() {
        let mode = named("appearance.mode", "nothing was changed").expect("a Setting");
        assert_eq!(
            pin(mode, Some(&json!("sepia"))),
            Err(ToolRefusal::new(
                "`appearance.mode` takes one of \"system\", \"dark\", or \"light\", not \
                 \"sepia\"; nothing was changed."
            ))
        );
        let icons = named("appearance.showIcons", "nothing was changed").expect("a Setting");
        assert_eq!(
            pin(icons, Some(&json!("true"))),
            Err(ToolRefusal::new(
                "`appearance.showIcons` takes one of false or true, not \"true\"; nothing was \
                 changed."
            )),
            "a string is told apart from the boolean it spells"
        );
        let width = named("sidebar.initialWidth", "nothing was changed").expect("a Setting");
        let refusal = pin(width, Some(&json!("x".repeat(500)))).expect_err("refused");
        assert!(
            refusal.to_string().contains("an integer of at least 24")
                && refusal.to_string().contains('…')
                && refusal.to_string().len() < 300,
            "a long value is cut short: {refusal}"
        );
        assert_eq!(
            pin(mode, Some(&json!("dark"))),
            Ok(SettingMutation::AppearanceMode {
                value: Some(AppearanceMode::Dark)
            })
        );
    }

    #[test]
    fn an_unknown_key_is_refused_naming_the_nearest_keys_a_sidekick_may_read() {
        let refusal = |key| match named(key, "nothing was described") {
            Ok(found) => panic!("{key} names no Setting, yet {} was found", found.key),
            Err(refusal) => refusal.to_string(),
        };
        assert_eq!(
            refusal("apperance.mode"),
            "Suru has no Setting `apperance.mode`, so nothing was described; did you mean \
             `appearance.mode`? list_settings lists every key."
        );
        assert!(
            refusal("theme").contains("did you mean `appearance.theme`?"),
            "a Setting's last name finds it"
        );
        assert!(
            refusal("showicons").contains("`appearance.showIcons`"),
            "whatever its case"
        );
        let width = refusal("width");
        for key in [
            "session.contentWidth",
            "sidebar.initialWidth",
            "aside.initialWidth",
        ] {
            assert!(width.contains(key), "{width}");
        }
        assert_eq!(
            refusal("zzz"),
            "Suru has no Setting `zzz`, so nothing was described; list_settings lists every \
             key, and takes a `group` to narrow them."
        );
        let near_serving = refusal("servng.enabled");
        assert!(
            !near_serving.contains("serving."),
            "a hidden Setting is never offered: {near_serving}"
        );
        assert!(
            refusal("pairing.anything").contains("govern Serving and Pairing"),
            "a key under a withheld namespace is withheld, whether or not a Setting has it"
        );
    }

    #[test]
    fn a_listing_by_group_lists_that_groups_settings_alone_in_the_schemas_order() {
        let in_force = snapshot(
            EffectiveSettings {
                appearance: crate::protocol::AppearanceSettings {
                    mode: AppearanceMode::Dark,
                    ..Default::default()
                },
                ..Default::default()
            },
            &["appearance.mode"],
        );
        let appearance = listing(&in_force, Some(SettingGroup::Appearance));
        assert_eq!(
            serde_json::to_value(&appearance).expect("serializes"),
            json!({
                "settings": [
                    { "key": "appearance.theme", "value": "system" },
                    { "key": "appearance.mode", "value": "dark" },
                    { "key": "appearance.landingPage", "value": "Minimal" },
                    { "key": "appearance.showIcons", "value": false },
                ],
            })
        );
        assert_eq!(
            listed_keys(&listing(&in_force, Some(SettingGroup::Experimental))),
            [
                "broker.enabled",
                "broker.maxDepth",
                "broker.maxConcurrentSubagents"
            ],
            "the Serving Settings beside them are left out"
        );
    }

    #[test]
    fn arguments_are_read_as_their_schemas_give_them() {
        assert_eq!(
            group_argument(&arguments(json!({ "group": "source_control" }))),
            Ok(Some(SettingGroup::SourceControl))
        );
        assert_eq!(group_argument(&arguments(json!({}))), Ok(None));
        let refusal = group_argument(&arguments(json!({ "group": "Serving" })))
            .expect_err("refused")
            .to_string();
        assert!(
            refusal.contains("`Serving` is none of them") && refusal.contains("`experimental`"),
            "{refusal}"
        );
        assert!(group_argument(&arguments(json!({ "group": 7 }))).is_err());
        assert_eq!(
            key_argument(
                BrokerTool::DescribeSetting,
                &arguments(json!({ "key": " appearance.mode " }))
            ),
            Ok("appearance.mode".to_owned())
        );
        for refused in [json!({}), json!({ "key": "" }), json!({ "key": ["a"] })] {
            let refusal = key_argument(BrokerTool::SetSetting, &arguments(refused.clone()))
                .expect_err("refused");
            assert!(
                refusal.to_string().starts_with("set_setting"),
                "{refused}: {refusal}"
            );
        }
    }

    /// The descriptions and schemas name every group a listing narrows to and
    /// take exactly what each Tool reads.
    #[test]
    fn the_schemas_take_what_the_tools_read_and_name_every_group() {
        for group in SettingGroup::ALL {
            assert!(
                LIST_SETTINGS_DESCRIPTION.contains(&format!("\"{}\"", group.name())),
                "{}",
                group.name()
            );
        }
        let properties = |schema: Value| {
            let mut properties = schema["properties"]
                .as_object()
                .expect("the schema names its properties")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            properties.sort_unstable();
            properties
        };
        assert_eq!(properties(list_settings_schema()), LIST_TAKES);
        assert_eq!(properties(describe_setting_schema()), DESCRIBE_TAKES);
        assert_eq!(properties(set_setting_schema()), SET_TAKES);
        for schema in [
            list_settings_schema(),
            describe_setting_schema(),
            set_setting_schema(),
        ] {
            assert!(
                schema["properties"].get("origin").is_none(),
                "Settings are this server's alone"
            );
        }
    }
}
