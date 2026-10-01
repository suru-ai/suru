//! `list_workspaces` and `set_workspace_description`: the Tools through which
//! a Sidekick learns which Workspaces there are on its own Server, and records
//! what it learned one is for.
//!
//! The listing names the Workspaces a Client's Workspace Picker offers — the
//! one each listed Session works in, once each — as compact rows: its
//! identity, its name, its presented root's path, its Description and
//! whether that was set rather than derived, and its Icon's name. A
//! Description a Sidekick sets is set through the very operation the Workspace
//! endpoint performs for the user,
//! [`SessionStore::set_workspace_description`](crate::sessions::SessionStore::set_workspace_description),
//! so it stands against every later derivation as the user's does and reaches
//! every Client in the same catalog change; empty text clears it, so it may
//! be derived again.

use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, takes_no_arguments, takes_only};
use crate::protocol::{Workspace, WorkspaceDescription, WorkspaceId, WorkspacePaths};

pub(super) const LIST_WORKSPACES_DESCRIPTION: &str = "\
List the Workspaces on this Suru server — the Repositories and directories \
the user's Sessions work in — so you can pick the right one for work, as \
compact rows, most recently worked in first. Takes no arguments. Answers with \
JSON of the shape {\"workspaces\": [row, ...]}. Each row has \
\"workspace_id\", the Workspace's identity; \"name\", the name the user knows \
it by; \"path\", the path it is presented by, which list_sessions takes as \
its \"workspace\" and gives in its rows; \"description\", what it is for, as \
{\"text\": \"...\", \"set\": true} where the user or a Sidekick set it, \
\"set\": false where Suru derived it, or null where it has none; and \
\"icon\", the name of its Icon in Suru's Icon Catalog, or null. Your own \
Workspace, the Sidekick Workspace, is among them.";

pub(super) const SET_WORKSPACE_DESCRIPTION_DESCRIPTION: &str = "\
Set a Workspace's Description on this Suru server: a sentence or two saying \
what it is for, so that you, the user and later Sidekicks can tell \
Workspaces apart by more than a name. A Description you set stands as one \
the user set — Suru never derives another over it — and every Client shows \
it. Takes \"workspace\", a Workspace's \"workspace_id\" or \"path\" as \
list_workspaces gives them, and \"text\", the Description, kept on one line \
and at most 300 characters; give \"\" to clear the Description, so Suru may \
derive one again. Answers with JSON of the shape {\"workspace_id\": \"...\", \
\"description\": {\"text\": \"...\", \"set\": true}}, or with \"description\": \
null once cleared. A Workspace list_workspaces does not list, and text \
running longer, are refused saying so.";

/// The JSON Schema of `list_workspaces`' arguments, of which there are none.
pub(super) fn list_workspaces_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
}

/// The JSON Schema of `set_workspace_description`'s arguments.
pub(super) fn set_workspace_description_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace": {
                "type": "string",
                "description": "The workspace_id or the path of a Workspace, as \
                    list_workspaces gives them.",
            },
            "text": {
                "type": "string",
                "description": "The Description: a sentence or two, at most 300 characters. \
                    \"\" clears it, so Suru may derive one again.",
            },
        },
        "required": DescribeArguments::TAKES,
        "additionalProperties": false,
    })
}

/// What `set_workspace_description` was called with.
#[derive(Debug, Eq, PartialEq)]
struct DescribeArguments {
    /// The Workspace as the call named it: by its identity, or by the path
    /// it is presented by.
    workspace: String,
    /// The Description, as the Sidekick wrote it; the store keeps it on one
    /// line, and blank text clears it.
    text: String,
}

impl DescribeArguments {
    const TAKES: [&'static str; 2] = ["workspace", "text"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        takes_only(BrokerTool::SetWorkspaceDescription, arguments, &Self::TAKES)?;
        let workspace = match arguments.get("workspace") {
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "set_workspace_description needs `workspace`, the workspace_id or the path \
                     of a Workspace as list_workspaces gives it.",
                ));
            }
            Some(Value::String(named)) if !named.trim().is_empty() => named.trim().to_owned(),
            Some(_) => {
                return Err(ToolRefusal::new(
                    "set_workspace_description's `workspace` must be the workspace_id or the \
                     path of a Workspace as list_workspaces gives it.",
                ));
            }
        };
        let text = match arguments.get("text") {
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "set_workspace_description needs `text`, the Description to set; give \"\" \
                     to clear it, so Suru may derive one again.",
                ));
            }
            Some(Value::String(text)) => text.clone(),
            Some(_) => {
                return Err(ToolRefusal::new(
                    "set_workspace_description's `text` must be a string: the Description to \
                     set, or \"\" to clear it.",
                ));
            }
        };
        Ok(Self { workspace, text })
    }
}

