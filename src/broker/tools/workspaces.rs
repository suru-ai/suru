//! `list_workspaces`, `set_workspace_description` and `clone_workspace`: the
//! Tools through which a Sidekick learns which Workspaces its own Server
//! knows — or a Remote, or every Server Everywhere — records what it learned
//! one is for, and makes a new one by Cloning a Repository URL directly
//! within a Workspace Root, each on its own Server or a Remote. A Remote's
//! Workspaces are listed as that Remote lists them to anyone who asks, named
//! in its own paths' syntax, each row carrying the Remote's name, and a
//! Description is set on one of them as a Client turned toward that Remote
//! sets one, through the Remote's own Workspace endpoint.
//!
//! Every other Agent — a Sidekick's Subagents among them — is offered
//! `list_workspaces` too, so that it knows where the user's other work lives
//! and may read it where its own Provider's tools reach (ADR 0051). It is
//! told of no Remote, so its listing is its own Server's alone, in the very
//! rows a Sidekick's is, the Sidekick Workspace among them: its description
//! says nothing of an `origin`, and a call giving one — `everywhere` among
//! them — is refused saying the Tool answers this server's Workspaces alone.
//! It is offered no Tool that describes a Workspace or begins work in one.
//!
//! A Server knows each Workspace its Workspace Registry holds an entry for —
//! every one a Session has begun in, whether or not one works there still,
//! and every one described before any Session did — and lists them from the
//! Registry alone. The listing names each as a compact row: its identity,
//! its name, its presented root's path, its Description and whether that was
//! set rather than derived, its Icon's name, and — where its directory
//! cannot be read now — why it cannot be opened. A directory a Client merely
//! stands in, which its Server was never told of, is no Workspace the Server
//! knows, and is not listed.
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
//! so it may be derived again. On a Remote, a directory it lists no Workspace
//! at is resolved by the Remote's own Workspace resolution, as a Client
//! turned toward it resolves one, which judges the path as the very text
//! given, in that Server's own syntax, and refuses one it finds nothing at in
//! its own words.
//!
//! A Clone is the very operation a Remote performs for its Peers'
//! Sidekicks
//! ([`SessionOperations::clone_workspace`](crate::server::operations::SessionOperations::clone_workspace)),
//! which judges where it may go and refuses, in words the Sidekick can pass
//! on, whatever it may not do; its Workspace is answered as a row of
//! `list_workspaces` names it, so a Sidekick may begin work there at once.
//! One with an `origin` naming a Remote is carried there through the Pairing
//! ([`SessionOperations::clone_remote_workspace`](crate::server::operations::SessionOperations::clone_remote_workspace))
//! and done there, within that Remote's own Workspace Roots: answered as that
//! Remote's listing names the Workspace, or refused in the Remote's own
//! words — or, where the Remote does not answer or does not understand a
//! Clone, refused saying so, nothing kept to Clone later. No other Agent is
//! offered it, a Sidekick's Subagents among them, since a Clone writes to the
//! user's disk outside the Agent's own Execution Directory.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    BrokerRole, BrokerTool, BrokerTools, ToolCall, ToolRefusal,
    origins::{self, Unanswered},
    takes_no_arguments, takes_only,
};
use crate::{
    protocol::{
        CloneWorkspaceRequest, Outlook, SetWorkspaceDescriptionRequest, Workspace,
        WorkspaceDescription, WorkspaceId, WorkspacePaths, WorkspaceUnavailability,
        one_line_description,
    },
    server::operations::{OriginRefusal, Origins, Refusal, RemoteActRefusal},
};

