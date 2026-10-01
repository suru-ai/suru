//! Presents the input of a Tool Call the way a reader should see it, which is
//! why this lives beside the Providers rather than inside one: every Provider
//! reports a Tool's arguments as JSON, and every Tool Call's row reads them on
//! one line.
//!
//! The arguments render at record time into one display string:
//!
//! - An object is its `key=value` pairs separated by single spaces, ordered by
//!   key so the same arguments read alike whichever Provider sent them. A
//!   string value stands bare, its line breaks and tabs escaped as `\n`, `\r`
//!   and `\t` so the pair stays on its line; every other value stands as
//!   compact JSON.
//! - An object with no arguments, and a `null`, render as nothing at all.
//! - Any other value renders as compact JSON.
//!
//! The string is display text, never parsed back: the Provider keeps the
//! arguments themselves. Capping it is orchestration's, which stores it.
//!
//! A Tool Call's input is a [`ToolCallInput`], which only the Tool's
//! arguments and the Tool they were given to make: a call of one of the
//! Broker's own Tools is rendered from what the Broker lets a Transcript keep
//! of its arguments, so no Provider records what the Broker withholds — a
//! Sidekick's Answers among them, any of which may be secret.

use serde_json::Value;

/// A Tool Call's input as its row records it: the Tool's arguments rendered
/// on one line by the rule this module states, from what the Broker lets a
/// Transcript keep where the Tool is the Broker's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallInput(String);

impl ToolCallInput {
    /// The input a call of the Tool `name` — hosted by the MCP server
    /// `server`, where it has one, and named as that server names it — with
    /// `arguments` records.
    pub fn of(server: Option<&str>, name: &str, arguments: &Value) -> Self {
        Self(match server {
            Some(crate::broker::BROKER_SERVER_NAME) => {
                present_tool_input(&crate::broker::recorded_tool_arguments(name, arguments))
            }
            _ => present_tool_input(arguments),
        })
    }

    /// The input with `redact` applied to its rendering, for a Provider that
    /// keeps secrets of its own out of what it records — which a rendering
    /// may spell anew, in a key, a number, or a join between arguments.
    pub(super) fn redacted(self, redact: impl FnOnce(&str) -> String) -> Self {
        Self(redact(&self.0))
    }

    /// The rendering, as orchestration stores it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// An input already rendered as `text`, for a test comparing what a
    /// Provider recorded against the rendering it expects.
    #[cfg(test)]
    pub(super) fn rendered(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// The display string of a Tool's arguments, by the rule this module states.
fn present_tool_input(input: &Value) -> String {
    match input {
        Value::Null => String::new(),
        Value::Object(arguments) => arguments
            .iter()
            .map(|(key, value)| format!("{key}={}", present_argument(value)))
            .collect::<Vec<_>>()
            .join(" "),
        value => compact_json(value),
    }
}

fn present_argument(value: &Value) -> String {
    match value {
        Value::String(text) => text
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t"),
        value => compact_json(value),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).expect("a JSON value always serializes")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ToolCallInput, present_tool_input};

    /// A call of the Broker's `answer_questionnaire` records which Session and
    /// Questionnaire it named and how many Answers it gave, never what they
    /// said; a Tool of the same name another server hosts, and any other of
    /// the Broker's Tools, records its arguments as they were.
    #[test]
    fn the_brokers_answers_are_withheld_from_the_input_a_tool_call_records() {
        let arguments = json!({
            "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "questionnaire_id": "0198b27e-4b02-7c4c-a83b-a83a4787453f",
            "answers": [{ "text": "tok-1" }],
        });
        assert_eq!(
            ToolCallInput::of(Some("suru"), "answer_questionnaire", &arguments).as_str(),
            "answers=1 Answer withheld questionnaire_id=0198b27e-4b02-7c4c-a83b-a83a4787453f \
             session_id=0198b27e-3a01-7c4c-a83b-a83a4787453f"
        );
        for server in [Some("elsewhere"), None] {
            assert_eq!(
                ToolCallInput::of(server, "answer_questionnaire", &arguments).as_str(),
                present_tool_input(&arguments)
            );
        }
        assert_eq!(
            ToolCallInput::of(Some("suru"), "list_sessions", &json!({ "title": "tok-1" })).as_str(),
            "title=tok-1"
        );
    }

    #[test]
    fn an_objects_arguments_read_as_key_value_pairs_on_one_line() {
        assert_eq!(
            present_tool_input(&json!({"file_path": "src/lib.rs", "limit": 20})),
            "file_path=src/lib.rs limit=20"
        );
    }

    #[test]
    fn a_string_argument_stands_bare_with_its_breaks_escaped() {
        assert_eq!(
            present_tool_input(&json!({"body": "first\nsecond\tthird\r"})),
            "body=first\\nsecond\\tthird\\r"
        );
    }

    #[test]
    fn a_structured_argument_stands_as_compact_json() {
        assert_eq!(
            present_tool_input(&json!({"draft": false, "labels": ["bug", "ui"], "meta": {"a": 1}})),
            r#"draft=false labels=["bug","ui"] meta={"a":1}"#
        );
    }

    #[test]
    fn no_arguments_render_as_nothing() {
        assert_eq!(present_tool_input(&json!({})), "");
        assert_eq!(present_tool_input(&json!(null)), "");
    }

    #[test]
    fn arguments_that_are_no_object_render_as_compact_json() {
        assert_eq!(present_tool_input(&json!(["a", 1])), r#"["a",1]"#);
        assert_eq!(present_tool_input(&json!("bare")), r#""bare""#);
        assert_eq!(present_tool_input(&json!(7)), "7");
    }
}
