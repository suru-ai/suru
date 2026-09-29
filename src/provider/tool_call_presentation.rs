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

use serde_json::Value;

/// The display string of a Tool's arguments, by the rule this module states.
pub(super) fn present_tool_input(input: &Value) -> String {
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

    use super::present_tool_input;

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
