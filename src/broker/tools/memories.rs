//! `store_memory`, `search_memory`, `recall_memory`, `update_memory` and
//! `forget_memory`: the Tools through which a Sidekick keeps Memories — what
//! it chose to keep past its own Session for every Sidekick after it, on any
//! Provider (see [`crate::memories`]).
//!
//! Memories stay with the Sidekick's own Server, as its Settings do (ADR
//! 0044), so none of the five takes an `origin`. Each reads its arguments
//! here and holds them to a Memory's bounds, refusing in words the Sidekick
//! can relay — what the bound is and how far past it the call went, never
//! what it was given — and reaches the Memories through the one operations
//! interface every Sidekick Tool acts through. A search answers rows carrying
//! a snippet and never a whole body, which only `recall_memory` reads.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, session_listing, takes_only};
use crate::{
    memories::{
        self, FoundMemory, MAX_BODY_CHARS, MAX_QUERY_WORDS, MAX_TAG_CHARS, MAX_TAGS,
        MAX_TITLE_CHARS, MatchQuery, Memory, MemoryChange, MemoryId, MemorySearch, NewMemory,
        QueryRefusal,
    },
    protocol::SessionTimestamp,
};

pub(super) const STORE_MEMORY_DESCRIPTION: &str = "\
Keep something worth knowing past your own Session as a Memory: what the user \
prefers, how their work is arranged, what a later Sidekick should not have to \
find out again. A Memory is this server's rather than this Session's: every \
Sidekick on this server, on any Provider, can find and recall it from now on, \
and it stands until a Sidekick forgets it, whatever becomes of the Session \
that stored it. Takes \"title\", one short line naming what the Memory is \
about, at most 100 characters, which every Sidekick begun later is shown \
before it recalls anything; \"body\", what to keep, at most 10000 characters; \
and \"tags\", optionally, a list of at most 10 words or short phrases to find \
it by, each at most 40 characters. A title is kept on one line, and a tag in \
lower case without a leading #, once. Answers with JSON of the shape \
{\"memory_id\": n, \"title\": \"...\", \"tags\": [...], \"stored_at\": \"...\", \
\"changed_at\": \"...\"}: the memory_id recall_memory, update_memory and \
forget_memory take, the title and tags as kept, and the RFC 3339 moments it \
was stored and last changed. A title or body that is missing, empty or too \
long, or tags past their limits, are refused saying so, and nothing is \
stored. Memories are this server's alone, so an \"origin\" is refused.";

pub(super) const SEARCH_MEMORY_DESCRIPTION: &str = "\
Find the Memories Sidekicks on this server have kept, by their words, tags \
and dates, as compact rows that never carry a whole body; recall_memory reads \
one whole. Every argument is optional: \"query\", words to find in a \
Memory's title, body or tags; \"tags\", a list of tags a Memory must carry \
every one of; \"changed_after\" and \"changed_before\", each an RFC 3339 \
moment such as 2026-10-01T09:30:00Z or a day such as 2026-10-01, which stands \
for its first moment in UTC, to find only Memories last changed at or after \
the one and before the other; and \"limit\", how many rows at most: 10 unless \
given, and at least 1. A query's words must each be found, whatever their case \
or accents and however an English word ends, so \"reviewing\" finds \
\"reviews\", unless OR joins them, and words inside double quotes must stand \
together in that order. Only OR is read as an operator: every other mark only \
separates words. Words are matched whole, so text written without spaces \
between its words, such as Chinese or Japanese, is found only by the whole \
run. With a query, the best matches come first; without one, every Memory the \
other arguments allow, most recently changed first. Answers with JSON of the \
shape {\"memories\": [row, ...], \"omitted\": n}, where \"omitted\" counts the \
Memories that matched but were left out past the limit. Each row has \
\"memory_id\", \"title\", \"tags\", \"stored_at\" and \"changed_at\", and \
\"snippet\": at most 240 characters of its body, around the words that \
matched where the body holds them and its opening otherwise. A query holding \
no word at all, or more than 32 words, is refused saying so. Memories are \
this server's alone, so an \"origin\" is refused.";

