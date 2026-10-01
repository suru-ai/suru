//! `list_workspaces` and `set_workspace_description`: the Tools through which
//! a Sidekick learns which Workspaces its own Server knows — or a Remote, or
//! every Server Everywhere — and records what it learned one is for. A
//! Remote's Workspaces are listed as that Remote lists them to anyone who
//! asks, named in its own paths' syntax, each row carrying the Remote's name;
//! a Description is set on this Server's alone for now.
//!
//! A Server knows a Workspace a Session works in — the one a Client's
//! Workspace Picker offers for each listed Session — and one it holds a
//! Description or Icon for though no Session works there. The listing names
//! each as a compact row: its identity, its name, its presented root's path,
//! its Description and whether that was set rather than derived, and its
//! Icon's name. A directory a Client merely stands in, which its Server was
//! never told of, is no Workspace the Server knows, and is not listed.
//!
//! A Description a Sidekick sets is set through the very operations the
//! Workspace endpoint performs for the user — a directory no known Workspace
//! is presented at resolved as the endpoint resolves one
//! ([`SessionOperations::workspace_at`](crate::server::operations::SessionOperations::workspace_at)),
//! then
//! [`SessionStore::set_workspace_description`](crate::sessions::SessionStore::set_workspace_description)
//! — so it stands against every later derivation as the user's does, reaches
//! every Client in the same catalog change, and makes a Workspace no Session
//! has worked in yet one the Server knows; text with nothing in it clears it,
//! so it may be derived again.

use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    BrokerTool, BrokerTools, ToolCall, ToolRefusal,
    origins::{self, Unanswered},
    takes_only,
};
use crate::protocol::{Outlook, Workspace, WorkspaceDescription, WorkspaceId, WorkspacePaths};

pub(super) const LIST_WORKSPACES_DESCRIPTION: &str = "\
List the Workspaces this Suru server knows — the Repositories and \
directories the user works in — so you can pick the right one for work, as \
compact rows: first those the user's Sessions work in, most recently worked \
in first, then those Suru holds a Description or Icon for though no Session \
works there, by path. A directory Suru has never been told of is not listed, \
even one the user's Client stands in; set_workspace_description makes one \
known. Takes one optional argument, \"origin\": the name of a Remote, as \
list_remotes gives it, to list the Workspaces that Remote knows instead, or \
\"everywhere\" to list this server's and then each Remote's in turn. Answers \
with JSON of the shape {\"workspaces\": [row, ...]}. Each row has \
\"workspace_id\", the Workspace's identity; \"origin\", the name of the \
Remote that knows it, left out for one on this server; \
\"name\", the name the user knows it by; \"path\", the path it is presented \
by, which list_sessions takes as its \"workspace\" and gives in its rows; \
\"description\", what it is for, as {\"text\": \"...\", \"set\": true} where \
the user or a Sidekick set it, \"set\": false where Suru derived it, or null \
where it has none; and \"icon\", the name of its Icon in Suru's Icon \
Catalog, or null. Your own Workspace, the Sidekick Workspace, is among \
them. A listing \"everywhere\" lists only what each server answered with \
now: one with a Remote that did not answer also has \"unanswered\", a list \
naming each such Remote as \"origin\" with a \"reason\" saying why, and none \
of its Workspaces. An \"origin\" naming a Remote this server is not paired \
with, or one that does not answer, is refused saying so.";

pub(super) const SET_WORKSPACE_DESCRIPTION_DESCRIPTION: &str = "\
Set a Workspace's Description on this Suru server: a sentence or two saying \
what it is for, so that you, the user and later Sidekicks can tell \
Workspaces apart by more than a name. A Description you set stands as one \
the user set — Suru never derives another over it — and every Client shows \
it. Takes \"workspace\": a Workspace's \"workspace_id\" or \"path\" exactly as \
list_workspaces gives them, or the absolute path of any directory, which \
names the Workspace it lies in, so you may describe one no Session has \
worked in yet; and \"text\", the Description, kept on one line and at most \
300 characters, where text with nothing in it, such as \"\", clears the \
Description so Suru may derive one again. Answers with JSON of the shape \
{\"workspace_id\": \"...\", \"path\": \"...\", \"description\": {\"text\": \
\"...\", \"set\": true}}: the Workspace described, the path it is presented \
by, and the Description it carries now, null once cleared. A \"workspace\" \
that is neither a Workspace Suru knows nor an existing directory, and text \
running longer, are refused saying so.";