/// `list_workspaces`' description as one caller reads it. What every caller
/// is answered — the listing, its order and the shape of its rows — is
/// written once here, so a change to it reaches every caller's description;
/// each fills in what is its own: what the listing is for, what the call
/// takes, a row's `origin` and what else takes its path, and what it says
/// after the rows.
macro_rules! list_workspaces_description {
    (
        purpose: $purpose:literal,
        takes: $takes:literal,
        row_origin: $row_origin:literal,
        path_used: $path_used:literal,
        then: $then:literal $(,)?
    ) => {
        concat!(
            "List the Workspaces this Suru server knows — the Repositories and \
             directories the user works in — so you ",
            $purpose,
            ", as compact rows: first those the user's Sessions work in, most \
             recently worked in first, then those Suru knows though no Session \
             works there now, because one once did or someone described it, by \
             path. ",
            $takes,
            " Answers with JSON of the shape {\"workspaces\": [row, ...]}. Each \
             row has \"workspace_id\", the Workspace's identity; ",
            $row_origin,
            "\"name\", the name the user knows it by; \"path\", the path it is \
             presented by",
            $path_used,
            "; \"description\", what it is for, as {\"text\": \"...\", \"set\": \
             true} where the user or a Sidekick set it, \"set\": false where Suru \
             derived it, or null where it has none; \"icon\", the name of its Icon \
             in Suru's Icon Catalog, or null; and, only where Suru cannot read its \
             directory now, \"unavailable\", saying why in plain words — its \
             directory gone, or it or the directory holding it unreadable, as on a \
             drive not mounted — so no work can begin there until it can be read. ",
            $then,
        )
    };
}

pub(super) const LIST_WORKSPACES_DESCRIPTION: &str = list_workspaces_description!(
    purpose: "can pick the right one for work",
    takes: "A directory Suru has never been told of is not listed, even one the \
        user's Client stands in; set_workspace_description makes one known. Takes \
        one optional argument, \"origin\": the name of a Remote, as list_remotes \
        gives it, to list the Workspaces that Remote knows instead, or \
        \"everywhere\" to list this server's and then each Remote's in turn.",
    row_origin: "\"origin\", the name of the Remote that knows it, left out for one \
        on this server; ",
    path_used: ", which list_sessions takes as its \"workspace\" and gives in its \
        rows",
    then: "Your own Workspace, the Sidekick Workspace, is among them. A listing \
        \"everywhere\" lists only what each server answered with now: one with a \
        Remote that did not answer also has \"unanswered\", a list naming each such \
        Remote as \"origin\" with a \"reason\" saying why, and none of its \
        Workspaces. An \"origin\" naming a Remote this server is not paired with, \
        or one that does not answer, is refused saying so.",
);

/// `list_workspaces` as any Agent but a Sidekick reads it: this server's
/// Workspaces alone, in the rows a Sidekick's listing gives, and nothing of a
/// Remote or of the Tools it is not offered.
pub(super) const LIST_THIS_SERVERS_WORKSPACES_DESCRIPTION: &str = list_workspaces_description!(
    purpose: "know where the user's other work lives and may read it where your own \
        tools reach",
    takes: "Takes no arguments.",
    row_origin: "",
    path_used: "",
    then: "Suru's own Workspace, the Sidekick Workspace, is among them.",
);

pub(super) const SET_WORKSPACE_DESCRIPTION_DESCRIPTION: &str = "\
Set a Workspace's Description: a sentence or two saying what it is for, so \
that you, the user and later Sidekicks can tell Workspaces apart by more \
than a name. A Description you set stands as one the user set — Suru never \
derives another over it — and every Client shows it. Takes \"workspace\": a \
Workspace's \"workspace_id\" or \"path\" exactly as list_workspaces gives \
them, or the absolute path of any directory on the server it is on, written \
as that server writes paths, which names the Workspace it lies in, so you may \
describe one no Session has worked in yet; \"origin\", the name of the \
Remote the Workspace is on, as list_remotes or its row gives it, left out for \
one on this server; and \"text\", the \
Description, kept on one line and at most \
300 characters, where text with nothing in it, such as \"\", clears the \
Description so Suru may derive one again. Answers with JSON of the shape \
{\"workspace_id\": \"...\", \"origin\": \"...\", \"path\": \"...\", \
\"description\": {\"text\": \"...\", \"set\": true}}: the Workspace described \
and the Remote that knows it, left out for one on this server, the path it is \
presented by, and the Description it carries now, null once cleared. A \
\"workspace\" that is neither a Workspace that server knows nor an existing \
directory there is refused saying why — a Remote's in its own words — as are \
text running longer, a Remote that does not answer, and a Remote's own \
Sidekick Workspace, which refuses you as yours refuses its Peers.";