pub(super) const RECALL_MEMORY_DESCRIPTION: &str = "\
Recall one Memory whole: its body as kept, with its title, tags and moments. \
Takes \"memory_id\", as store_memory or search_memory answered with it, or as \
your instructions name it. Answers with JSON of the shape {\"memory_id\": n, \
\"title\": \"...\", \"body\": \"...\", \"tags\": [...], \"stored_at\": \
\"...\", \"changed_at\": \"...\"}, each moment in RFC 3339. A memory_id naming \
no Memory — never stored on this server, or forgotten since — is refused \
saying so. Memories are this server's alone, so an \"origin\" is refused.";

pub(super) const UPDATE_MEMORY_DESCRIPTION: &str = "\
Change a Memory so that what is kept stays true: its title, its body, its \
tags, or any of them, the rest left as they were, and the moment it was last \
changed made now. Takes \"memory_id\", and at least one of \"title\", \"body\" \
and \"tags\", each held to the limits store_memory states; \"tags\" replaces \
every tag the Memory carried, and an empty list removes them all. Answers \
with JSON of the shape {\"memory_id\": n, \"title\": \"...\", \"tags\": [...], \
\"stored_at\": \"...\", \"changed_at\": \"...\"}, the Memory as it stands now \
but for its body. A memory_id naming no Memory, a call naming nothing to \
change, or a value past its limit is refused saying so, and nothing is \
changed. Memories are this server's alone, so an \"origin\" is refused.";

pub(super) const FORGET_MEMORY_DESCRIPTION: &str = "\
Forget a Memory that is no longer true or worth keeping: it is gone for every \
Sidekick on this server, for good, and its memory_id is never given to \
another. Takes \"memory_id\". Answers with JSON of the shape {\"memory_id\": \
n, \"forgotten\": true}. A memory_id naming no Memory — never stored on this \
server, or already forgotten — is refused saying so. Memories are this \
server's alone, so an \"origin\" is refused.";

/// How many rows a search answers with unless asked for another number.
const DEFAULT_LIMIT: usize = 10;

/// What `store_memory` takes.
const STORE_TAKES: [&str; 3] = ["title", "body", "tags"];

/// What `search_memory` takes.
const SEARCH_TAKES: [&str; 5] = ["query", "tags", "changed_after", "changed_before", "limit"];

/// What `recall_memory` and `forget_memory` take: the one Memory.
const ONE_MEMORY_TAKES: [&str; 1] = ["memory_id"];

/// What `update_memory` takes: the Memory, and what to change of it.
const UPDATE_TAKES: [&str; 4] = ["memory_id", "title", "body", "tags"];

/// What `search_memory` is refused for a query holding no word.
const NOTHING_TO_SEARCH: &str = "search_memory's `query` holds no word to search for, only marks \
     it ignores; give it words, or leave it out to list Memories by when they last changed.";

/// What `update_memory` is refused for a call naming nothing to change.
const NOTHING_TO_CHANGE: &str = "update_memory needs at least one of `title`, `body` and `tags` \
     to change; nothing was changed.";

fn memory_id_property() -> Value {
    json!({
        "type": "integer",
        "description": "The Memory's memory_id, as store_memory or search_memory answered with \
            it, or as your instructions name it.",
    })
}

fn title_property() -> Value {
    json!({
        "type": "string",
        "description": "One short line naming what the Memory is about: at most 100 characters.",
    })
}

fn body_property() -> Value {
    json!({
        "type": "string",
        "description": "What the Memory keeps: at most 10000 characters.",
    })
}

fn tags_property(description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": description,
    })
}