/// What `list_workspaces` takes: the Servers whose Workspaces to list.
const LIST_TAKES: [&str; 1] = ["origin"];

/// The JSON Schema of `list_workspaces`' arguments.
pub(super) fn list_workspaces_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "origin": origins::origins_property("Workspaces"),
        },
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
                "description": "The workspace_id or the path of a Workspace, exactly as \
                    list_workspaces gives them, or the absolute path of a directory in the \
                    Workspace.",
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
    /// The Workspace as the call named it — by its identity, by the path it
    /// is presented by, or by a directory in it — exactly as given, since
    /// `atlas ` and `atlas` may be two directories.
    workspace: String,
    /// The Description, as the Sidekick wrote it; the store keeps it on one
    /// line, and text with nothing in it clears it.
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
            Some(Value::String(named)) if !named.trim().is_empty() => named.clone(),
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
    /// The Remotes a listing Everywhere could not list, each saying why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unanswered: Vec<Unanswered>,
}

/// One row of a listing.
#[derive(Debug, Serialize)]
struct ListedWorkspace {
    workspace_id: WorkspaceId,
    /// The Remote that knows the Workspace, and nothing for one this Server
    /// knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    /// The name every listing of Workspaces gives it.
    name: String,
    /// The path it is presented by — its presented root — as `list_sessions`
    /// spells a Workspace too.
    path: String,
    description: Option<WorkspaceDescription>,
    /// The Icon Catalog's name for its Icon, never the glyph: a name is what
    /// an Agent can read.
    icon: Option<String>,
}

/// What `set_workspace_description` answers: the Workspace it described,
/// where that is presented, and the Description it carries now.
#[derive(Debug, Serialize)]
struct DescribedWorkspace {
    workspace_id: WorkspaceId,
    path: String,
    description: Option<WorkspaceDescription>,
}

/// `workspace`, known at `origin`, as a row names it — in the syntax of
/// `paths`, the paths of the Server that knows it.
fn listed_workspace(
    origin: &Outlook,
    paths: &WorkspacePaths,
    workspace: Workspace,
) -> ListedWorkspace {
    ListedWorkspace {
        origin: origins::row_origin(origin.clone()),
        name: paths.workspace_name(&workspace),
        path: workspace.path.to_string_lossy().into_owned(),
        workspace_id: workspace.id,
        description: workspace.description,
        icon: workspace.icon,
    }
}

/// The one Workspace among `known` that `named` names, by its identity or by
/// the path it is presented by, or `None` where none is. Two Workspaces may
/// be presented at one path; naming that path names neither, and is refused
/// for their identities.
fn named_among(known: Vec<Workspace>, named: &str) -> Result<Option<Workspace>, ToolRefusal> {
    let mut named_so = known
        .into_iter()
        .filter(|workspace| workspace.is_named_by(named));
    match (named_so.next(), named_so.next()) {
        (Some(workspace), None) => Ok(Some(workspace)),
        (Some(_), Some(_)) => Err(ToolRefusal::new(format!(
            "More than one Workspace is presented at `{named}`; name the one you mean by the \
             workspace_id list_workspaces gives it."
        ))),
        (None, _) => Ok(None),
    }
}

/// What a `workspace` naming no Workspace this server knows, and no
/// directory to find one in, is refused with.
fn unknown_workspace(named: &str) -> ToolRefusal {
    ToolRefusal::new(format!(
        "Suru knows no Workspace `{named}` on this server, and no directory is there to find one \
         in; name a Workspace by the workspace_id or the path list_workspaces gives it, or by the \
         absolute path of a directory in it."
    ))
}

impl BrokerTools {
    /// Answers `list_workspaces`: every Workspace known at the Origins the
    /// call ranges over, this server's alone unless it names others, as a
    /// row.
    pub(super) async fn list_workspaces(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_only(BrokerTool::ListWorkspaces, &call.arguments, &LIST_TAKES)?;
        let origins = origins::origins(BrokerTool::ListWorkspaces, &call.arguments)?;
        let gathered = self
            .operations
            .workspaces_in(&origins)
            .await
            .map_err(|refusal| {
                origins::origin_refusal(refusal, "Its Workspaces were not listed.")
            })?;
        let listing = WorkspaceListing {
            workspaces: gathered
                .answered
                .into_iter()
                .flat_map(|(origin, listing)| {
                    let paths = listing.workspace_paths;
                    listing
                        .workspaces
                        .into_iter()
                        .map(move |workspace| listed_workspace(&origin, &paths, workspace))
                })
                .collect(),
            unanswered: gathered
                .unanswered
                .into_iter()
                .map(Unanswered::from)
                .collect(),
        };
        Ok(serde_json::to_value(listing).expect("a listing of Workspaces always serializes"))
    }