pub(super) const CLONE_WORKSPACE_DESCRIPTION: &str = "\
Clone a Repository URL into a new Workspace directly within one of this \
server's Workspace Roots — the directories the user named for their work — \
and nowhere else, or within one of a Remote's own. Answers once the Clone is \
done with the new Workspace as a row of list_workspaces, {\"workspace_id\": \
\"...\", \"name\": \"...\", \"path\": \"...\", \"description\": null, \
\"icon\": null}, ready for begin_session; Suru derives its Icon and \
Description at once, as for a Workspace a first Session begins in, and \
list_workspaces shows them once they land. Takes \"repository_url\", the \
address to Clone from as Git reads one: an https or ssh URL, user@host:path, \
or the path of a Repository on the machine of the server Cloning it; \
\"root\", the Workspace Root to Clone into, spelled as the workspace.roots \
Setting names it or as the directory it names, which may be left out where \
exactly one Root is configured and must be given where several are; \
\"name\", the directory to make within the Root, one plain directory name and \
never a path, the URL's last segment without \".git\" unless given; \
\"branch\", the branch to check out, the Repository's default unless given; \
and \"origin\", the name of a Remote, as list_remotes gives it, to Clone \
there instead — within that Remote's own Workspace Roots, which \"root\" then \
names, and by its own rules — answered with the row as that Remote lists it, \
carrying its name as \"origin\" and its path as that server writes paths; \
leave it out to Clone on this server. The Clone is Git's ordinary one, \
neither shallow nor recursing into submodules, and asks no one for anything: \
a credential Git cannot find fails it at once. It is refused, saying why — a \
Remote's in its own words — when no Workspace Root is configured; when \
\"root\" is left out while several are, or names none of them, naming those \
configured; when the Root cannot be read, since Suru never makes one; when \
\"name\" is not one plain directory name; when something already stands \
where the Clone would go, naming the Workspace there by its workspace_id \
where it is one; when Git fails, in Git's own words; when it does not finish \
within the time the server allows a Clone, well within the time a call may \
take; and when it would go within a Remote's own Sidekick Workspace, which \
refuses you as yours refuses its Peers. A Clone Git began and could not \
finish leaves nothing behind. A Remote that does not answer, or does not \
understand a Clone, refuses it, and nothing is kept to Clone there later; \
one whose answer was lost once it took the Clone may have made it all the \
same, so list its Workspaces before asking again.";

/// What `list_workspaces` takes: the Servers whose Workspaces to list.
const LIST_TAKES: [&str; 1] = ["origin"];

/// What `clone_workspace` takes.
const CLONE_TAKES: [&str; 5] = ["repository_url", "root", "name", "branch", "origin"];

/// The JSON Schema of `clone_workspace`'s arguments.
pub(super) fn clone_workspace_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repository_url": {
                "type": "string",
                "description": "The address to Clone from, as Git reads one: an https or ssh \
                    URL, user@host:path, or the path of a Repository on the machine of the server \
                    Cloning it.",
            },
            "root": {
                "type": "string",
                "description": "The Workspace Root to Clone into, spelled as the workspace.roots \
                    Setting names it or as the directory it names; leave it out where exactly \
                    one Root is configured.",
            },
            "name": {
                "type": "string",
                "description": "The directory to make within the Root: one plain directory \
                    name, never a path. The URL's last segment without .git unless given.",
            },
            "branch": {
                "type": "string",
                "description": "The branch to check out; the Repository's default unless given.",
            },
            "origin": {
                "type": "string",
                "description": "The name of a Remote, as list_remotes gives it, to Clone within \
                    that Remote's own Workspace Roots; leave it out to Clone on this server.",
            },
        },
        "required": ["repository_url"],
        "additionalProperties": false,
    })
}