/// The JSON Schema of `store_memory`'s arguments.
pub(super) fn store_memory_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": title_property(),
            "body": body_property(),
            "tags": tags_property(
                "At most 10 words or short phrases to find the Memory by, each at most 40 \
                 characters.",
            ),
        },
        "required": ["title", "body"],
        "additionalProperties": false,
    })
}

/// The JSON Schema of `search_memory`'s arguments.
pub(super) fn search_memory_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to find in a Memory's title, body or tags, each of which \
                    must be found unless joined by OR; words inside double quotes must stand \
                    together. Leave it out to list Memories by when they last changed.",
            },
            "tags": tags_property("Find only Memories carrying every one of these tags."),
            "changed_after": {
                "type": "string",
                "description": "An RFC 3339 moment or a YYYY-MM-DD day: find only Memories last \
                    changed at or after it.",
            },
            "changed_before": {
                "type": "string",
                "description": "An RFC 3339 moment or a YYYY-MM-DD day: find only Memories last \
                    changed before it.",
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "How many rows at most: 10 unless given.",
            },
        },
        "additionalProperties": false,
    })
}

/// The JSON Schema of the arguments of `recall_memory` and `forget_memory`.
pub(super) fn one_memory_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "memory_id": memory_id_property() },
        "required": ONE_MEMORY_TAKES,
        "additionalProperties": false,
    })
}

/// The JSON Schema of `update_memory`'s arguments.
pub(super) fn update_memory_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "memory_id": memory_id_property(),
            "title": title_property(),
            "body": body_property(),
            "tags": tags_property(
                "Every tag the Memory is to carry, replacing those it carried: at most 10, each \
                 at most 40 characters. An empty list removes them all.",
            ),
        },
        "required": ["memory_id"],
        "additionalProperties": false,
    })
}

/// What `store_memory` and `update_memory` answer: a Memory as kept, but for
/// its body, which the Sidekick has just written.
#[derive(Debug, Serialize)]
struct KeptMemory {
    memory_id: MemoryId,
    title: String,
    tags: Vec<String>,
    stored_at: String,
    changed_at: String,
}

impl From<Memory> for KeptMemory {
    fn from(memory: Memory) -> Self {
        Self {
            memory_id: memory.id,
            title: memory.title,
            tags: memory.tags,
            stored_at: spelled(memory.stored_at),
            changed_at: spelled(memory.changed_at),
        }
    }
}

/// What `recall_memory` answers: a Memory whole.
#[derive(Debug, Serialize)]
struct RecalledMemory {
    memory_id: MemoryId,
    title: String,
    body: String,
    tags: Vec<String>,
    stored_at: String,
    changed_at: String,
}

impl From<Memory> for RecalledMemory {
    fn from(memory: Memory) -> Self {
        Self {
            memory_id: memory.id,
            title: memory.title,
            body: memory.body,
            tags: memory.tags,
            stored_at: spelled(memory.stored_at),
            changed_at: spelled(memory.changed_at),
        }
    }
}

/// What `search_memory` answers.
#[derive(Debug, Serialize)]
struct MemoryListing {
    memories: Vec<ListedMemory>,
    /// How many Memories matched but were left out past the limit.
    omitted: usize,
}

/// One row of a search: never a body, only a snippet of one.
#[derive(Debug, Serialize)]
struct ListedMemory {
    memory_id: MemoryId,
    title: String,
    tags: Vec<String>,
    stored_at: String,
    changed_at: String,
    snippet: String,
}

impl From<FoundMemory> for ListedMemory {
    fn from(found: FoundMemory) -> Self {
        Self {
            memory_id: found.id,
            title: found.title,
            tags: found.tags,
            stored_at: spelled(found.stored_at),
            changed_at: spelled(found.changed_at),
            snippet: found.snippet,
        }
    }
}

/// A moment as every Memory Tool spells it, and `search_memory` takes it
/// back: as `list_sessions` spells a Session's.
fn spelled(at: SessionTimestamp) -> String {
    session_listing::spelled(at)
}