    /// Answers `set_workspace_description`: sets, or clears, the named
    /// Workspace's Description as the user's own setting of it does — one
    /// this server knows, or the one a directory resolves to — and says what
    /// it carries now.
    pub(super) async fn set_workspace_description(
        &self,
        call: &ToolCall,
    ) -> Result<Value, ToolRefusal> {
        let arguments = DescribeArguments::read(&call.arguments)?;
        let (workspace, resolved) =
            match named_among(self.sessions.listed_workspaces(), &arguments.workspace)? {
                Some(known) => (known, None),
                None => {
                    let resolved = self
                        .operations
                        .workspace_at(Path::new(&arguments.workspace))
                        .await
                        .ok_or_else(|| unknown_workspace(&arguments.workspace))?;
                    (resolved.clone(), Some(resolved))
                }
            };
        let description = self
            .sessions
            .set_workspace_description(&workspace.id, &arguments.text, resolved.as_ref())
            .map_err(|refusal| ToolRefusal::new(format!("{refusal}.")))?;
        Ok(serde_json::to_value(DescribedWorkspace {
            path: workspace.path.to_string_lossy().into_owned(),
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
    fn a_workspace_is_named_by_its_identity_or_the_path_it_is_presented_by_exactly() {
        let atlas = workspace("atlas");
        let notes = workspace("notes");
        let known = vec![atlas.clone(), notes.clone()];
        assert_eq!(named_among(known.clone(), &notes.id.0), Ok(Some(notes)));
        let path = atlas.path.to_string_lossy().into_owned();
        assert_eq!(named_among(known.clone(), &path), Ok(Some(atlas)));
        assert_eq!(
            named_among(known.clone(), &format!("{path} ")),
            Ok(None),
            "`atlas ` may be another directory than `atlas`"
        );
        assert_eq!(named_among(known, "elsewhere"), Ok(None));
    }

    #[test]
    fn a_path_two_workspaces_are_presented_at_is_refused_for_their_identities() {
        let directory = workspace("atlas");
        let repository = Workspace {
            id: WorkspaceId("git:atlas".to_owned()),
            ..directory.clone()
        };
        let refusal = named_among(
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
            named_among(vec![directory, repository.clone()], "git:atlas"),
            Ok(Some(repository)),
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
        let here = WorkspacePaths::default();
        assert_eq!(
            serde_json::to_value(listed_workspace(&Outlook::Local, &here, atlas.clone()))
                .expect("a row serializes"),
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
            listed_workspace(&Outlook::Local, &here, unknown_main).name,
            "atlas.git (main checkout unknown)"
        );
    }

    #[test]
    fn a_remotes_row_carries_its_name_and_is_named_in_the_remotes_own_syntax() {
        let windows = WorkspacePaths {
            home: None,
            style: crate::protocol::PathStyle::Windows,
        };
        let atlas = Workspace::directory(PathBuf::from(r"C:\Users\ada\atlas"));
        let row = serde_json::to_value(listed_workspace(
            &Outlook::Remote("workstation".to_owned()),
            &windows,
            atlas,
        ))
        .expect("a row serializes");
        assert_eq!(row["origin"], json!("workstation"));
        assert_eq!(
            row["name"],
            json!("atlas"),
            "a Windows Remote's Workspace is named by its last component wherever it is listed"
        );
    }

    #[test]
    fn set_arguments_are_read_as_their_schema_gives_them() {
        assert_eq!(
            arguments(json!({ "workspace": "/srv/atlas ", "text": "  Charts.\n" })),
            Ok(DescribeArguments {
                workspace: "/srv/atlas ".to_owned(),
                text: "  Charts.\n".to_owned(),
            }),
            "the Workspace is named exactly as given, and the text left for the store to keep \
             on one line"
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
            list_workspaces_schema()["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>()),
            Some(LIST_TAKES.map(str::to_owned).to_vec()),
            "list_workspaces takes only the Origins to list"
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