/// What `clone_workspace` was called with, as the Clone takes it: a Root or
/// a branch with nothing in it names none, while a name is left for the
/// Clone to judge, whatever it holds.
fn clone_arguments(arguments: &Map<String, Value>) -> Result<CloneWorkspaceRequest, ToolRefusal> {
    let tool = BrokerTool::CloneWorkspace;
    takes_only(tool, arguments, &CLONE_TAKES)?;
    let text = |argument: &str, what: &str| match arguments.get(argument) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(given)) => Ok(Some(given.clone())),
        Some(_) => Err(ToolRefusal::new(format!(
            "{} `{argument}` must be a string: {what}.",
            super::possessive(tool)
        ))),
    };
    let named = |given: Option<String>| given.filter(|given| !given.trim().is_empty());
    let repository_url = named(text(
        "repository_url",
        "the address to Clone from, as Git reads one",
    )?)
    .ok_or_else(|| {
        ToolRefusal::new(
            "clone_workspace needs `repository_url`, the address to Clone from, as Git reads one.",
        )
    })?;
    Ok(CloneWorkspaceRequest {
        repository_url,
        root: named(text(
            "root",
            "the Workspace Root to Clone into, as the workspace.roots Setting names it",
        )?),
        name: text("name", "one plain directory name to make within the Root")?,
        branch: named(text("branch", "the branch to check out")?),
    })
}

/// The JSON Schema of `list_workspaces`' arguments, as an Agent that is
/// `role` to the Broker may give them: a Sidekick, the Origins to list, and
/// any other Agent nothing.
pub(super) fn list_workspaces_schema(role: BrokerRole) -> Value {
    match role {
        BrokerRole::Sidekick => json!({
            "type": "object",
            "properties": {
                "origin": origins::origins_property("Workspaces"),
            },
            "additionalProperties": false,
        }),
        BrokerRole::Agent => json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    }
}

/// The Servers a call of `list_workspaces` by an Agent that is `role` to the
/// Broker lists: for a Sidekick, those its `origin` names; for any other
/// Agent, which is told of no Remote, this server alone — an `origin` it
/// gives, whatever it names, refused saying so and to call again without
/// one. A Sidekick's native Codex Subagent is shown the Sidekick's schema,
/// `origin` and all, since Codex names the calling thread on a call but not
/// on `tools/list` (`docs/validation/0408-subagent-mcp-attribution.md`), so
/// the refusal says the `origin` is refused to it, not to every caller.
fn listed_origins(
    role: BrokerRole,
    arguments: &Map<String, Value>,
) -> Result<Origins, ToolRefusal> {
    let tool = BrokerTool::ListWorkspaces;
    match role {
        BrokerRole::Sidekick => {
            takes_only(tool, arguments, &LIST_TAKES)?;
            origins::origins(tool, arguments)
        }
        BrokerRole::Agent => {
            if arguments.contains_key("origin") {
                return Err(ToolRefusal::new(format!(
                    "{} answers you with this Suru server's Workspaces alone, so it takes no \
                     `origin` from you; call it again without `origin`.",
                    tool.name()
                )));
            }
            takes_no_arguments(tool, arguments)?;
            Ok(Origins::One(Outlook::Local))
        }
    }
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
                    Workspace, written as the server it is on writes paths.",
            },
            "origin": origins::origin_property(),
            "text": {
                "type": "string",
                "description": "The Description: a sentence or two, at most 300 characters. \
                    \"\" clears it, so Suru may derive one again.",
            },
        },
        "required": DescribeArguments::REQUIRED,
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
    const TAKES: [&'static str; 3] = ["workspace", "origin", "text"];
    const REQUIRED: [&'static str; 2] = ["workspace", "text"];

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
    /// Why it cannot be opened now, where its Server could not read its
    /// directory as it listed it, and nothing for one that can be.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<&'static str>,
}

/// What `set_workspace_description` answers: the Workspace it described,
/// where that is presented, and the Description it carries now.
#[derive(Debug, Serialize)]
struct DescribedWorkspace {
    workspace_id: WorkspaceId,
    /// The Remote that knows the Workspace, and nothing for one this Server
    /// knows — named as a row of `list_workspaces` names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
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
        unavailable: workspace.unavailable.map(WorkspaceUnavailability::reason),
    }
}