/// Refuses a call of `tool` naming an `origin`: Memories stay with the
/// Sidekick's own Server, so none is reached on a Remote.
fn takes_no_origin(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<(), ToolRefusal> {
    if arguments.contains_key("origin") {
        return Err(ToolRefusal::new(format!(
            "{} takes no `origin`: Memories are this server's alone, kept for its own Sidekicks, \
             so nothing was done.",
            tool.name()
        )));
    }
    Ok(())
}

/// What a Sidekick is told where the Server's own storage failed it, and so
/// `not_done` was not done.
fn storage_failed(not_done: &str) -> ToolRefusal {
    ToolRefusal::new(format!(
        "Suru's own storage failed, so {not_done}; its Log says how."
    ))
}

/// What a Sidekick is told of a memory_id naming no Memory kept here.
fn no_such_memory(id: MemoryId) -> ToolRefusal {
    ToolRefusal::new(format!(
        "Suru keeps no Memory {id} on this server: it was never stored here, or has been \
         forgotten since. search_memory finds Memories by their words, and lists them when \
         given none."
    ))
}

/// The Memory a call of `tool` names by its `memory_id`: a whole number, or
/// the digits of one, as a harness may send a number it was given.
fn memory_id(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<MemoryId, ToolRefusal> {
    let name = tool.name();
    let named = match arguments.get("memory_id") {
        None | Some(Value::Null) => {
            return Err(ToolRefusal::new(format!(
                "{name} needs `memory_id`, as store_memory or search_memory answered with it."
            )));
        }
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(digits)) => digits.trim().parse::<i64>().ok(),
        Some(_) => None,
    };
    named.map(MemoryId::new).ok_or_else(|| {
        ToolRefusal::new(format!(
            "{name}'s `memory_id` must be a whole number, as store_memory or search_memory \
             answered with it."
        ))
    })
}

/// The title a call of `tool` gives, kept on one line and held to its
/// bound, or `None` where it gives none; `not_done` says what a refusal
/// leaves undone.
fn title(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    not_done: &str,
) -> Result<Option<String>, ToolRefusal> {
    let name = tool.name();
    let title = match arguments.get("title") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(title)) => memories::one_line(title),
        Some(_) => {
            return Err(ToolRefusal::new(format!(
                "{name}'s `title` must be a string."
            )));
        }
    };
    if title.is_empty() {
        return Err(ToolRefusal::new(format!(
            "{name}'s `title` is empty; give the Memory a short line naming what it is about. \
             {not_done}"
        )));
    }
    let length = title.chars().count();
    if length > MAX_TITLE_CHARS {
        return Err(ToolRefusal::new(format!(
            "{name}'s `title` runs to {length} characters, and a Memory's title holds at most \
             {MAX_TITLE_CHARS}; shorten it, and keep the rest in its body. {not_done}"
        )));
    }
    Ok(Some(title))
}

/// The body a call of `tool` gives, held to its bound, or `None` where it
/// gives none; `not_done` says what a refusal leaves undone.
fn body(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    not_done: &str,
) -> Result<Option<String>, ToolRefusal> {
    let name = tool.name();
    let body = match arguments.get("body") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(body)) => memories::body(body),
        Some(_) => {
            return Err(ToolRefusal::new(format!(
                "{name}'s `body` must be a string."
            )));
        }
    };
    if body.trim().is_empty() {
        return Err(ToolRefusal::new(format!(
            "{name}'s `body` is empty; say what the Memory keeps. {not_done}"
        )));
    }
    let length = body.chars().count();
    if length > MAX_BODY_CHARS {
        return Err(ToolRefusal::new(format!(
            "{name}'s `body` runs to {length} characters, and a Memory's body holds at most \
             {MAX_BODY_CHARS}; keep what matters, or keep the rest as Memories of their own. \
             {not_done}"
        )));
    }
    Ok(Some(body))
}