/// What `list_workspaces` answers.
#[derive(Debug, Serialize)]
struct WorkspaceListing {
    workspaces: Vec<ListedWorkspace>,
}

/// One row of a listing.
#[derive(Debug, Serialize)]
struct ListedWorkspace {
    workspace_id: WorkspaceId,
    /// The name a Workspace Picker row gives it.
    name: String,
    /// The path it is presented by — its presented root — as `list_sessions`
    /// spells a Workspace too.
    path: String,
    description: Option<WorkspaceDescription>,
    /// The Icon Catalog's name for its Icon, never the glyph: a name is what
    /// an Agent can read.
    icon: Option<String>,
}

/// What `set_workspace_description` answers: the Description the Workspace
/// carries now.
#[derive(Debug, Serialize)]
struct DescribedWorkspace {
    workspace_id: WorkspaceId,
    description: Option<WorkspaceDescription>,
}

fn listed_workspace(workspace: Workspace) -> ListedWorkspace {
    let name = WorkspacePaths::default().name(&workspace.path);
    ListedWorkspace {
        // Named as the Workspace Picker names a Workspace whose main
        // checkout Suru does not know, since its path then is no checkout.
        name: if workspace.main_unknown() {
            format!("{name} (main checkout unknown)")
        } else {
            name
        },
        path: workspace.path.to_string_lossy().into_owned(),
        workspace_id: workspace.id,
        description: workspace.description,
        icon: workspace.icon,
    }
}

/// The Workspace `named` names among `listed`: the one whose identity it is,
/// or else the one presented at the path it spells.
fn named_workspace(listed: Vec<Workspace>, named: &str) -> Result<Workspace, ToolRefusal> {
    if let Some(workspace) = listed.iter().find(|workspace| workspace.id.0 == named) {
        return Ok(workspace.clone());
    }
    let mut presented_there = listed
        .into_iter()
        .filter(|workspace| workspace.path == Path::new(named));
    match (presented_there.next(), presented_there.next()) {
        (Some(workspace), None) => Ok(workspace),
        (Some(_), Some(_)) => Err(ToolRefusal::new(format!(
            "More than one Workspace is presented at `{named}`; name the one you mean by the \
             workspace_id list_workspaces gives it."
        ))),
        (None, _) => Err(ToolRefusal::new(format!(
            "Suru knows no Workspace `{named}` on this server; name one by the workspace_id or \
             the path list_workspaces gives it."
        ))),
    }
}

impl BrokerTools {
    /// Answers `list_workspaces`: every Workspace a listed Session works in,
    /// as a row.
    pub(super) fn list_workspaces(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_no_arguments(BrokerTool::ListWorkspaces, &call.arguments)?;
        let listing = WorkspaceListing {
            workspaces: self
                .sessions
                .listed_workspaces()
                .into_iter()
                .map(listed_workspace)
                .collect(),
        };
        Ok(serde_json::to_value(listing).expect("a listing of Workspaces always serializes"))
    }