/// The one Workspace among `known` that `named` names, by its identity or by
/// the path it is presented by, or `None` where none is. Two Workspaces may
/// be presented at one path; naming that path names neither, and is refused
/// for their identities. A Workspace this server knows is named by a path
/// as this machine reads paths, and a Remote's only by the very text its
/// path is spelled in there.
fn named_among(
    known: Vec<Workspace>,
    named: &str,
    origin: &Outlook,
) -> Result<Option<Workspace>, ToolRefusal> {
    let mut named_so = known.into_iter().filter(|workspace| match origin {
        Outlook::Local => workspace.is_named_by(named),
        Outlook::Remote(_) => workspace.is_spelled_by(named),
    });
    match (named_so.next(), named_so.next()) {
        (Some(workspace), None) => Ok(Some(workspace)),
        (Some(_), Some(_)) => Err(ToolRefusal::new(format!(
            "More than one Workspace is presented at `{named}`; name the one you mean by the \
             workspace_id list_workspaces gives it."
        ))),
        (None, _) => Ok(None),
    }
}

/// What a `workspace` naming no Workspace the Remote `name` lists is refused
/// with where resolving it there found none: in the Remote's own words where
/// it refused the directory, and otherwise saying why it could not be asked.
/// A resolution changes nothing there, so either way nothing was done.
fn unresolved_on(name: &str, named: &str, refusal: RemoteActRefusal) -> ToolRefusal {
    match refusal {
        RemoteActRefusal::Refused {
            reason: Refusal::Said(reason) | Refusal::Failed(reason),
            ..
        } => ToolRefusal::new(format!(
            "The Remote `{name}` knows no Workspace `{named}`, and resolving it there found none: \
             \"{reason}\". Name one by the workspace_id or the path list_workspaces gives it \
             with \"origin\": \"{name}\", or by the absolute path of a directory in it, written \
             as that server writes paths."
        )),
        RemoteActRefusal::Origin(refusal) => {
            origins::origin_refusal(refusal, "Nothing was done there.")
        }
    }
}