/// The strings a `tags` argument of `tool` lists, or `None` where it gives
/// none.
fn written_tags(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
) -> Result<Option<Vec<&str>>, ToolRefusal> {
    let refusal = || {
        ToolRefusal::new(format!(
            "{}'s `tags` must be a list of strings, such as [\"release\", \"ci\"].",
            tool.name()
        ))
    };
    match arguments.get("tags") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(tags)) => tags
            .iter()
            .map(|tag| tag.as_str().ok_or_else(refusal))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(refusal()),
    }
}

/// The tags a call of `tool` gives a Memory, as the Memory carries them and
/// held to their bounds, or `None` where it gives none; `not_done` says what
/// a refusal leaves undone.
fn tags(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    not_done: &str,
) -> Result<Option<Vec<String>>, ToolRefusal> {
    let name = tool.name();
    let Some(written) = written_tags(tool, arguments)? else {
        return Ok(None);
    };
    let kept = memories::tags(written);
    if kept.len() > MAX_TAGS {
        return Err(ToolRefusal::new(format!(
            "{name}'s `tags` name {} tags, and a Memory carries at most {MAX_TAGS}. {not_done}",
            kept.len()
        )));
    }
    if let Some(length) = kept
        .iter()
        .map(|tag| tag.chars().count())
        .find(|length| *length > MAX_TAG_CHARS)
    {
        return Err(ToolRefusal::new(format!(
            "A Memory's tag holds at most {MAX_TAG_CHARS} characters, and one of {name}'s \
             `tags` runs to {length}; shorten it. {not_done}"
        )));
    }
    Ok(Some(kept))
}

/// What `search_memory` was called with, each argument checked for the shape
/// its schema gives it.
fn search_arguments(arguments: &Map<String, Value>) -> Result<MemorySearch, ToolRefusal> {
    let tool = BrokerTool::SearchMemory;
    let query = match arguments.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(written)) => {
            MatchQuery::read(written).map_err(|refusal| match refusal {
                QueryRefusal::NothingToSearch => ToolRefusal::new(NOTHING_TO_SEARCH),
                QueryRefusal::TooManyWords(words) => ToolRefusal::new(format!(
                    "search_memory's `query` holds {words} words, and a search takes at most \
                     {MAX_QUERY_WORDS}; give the few that matter most."
                )),
            })?
        }
        Some(_) => {
            return Err(ToolRefusal::new(
                "search_memory's `query` must be a string.",
            ));
        }
    };
    let moment = |argument: &str| match arguments.get(argument) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(spelled)) if spelled.trim().is_empty() => Ok(None),
        Some(Value::String(spelled)) => session_listing::moment(spelled.trim())
            .map(Some)
            .ok_or_else(|| {
                ToolRefusal::new(format!(
                    "search_memory's `{argument}` must be an RFC 3339 moment, such as \
                     2026-10-01T09:30:00Z, or a day, such as 2026-10-01."
                ))
            }),
        Some(_) => Err(ToolRefusal::new(format!(
            "search_memory's `{argument}` must be a string."
        ))),
    };
    let limit = match arguments.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(Value::Number(number)) if number.as_u64().is_some_and(|rows| rows > 0) => {
            // More rows than there could be Memories asks for all of them.
            usize::try_from(number.as_u64().unwrap_or(u64::MAX)).unwrap_or(usize::MAX)
        }
        Some(Value::Number(number)) if number.is_u64() || number.is_i64() => {
            return Err(ToolRefusal::new(format!(
                "search_memory's `limit` must be a whole number of rows, at least 1; {number} \
                 asks for none. Leave it out for {DEFAULT_LIMIT}."
            )));
        }
        Some(_) => {
            return Err(ToolRefusal::new(
                "search_memory's `limit` must be a whole number of rows, at least 1.",
            ));
        }
    };
    Ok(MemorySearch {
        query,
        tags: memories::tags(written_tags(tool, arguments)?.unwrap_or_default()),
        changed_after: moment("changed_after")?,
        changed_before: moment("changed_before")?,
        limit,
    })
}