    /// Answers `set_workspace_description`: sets, or clears, the named
    /// Workspace's Description as the user's own setting of it does, and says
    /// what it carries now.
    pub(super) fn set_workspace_description(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let arguments = DescribeArguments::read(&call.arguments)?;
        let workspace = named_workspace(self.sessions.listed_workspaces(), &arguments.workspace)?;
        // A listed Workspace is one this server knows by its own listing,
        // even one only a Session Suru could not read works in, so it is
        // named as the Workspace resolved.
        let description = self
            .sessions
            .set_workspace_description(&workspace.id, &arguments.text, Some(&workspace))
            .map_err(|refusal| ToolRefusal::new(format!("{refusal}.")))?;
        Ok(serde_json::to_value(DescribedWorkspace {
            workspace_id: workspace.id,
            description,
        })
        .expect("a described Workspace always serializes"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::protocol::{
        MAX_WORKSPACE_DESCRIPTION_CHARS, Repository, RepositoryId, RepositoryLocation,
        SourceControlAvailability, SourceControlCapabilities,
    };

    fn arguments(arguments: Value) -> Result<DescribeArguments, ToolRefusal> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        DescribeArguments::read(&arguments)
    }

    /// A directory Workspace presented at `path`, rooted for the platform.
    fn workspace(path: &str) -> Workspace {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        Workspace::directory(PathBuf::from(root).join(path))
    }

    #[test]
    fn a_workspace_is_named_by_its_identity_or_the_path_it_is_presented_by() {
        let atlas = workspace("atlas");
        let notes = workspace("notes");
        let listed = vec![atlas.clone(), notes.clone()];
        assert_eq!(named_workspace(listed.clone(), &notes.id.0), Ok(notes));
        assert_eq!(
            named_workspace(listed.clone(), &atlas.path.to_string_lossy()),
            Ok(atlas.clone())
        );
        let unknown = named_workspace(listed, "elsewhere").expect_err("refused");
        assert!(
            unknown
                .to_string()
                .starts_with("Suru knows no Workspace `elsewhere`"),
            "{unknown}"
        );
    }

    #[test]
    fn a_path_two_workspaces_are_presented_at_is_refused_for_their_identities() {
        let directory = workspace("atlas");
        let repository = Workspace {
            id: WorkspaceId("git:atlas".to_owned()),
            ..directory.clone()
        };
        let refusal = named_workspace(
            vec![directory.clone(), repository.clone()],
            &directory.path.to_string_lossy(),
        )
        .expect_err("refused");
        assert!(
            refusal
                .to_string()
                .contains("name the one you mean by the workspace_id"),
            "{refusal}"
        );
        assert_eq!(
            named_workspace(vec![directory, repository.clone()], "git:atlas"),
            Ok(repository),
            "and either is named by its identity"
        );
    }

    #[test]
    fn a_row_names_its_workspace_as_the_workspace_picker_does() {
        let mut atlas = workspace("atlas");
        atlas.icon = Some("dev-rust".to_owned());
        atlas.description = Some(WorkspaceDescription {
            text: "Where the atlas is charted.".to_owned(),
            set: false,
        });
        assert_eq!(
            serde_json::to_value(listed_workspace(atlas.clone())).expect("a row serializes"),
            json!({
                "workspace_id": atlas.id,
                "name": "atlas",
                "path": atlas.path,
                "description": { "text": "Where the atlas is charted.", "set": false },
                "icon": "dev-rust",
            })
        );

        let metadata = workspace("atlas.git");
        let unknown_main = Workspace {
            repository: Some(Box::new(Repository {
                id: RepositoryId::from_metadata("git", &metadata.path),
                system: "git".to_owned(),
                metadata_directory: metadata.path.clone(),
                location: RepositoryLocation::UnknownMain,
                availability: SourceControlAvailability::Available,
                capabilities: SourceControlCapabilities::discovery_only(),
            })),
            ..metadata
        };
        assert_eq!(
            listed_workspace(unknown_main).name,
            "atlas.git (main checkout unknown)"
        );
    }

    #[test]
    fn set_arguments_are_read_as_their_schema_gives_them() {
        assert_eq!(
            arguments(json!({ "workspace": " git:atlas ", "text": "  Charts.\n" })),
            Ok(DescribeArguments {
                workspace: "git:atlas".to_owned(),
                text: "  Charts.\n".to_owned(),
            }),
            "the text is left for the store to keep on one line"
        );
        assert_eq!(
            arguments(json!({ "workspace": "git:atlas", "text": "" })),
            Ok(DescribeArguments {
                workspace: "git:atlas".to_owned(),
                text: String::new(),
            }),
            "empty text is taken, to clear the Description"
        );
        for refused in [
            json!({ "text": "Charts." }),
            json!({ "workspace": "", "text": "Charts." }),
            json!({ "workspace": 7, "text": "Charts." }),
            json!({ "workspace": "git:atlas" }),
            json!({ "workspace": "git:atlas", "text": null }),
            json!({ "workspace": "git:atlas", "text": ["Charts."] }),
            json!({ "workspace": "git:atlas", "text": "Charts.", "description": "Charts." }),
        ] {
            let refusal = arguments(refused.clone()).expect_err("refused");
            assert!(
                refusal.to_string().starts_with("set_workspace_description"),
                "{refused} is refused naming the Tool: {refusal}"
            );
        }
    }

    #[test]
    fn the_schemas_take_what_the_arguments_read() {
        let schema = set_workspace_description_schema();
        let mut properties = schema["properties"]
            .as_object()
            .expect("the schema names its properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        properties.sort_unstable();
        assert_eq!(properties, ["text", "workspace"]);
        assert_eq!(schema["required"], json!(DescribeArguments::TAKES));
        assert_eq!(
            list_workspaces_schema()["properties"],
            json!({}),
            "list_workspaces takes no arguments"
        );
    }

    #[test]
    fn the_descriptions_state_the_limit_a_description_is_held_to() {
        let limit = format!("at most {MAX_WORKSPACE_DESCRIPTION_CHARS} characters");
        assert!(SET_WORKSPACE_DESCRIPTION_DESCRIPTION.contains(&limit));
        assert!(
            set_workspace_description_schema()["properties"]["text"]["description"]
                .as_str()
                .is_some_and(|described| described.contains(&limit))
        );
    }
}