/// What a Clone carried to a Remote is refused with: in the Remote's own
/// words where it refused it; saying it does not understand a Clone where it
/// speaks another protocol than this server's, as one without the Clone
/// does; and where the Remote took it and its answer never came back, that
/// it may have landed all the same and how to find out — otherwise as any
/// act on a Remote is refused. Nothing is kept to Clone there later.
fn remote_clone_refusal(refusal: RemoteActRefusal) -> ToolRefusal {
    match refusal {
        RemoteActRefusal::Origin(OriginRefusal::Silent(silent))
            if silent.speaks_another_protocol() =>
        {
            ToolRefusal::new(format!(
                "The Remote `{}` does not understand a Clone: it runs a version of Suru that \
                 speaks another protocol than this server's, so nothing can be asked of it until \
                 one of them is updated. Nothing was cloned there, and nothing is kept to do once \
                 it answers.",
                silent.name
            ))
        }
        refusal if refusal.may_have_acted() => {
            let remote = refusal.remote().to_owned();
            ToolRefusal::new(format!(
                "{} The Clone may have been done there all the same: list that Remote's \
                 Workspaces with list_workspaces and \"origin\": \"{remote}\" to find out before \
                 asking again; nothing is kept to do later.",
                origins::remote_act_sentence(refusal)
            ))
        }
        refusal => origins::remote_act_refusal(refusal),
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
    /// call ranges over, this server's alone unless a Sidekick's call names
    /// others, as a row.
    pub(super) async fn list_workspaces(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let origins = listed_origins(call.caller.role(), &call.arguments)?;
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
        let origin = origins::origin(BrokerTool::SetWorkspaceDescription, &call.arguments)?;
        if let Outlook::Remote(name) = &origin {
            return self.describe_remote_workspace(call, name, arguments).await;
        }
        let (workspace, presented_at) = match named_among(
            self.sessions.listed_workspaces(),
            &arguments.workspace,
            &origin,
        )? {
            Some(known) => (known, None),
            None => {
                let directory = Path::new(&arguments.workspace);
                let resolved = self
                    .operations
                    .workspace_at(directory)
                    .await
                    .ok_or_else(|| unknown_workspace(&arguments.workspace))?;
                (resolved, Some(directory))
            }
        };
        let author = self.sidekick_author(call);
        let description = self
            .operations
            .describe_workspace(&workspace.id, &arguments.text, presented_at, Some(&author))
            .await
            .map_err(|refusal| ToolRefusal::new(format!("{refusal}.")))?;
        Ok(serde_json::to_value(DescribedWorkspace {
            path: workspace.path.to_string_lossy().into_owned(),
            workspace_id: workspace.id,
            origin: None,
            description,
        })
        .expect("a described Workspace always serializes"))
    }

    /// Answers `clone_workspace`: Clones the Repository URL directly within
    /// the Workspace Root the call names, or the only one, as the server its
    /// `origin` names Clones one — this server, or a Remote, within its own
    /// Roots — and answers the Workspace made there as a row of
    /// `list_workspaces` names it.
    pub(super) async fn clone_workspace(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        let request = clone_arguments(&call.arguments)?;
        let origin = origins::origin(BrokerTool::CloneWorkspace, &call.arguments)?;
        let author = self.sidekick_author(call);
        let (paths, workspace) = match &origin {
            Outlook::Local => (
                WorkspacePaths::discover(),
                self.operations
                    .clone_workspace(&request, &author)
                    .await
                    .map_err(|refusal| ToolRefusal::new(refusal.to_string()))?,
            ),
            Outlook::Remote(name) => {
                let cloned = self
                    .operations
                    .clone_remote_workspace(name, &request, author)
                    .await
                    .map_err(remote_clone_refusal)?;
                (cloned.workspace_paths, cloned.workspace)
            }
        };
        Ok(
            serde_json::to_value(listed_workspace(&origin, &paths, workspace))
                .expect("a row of Workspaces always serializes"),
        )
    }

    /// Answers `set_workspace_description` for a Workspace the Remote `name`
    /// knows: sets, or clears, its Description through that Remote's own
    /// Workspace endpoint, as a Client turned toward the Remote does, and
    /// says what it carries now — kept on one line as every Server keeps
    /// one.
    async fn describe_remote_workspace(
        &self,
        call: &ToolCall,
        name: &str,
        arguments: DescribeArguments,
    ) -> Result<Value, ToolRefusal> {
        let listing = self
            .operations
            .remote_workspaces(name)
            .await
            .map_err(origins::remote_act_refusal)?;
        let remote = Outlook::Remote(name.to_owned());
        let (workspace, presented_at) =
            match named_among(listing.workspaces, &arguments.workspace, &remote)? {
                Some(known) => {
                    let path = known.path.clone();
                    (known, path)
                }
                // A directory the Remote lists no Workspace at is resolved
                // there, as a Client turned toward it resolves one, and is
                // described by that very text, which the Remote resolves
                // again as it describes it.
                None => {
                    let resolved = self
                        .operations
                        .resolve_remote_workspace(name, &arguments.workspace)
                        .await
                        .map_err(|refusal| unresolved_on(name, &arguments.workspace, refusal))?;
                    (resolved.workspace, PathBuf::from(&arguments.workspace))
                }
            };
        let author = self.sidekick_author(call);
        self.operations
            .describe_remote_workspace(
                name,
                &SetWorkspaceDescriptionRequest {
                    workspace_id: workspace.id.clone(),
                    path: Some(presented_at),
                    description: arguments.text.clone(),
                },
                author,
            )
            .await
            .map_err(origins::remote_act_refusal)?;
        let line = one_line_description(&arguments.text);
        Ok(serde_json::to_value(DescribedWorkspace {
            path: workspace.path.to_string_lossy().into_owned(),
            workspace_id: workspace.id,
            origin: Some(name.to_owned()),
            description: (!line.is_empty()).then_some(WorkspaceDescription {
                text: line,
                set: true,
            }),
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
        let here = Outlook::Local;
        assert_eq!(
            named_among(known.clone(), &notes.id.0, &here),
            Ok(Some(notes))
        );
        let path = atlas.path.to_string_lossy().into_owned();
        assert_eq!(
            named_among(known.clone(), &path, &here),
            Ok(Some(atlas.clone()))
        );
        assert_eq!(
            named_among(known.clone(), &format!("{path} "), &here),
            Ok(None),
            "`atlas ` may be another directory than `atlas`"
        );
        assert_eq!(named_among(known.clone(), "elsewhere", &here), Ok(None));
        assert_eq!(
            named_among(known, &path, &Outlook::Remote("workstation".to_owned())),
            Ok(Some(atlas)),
            "a Remote's Workspace is named by the text its path is spelled in"
        );
    }

    #[test]
    fn a_remotes_workspace_is_named_only_by_the_very_text_of_its_path() {
        let remote = Outlook::Remote("workstation".to_owned());
        let spelled = Workspace::directory(std::path::PathBuf::from("/srv/atlas"));
        assert_eq!(
            named_among(vec![spelled.clone()], "/srv/atlas", &remote),
            Ok(Some(spelled.clone()))
        );
        assert_eq!(
            named_among(vec![spelled], "/srv/atlas/", &remote),
            Ok(None),
            "another spelling is another directory, in the Remote's own syntax"
        );
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
            &Outlook::Local,
        )
        .expect_err("refused");
        assert!(
            refusal
                .to_string()
                .contains("name the one you mean by the workspace_id"),
            "{refusal}"
        );
        assert_eq!(
            named_among(
                vec![directory, repository.clone()],
                "git:atlas",
                &Outlook::Local
            ),
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
            sidekick_workspace: None,
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
        assert_eq!(properties, ["origin", "text", "workspace"]);
        let mut takes = DescribeArguments::TAKES.to_vec();
        takes.sort_unstable();
        assert_eq!(properties, takes);
        assert_eq!(schema["required"], json!(DescribeArguments::REQUIRED));
        assert_eq!(
            list_workspaces_schema(BrokerRole::Sidekick)["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>()),
            Some(LIST_TAKES.map(str::to_owned).to_vec()),
            "a Sidekick's list_workspaces takes only the Origins to list"
        );
        assert_eq!(
            list_workspaces_schema(BrokerRole::Agent),
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            "any other Agent's takes nothing"
        );
    }

    fn listed(role: BrokerRole, arguments: Value) -> Result<Origins, ToolRefusal> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        listed_origins(role, &arguments)
    }

    /// An Agent other than a Sidekick is told of no Remote, so its listing is
    /// this server's alone: an `origin` it gives — `everywhere` among them,
    /// and one naming this server — is refused saying so, and any other
    /// argument as a Tool taking none refuses it. A Sidekick's listing reaches
    /// where its `origin` says, as it always has.
    #[test]
    fn an_agents_listing_is_this_servers_alone_and_a_sidekicks_reaches_where_its_origin_says() {
        let here = Ok(Origins::One(Outlook::Local));
        assert_eq!(listed(BrokerRole::Agent, json!({})), here);
        for origin in [
            json!("everywhere"),
            json!("Everywhere"),
            json!("workstation"),
            json!(""),
            Value::Null,
            json!(7),
        ] {
            assert_eq!(
                listed(BrokerRole::Agent, json!({ "origin": origin })),
                Err(ToolRefusal::new(
                    "list_workspaces answers you with this Suru server's Workspaces alone, so it \
                     takes no `origin` from you; call it again without `origin`."
                )),
                "{origin}"
            );
        }
        assert_eq!(
            listed(BrokerRole::Agent, json!({ "limit": 5 })),
            Err(ToolRefusal::new(
                "list_workspaces takes no arguments; call it again without limit."
            ))
        );

        assert_eq!(listed(BrokerRole::Sidekick, json!({})), here);
        assert_eq!(
            listed(BrokerRole::Sidekick, json!({ "origin": "everywhere" })),
            Ok(Origins::Everywhere)
        );
        assert_eq!(
            listed(BrokerRole::Sidekick, json!({ "origin": "workstation" })),
            Ok(Origins::One(Outlook::Remote("workstation".to_owned())))
        );
        assert_eq!(
            listed(BrokerRole::Sidekick, json!({ "limit": 5 })),
            Err(ToolRefusal::new(
                "list_workspaces takes no argument `limit`; it takes `origin`."
            ))
        );
    }

    fn clone(arguments: Value) -> Result<CloneWorkspaceRequest, ToolRefusal> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        clone_arguments(&arguments)
    }

    #[test]
    fn clone_arguments_are_read_as_their_schema_gives_them() {
        assert_eq!(
            clone(json!({ "repository_url": "https://example.com/atlas.git" })),
            Ok(CloneWorkspaceRequest {
                repository_url: "https://example.com/atlas.git".to_owned(),
                root: None,
                name: None,
                branch: None,
            }),
            "only the Repository URL is needed"
        );
        assert_eq!(
            clone(json!({
                "repository_url": "git@example.com:atlas.git",
                "root": "~/src",
                "name": "charts",
                "branch": "main",
                "origin": null,
            })),
            Ok(CloneWorkspaceRequest {
                repository_url: "git@example.com:atlas.git".to_owned(),
                root: Some("~/src".to_owned()),
                name: Some("charts".to_owned()),
                branch: Some("main".to_owned()),
            })
        );
        assert_eq!(
            clone(json!({
                "repository_url": "git@example.com:atlas.git",
                "root": "",
                "name": null,
                "branch": "  ",
            })),
            Ok(CloneWorkspaceRequest {
                repository_url: "git@example.com:atlas.git".to_owned(),
                root: None,
                name: None,
                branch: None,
            }),
            "a Root or a branch with nothing in it names none"
        );
        assert_eq!(
            clone(json!({ "repository_url": "git@example.com:atlas.git", "name": "" })),
            Ok(CloneWorkspaceRequest {
                repository_url: "git@example.com:atlas.git".to_owned(),
                root: None,
                name: Some(String::new()),
                branch: None,
            }),
            "a name is left for the Clone to judge, whatever it holds"
        );
        for refused in [
            json!({}),
            json!({ "repository_url": "" }),
            json!({ "repository_url": "   " }),
            json!({ "repository_url": 7 }),
            json!({ "repository_url": "atlas.git", "root": 7 }),
            json!({ "repository_url": "atlas.git", "name": ["atlas"] }),
            json!({ "repository_url": "atlas.git", "branch": false }),
            json!({ "repository_url": "atlas.git", "depth": 1 }),
            json!({ "repository_url": "atlas.git", "recurse_submodules": true }),
        ] {
            let refusal = clone(refused.clone()).expect_err("refused");
            assert!(
                refusal.to_string().starts_with("clone_workspace"),
                "{refused} is refused naming the Tool: {refusal}"
            );
        }
    }

    #[test]
    fn the_clone_schema_takes_what_its_arguments_read_and_needs_only_the_url() {
        let schema = clone_workspace_schema();
        let mut properties = schema["properties"]
            .as_object()
            .expect("the schema names its properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        properties.sort_unstable();
        let mut takes = CLONE_TAKES.to_vec();
        takes.sort_unstable();
        assert_eq!(properties, takes);
        assert_eq!(schema["required"], json!(["repository_url"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn clone_workspaces_description_states_the_roots_bound_and_each_refusal() {
        for said in [
            "directly within one of this server's Workspace Roots",
            "and nowhere else",
            "as a row of list_workspaces",
            "begin_session",
            "\"repository_url\"",
            "\"root\"",
            "\"name\"",
            "\"branch\"",
            "\"origin\"",
            "neither shallow nor recursing into submodules",
            "a credential Git cannot find fails it at once",
            "when no Workspace Root is configured",
            "naming those configured",
            "when the Root cannot be read",
            "not one plain directory name",
            "naming the Workspace there by its workspace_id",
            "in Git's own words",
            "does not finish within",
            "leaves nothing behind",
            "within one of a Remote's own",
            "as list_remotes gives it",
            "a Remote's in its own words",
            "a Remote's own Sidekick Workspace",
            "does not understand a Clone",
            "nothing is kept to Clone there later",
            "list its Workspaces before asking again",
        ] {
            assert!(
                CLONE_WORKSPACE_DESCRIPTION.contains(said),
                "clone_workspace's description says {said:?}: {CLONE_WORKSPACE_DESCRIPTION}"
            );
        }
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