impl BrokerTools {
    /// Answers `store_memory`: stores the Memory the call gives, and says
    /// how it was kept.
    pub(super) async fn store_memory(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::StoreMemory;
        let arguments = &call.arguments;
        takes_no_origin(tool, arguments)?;
        takes_only(tool, arguments, &STORE_TAKES)?;
        let not_done = "Nothing was stored.";
        let title = title(tool, arguments, not_done)?.ok_or_else(|| {
            ToolRefusal::new(
                "store_memory needs `title`, one short line naming what the Memory is about.",
            )
        })?;
        let body = body(tool, arguments, not_done)?
            .ok_or_else(|| ToolRefusal::new("store_memory needs `body`, what the Memory keeps."))?;
        let tags = tags(tool, arguments, not_done)?.unwrap_or_default();
        let memory = self
            .operations
            .store_memory(NewMemory { title, body, tags })
            .await
            .map_err(|_| storage_failed("nothing was stored"))?;
        Ok(
            serde_json::to_value(KeptMemory::from(memory))
                .expect("a kept Memory always serializes"),
        )
    }

    /// Answers `search_memory`: the Memories the call finds, as rows that
    /// never carry a whole body, and how many it left out.
    pub(super) async fn search_memory(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::SearchMemory;
        takes_no_origin(tool, &call.arguments)?;
        takes_only(tool, &call.arguments, &SEARCH_TAKES)?;
        let search = search_arguments(&call.arguments)?;
        let found = self
            .operations
            .search_memories(search)
            .await
            .map_err(|_| storage_failed("no Memory was searched"))?;
        let omitted = found.matched.saturating_sub(found.found.len());
        Ok(serde_json::to_value(MemoryListing {
            memories: found.found.into_iter().map(ListedMemory::from).collect(),
            omitted,
        })
        .expect("a listing of Memories always serializes"))
    }

    /// Answers `recall_memory`: the Memory the call names, whole.
    pub(super) async fn recall_memory(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::RecallMemory;
        takes_no_origin(tool, &call.arguments)?;
        takes_only(tool, &call.arguments, &ONE_MEMORY_TAKES)?;
        let id = memory_id(tool, &call.arguments)?;
        let memory = self
            .operations
            .recall_memory(id)
            .await
            .map_err(|_| storage_failed("no Memory was read"))?
            .ok_or_else(|| no_such_memory(id))?;
        Ok(serde_json::to_value(RecalledMemory::from(memory))
            .expect("a recalled Memory always serializes"))
    }

    /// Answers `update_memory`: changes what the call names of the Memory it
    /// names, and says how that Memory stands now.
    pub(super) async fn update_memory(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::UpdateMemory;
        let arguments = &call.arguments;
        takes_no_origin(tool, arguments)?;
        takes_only(tool, arguments, &UPDATE_TAKES)?;
        let id = memory_id(tool, arguments)?;
        let not_done = "Nothing was changed.";
        let change = MemoryChange {
            title: title(tool, arguments, not_done)?,
            body: body(tool, arguments, not_done)?,
            tags: tags(tool, arguments, not_done)?,
        };
        if change.is_empty() {
            return Err(ToolRefusal::new(NOTHING_TO_CHANGE));
        }
        let memory = self
            .operations
            .change_memory(id, change)
            .await
            .map_err(|_| storage_failed("nothing was changed"))?
            .ok_or_else(|| no_such_memory(id))?;
        Ok(
            serde_json::to_value(KeptMemory::from(memory))
                .expect("a kept Memory always serializes"),
        )
    }

    /// Answers `forget_memory`: forgets the Memory the call names.
    pub(super) async fn forget_memory(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::ForgetMemory;
        takes_no_origin(tool, &call.arguments)?;
        takes_only(tool, &call.arguments, &ONE_MEMORY_TAKES)?;
        let id = memory_id(tool, &call.arguments)?;
        let forgotten = self
            .operations
            .forget_memory(id)
            .await
            .map_err(|_| storage_failed("nothing was forgotten"))?;
        if !forgotten {
            return Err(no_such_memory(id));
        }
        Ok(json!({ "memory_id": id, "forgotten": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memories::SNIPPET_CHARS;

    fn arguments(arguments: Value) -> Map<String, Value> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        arguments
    }

    #[test]
    fn every_description_states_the_bounds_it_holds_a_memory_to() {
        for (description, bounds) in [
            (
                STORE_MEMORY_DESCRIPTION,
                vec![
                    format!("at most {MAX_TITLE_CHARS} characters"),
                    format!("at most {MAX_BODY_CHARS} characters"),
                    format!("at most {MAX_TAGS} words or short phrases"),
                    format!("each at most {MAX_TAG_CHARS} characters"),
                ],
            ),
            (
                SEARCH_MEMORY_DESCRIPTION,
                vec![
                    format!("{DEFAULT_LIMIT} unless given"),
                    format!("at most {SNIPPET_CHARS} characters of its body"),
                    format!("more than {MAX_QUERY_WORDS} words"),
                ],
            ),
        ] {
            for bound in bounds {
                assert!(description.contains(&bound), "{bound}: {description}");
            }
        }
    }

    #[test]
    fn every_schema_takes_what_its_tool_reads() {
        for (schema, takes) in [
            (store_memory_schema(), STORE_TAKES.to_vec()),
            (search_memory_schema(), SEARCH_TAKES.to_vec()),
            (one_memory_schema(), ONE_MEMORY_TAKES.to_vec()),
            (update_memory_schema(), UPDATE_TAKES.to_vec()),
        ] {
            let mut properties = schema["properties"]
                .as_object()
                .expect("the schema names its properties")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            properties.sort_unstable();
            let mut takes = takes;
            takes.sort_unstable();
            assert_eq!(properties, takes);
        }
    }

    #[test]
    fn a_memory_id_is_a_whole_number_or_its_digits() {
        let read = |named: Value| memory_id(BrokerTool::RecallMemory, &arguments(named));
        assert_eq!(read(json!({ "memory_id": 12 })), Ok(MemoryId::new(12)));
        assert_eq!(read(json!({ "memory_id": " 12 " })), Ok(MemoryId::new(12)));
        for refused in [json!(1.5), json!("twelve"), json!([12]), json!(true)] {
            let refusal = read(json!({ "memory_id": refused })).expect_err("refused");
            assert!(
                refusal.to_string().contains("must be a whole number"),
                "{refused}: {refusal}"
            );
        }
        assert!(
            read(json!({}))
                .expect_err("refused")
                .to_string()
                .starts_with("recall_memory needs `memory_id`")
        );
    }

    #[test]
    fn a_search_reads_its_filters_and_defaults_to_ten_rows() {
        let search = search_arguments(&arguments(json!({}))).expect("a search");
        assert_eq!(
            search,
            MemorySearch {
                query: None,
                tags: Vec::new(),
                changed_after: None,
                changed_before: None,
                limit: DEFAULT_LIMIT,
            }
        );
        let search = search_arguments(&arguments(json!({
            "query": "  ",
            "tags": ["#Release", "release", "CI"],
            "changed_after": "1970-01-02",
            "changed_before": "",
            "limit": 3,
        })))
        .expect("a search");
        assert_eq!(search.query, None, "a query of whitespace is none");
        assert_eq!(search.tags, ["release", "ci"]);
        assert_eq!(
            search.changed_after,
            Some(SessionTimestamp(24 * 60 * 60 * 1_000))
        );
        assert_eq!(search.changed_before, None);
        assert_eq!(search.limit, 3);
    }
}
