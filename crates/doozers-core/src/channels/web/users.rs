//! The Users pages, `/admin/users`: the users of this Doozers, and **Remove**, which forgets a
//! user's data. Next to the other admin pages and like them: behind the login, admins only, every
//! answer `no-store`, every change a CSRF-checked form post. Design:
//! `Docs/technical/web-channel.md#user-pages`.
//!
//! The list has a row per user, with the actions a row has beside the username: **Manage**, which
//! opens the user's page ([`users_manage`](super::users_manage), with **Invite a user**), and
//! **Remove**. The Remove confirmation, `/admin/users/<id>/remove`, is built
//! from [`Forgetter::preview`]: a count for each kind of data the removal deletes, the workspaces
//! it deletes, and what stays. It says why a removal would be refused (it is the admin's own
//! account, or the last active admin's, or the user has a turn running) and offers the button only
//! when it wouldn't be. The `POST` calls [`Forgetter::forget`], which decides again.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::{Role, UserData, UserState, WorkspaceOwner};
use doozers_types::{Timestamp, UserId};
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::shell::{Nav, Shell};
use super::users_manage::{
    invite, invite_page, recovery_link, save_details, save_proxy_identity, save_role, sign_in_text,
    user_page,
};
use crate::forget::{ForgetError, ForgetPreview, Forgetter};
use crate::http::{self, RouterBuilder};
use crate::users::Users;

/// What the user pages need.
#[derive(Clone)]
pub(super) struct UserPages {
    pub(super) app: WebApp,
    pub(super) users: Arc<Users>,
    forgetter: Forgetter,
}

impl WebApp {
    /// Adds the Users pages to `builder`, listing `users` and removing one through `forgetter`,
    /// behind the same authentication middleware as the chat pages. Every answer is `no-store`.
    pub fn install_users(
        &self,
        builder: RouterBuilder,
        users: Arc<Users>,
        forgetter: Forgetter,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/users", get(list_page))
                .route("/admin/invite-user", get(invite_page).post(invite))
                .route("/admin/users/{id}", get(user_page))
                .route("/admin/users/{id}/details", post(save_details))
                .route("/admin/users/{id}/role", post(save_role))
                .route(
                    "/admin/users/{id}/proxy-identity",
                    post(save_proxy_identity),
                )
                .route("/admin/users/{id}/recovery-link", post(recovery_link))
                .route("/admin/users/{id}/remove", get(remove_page).post(remove))
                .with_state(UserPages {
                    app: self.clone(),
                    users,
                    forgetter,
                })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A user in the list.
struct UserRow {
    id: String,
    username: String,
    display_name: String,
    role: &'static str,
    admin: bool,
    state: &'static str,
    active: bool,
    /// When they last signed in, or "Never".
    last_sign_in: String,
    /// The user looking at the page.
    you: bool,
}

#[derive(Template)]
#[template(path = "web/admin_users.html")]
struct ListPage {
    shell: Shell,
    users: Vec<UserRow>,
    notice: Option<String>,
}

/// `?removed=`, after a removal: the name of the user who was removed.
#[derive(Debug, Default, Deserialize)]
struct Notice {
    #[serde(default)]
    removed: Option<String>,
}

/// `GET /admin/users`: every user, with their username, display name, role, state, and last
/// sign-in, and **Manage** and **Remove** links.
async fn list_page(
    State(pages): State<UserPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    // The last sign-ins are the store's: nothing replaces the router's list when a session starts.
    let signed_in: HashMap<UserId, Option<Timestamp>> = match pages.app.inner.store.users().await {
        Ok(stored) => stored
            .into_iter()
            .map(|stored| (stored.id, stored.last_sign_in_at))
            .collect(),
        Err(err) => return internal_error("cannot read the users", &err),
    };
    let directory = pages.users.list().get();
    let users = directory
        .users()
        .iter()
        .map(|listed| UserRow {
            id: listed.id.to_string(),
            username: listed.username.clone(),
            display_name: listed.display_name.clone(),
            role: match listed.role {
                Role::Admin => "Admin",
                Role::Member => "Member",
            },
            admin: listed.role == Role::Admin,
            state: match listed.state {
                UserState::Active => "Active",
                UserState::Disabled => "Disabled",
            },
            active: listed.state == UserState::Active,
            last_sign_in: sign_in_text(signed_in.get(&listed.id).copied().flatten()),
            you: listed.id == user.user,
        })
        .collect();
    http::render(&ListPage {
        shell: Shell::new(&user, Nav::Users),
        users,
        notice: notice.removed.map(|name| format!("Removed {name}.")),
    })
}

#[derive(Template)]
#[template(path = "web/admin_user_remove.html")]
struct RemovePage<'a> {
    shell: Shell,
    csrf: &'a str,
    id: String,
    username: String,
    display_name: String,
    /// What the removal deletes, one sentence each.
    deletes: Vec<String>,
    /// The workspaces it deletes, one sentence each.
    workspaces: Vec<String>,
    /// What stays.
    stays: Vec<String>,
    /// A turn is running, or messages are waiting.
    busy: bool,
    /// Why it would be refused, if it would be.
    refusal: Option<&'static str>,
}

/// `count` and its noun: `1 chat`, `2 chats`. `plural` is the noun's plural.
fn counted(count: u64, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// What a removal deletes, one sentence per kind of data the user has.
fn deletes(preview: &ForgetPreview) -> Vec<String> {
    let UserData {
        direct_sessions,
        conversations,
        job_sessions,
        messages,
        summaries,
        group_messages,
        group_summaries,
        inbox,
        outbox,
        runtime_jobs,
        web_sessions,
        identities,
        link_codes,
        passwords,
        passkeys,
        oidc_connections,
        invites,
        ssh_keys,
        ..
    } = &preview.data;
    let mut lines = Vec::new();
    if *direct_sessions > 0 {
        lines.push(format!(
            "{} in {}, with {} and {} in them{}.",
            counted(*conversations, "chat", "chats"),
            counted(*direct_sessions, "session", "sessions"),
            counted(*messages, "message", "messages"),
            counted(*summaries, "summary", "summaries"),
            if *job_sessions > 0 {
                " and in job runs"
            } else {
                ""
            },
        ));
    } else if *messages > 0 || *summaries > 0 {
        lines.push(format!(
            "{} and {} in their job runs.",
            counted(*messages, "message", "messages"),
            counted(*summaries, "summary", "summaries"),
        ));
    }
    if *job_sessions > 0 {
        lines.push(format!(
            "{} of the jobs they scheduled.",
            counted(*job_sessions, "run", "runs")
        ));
    }
    if *group_messages > 0 || *group_summaries > 0 {
        lines.push(format!(
            "In group chats, {} they sent {} replaced with \"message removed\", and {} covering \
             them {} deleted, to be written again.",
            counted(*group_messages, "message", "messages"),
            if *group_messages == 1 { "is" } else { "are" },
            counted(*group_summaries, "summary", "summaries"),
            if *group_summaries == 1 { "is" } else { "are" },
        ));
    }
    let memories: u64 = preview.memories.iter().map(|agent| agent.memories).sum();
    if memories > 0 {
        let agents: Vec<String> = preview
            .memories
            .iter()
            .map(|agent| format!("{} ({})", agent.agent, agent.memories))
            .collect();
        lines.push(format!(
            "{} private {}, in {}.",
            memories,
            if memories == 1 { "memory" } else { "memories" },
            agents.join(", ")
        ));
    }
    if preview.profile {
        lines.push(format!(
            "Their profile, USER.md, and everything else in the folder users/{}/.",
            preview.user.id
        ));
    }
    if *identities > 0 || *link_codes > 0 {
        let mut line = counted(*identities, "channel identity", "channel identities");
        if *link_codes > 0 {
            line.push_str(&format!(
                " and {}",
                counted(*link_codes, "pending link code", "pending link codes")
            ));
        }
        lines.push(format!("{line}."));
    }
    let mut sign_in = Vec::new();
    if *passwords > 0 {
        sign_in.push("their password".to_owned());
    }
    if *passkeys > 0 {
        sign_in.push(counted(*passkeys, "passkey", "passkeys"));
    }
    if *oidc_connections > 0 {
        sign_in.push(counted(
            *oidc_connections,
            "identity provider connection",
            "identity provider connections",
        ));
    }
    if *web_sessions > 0 {
        sign_in.push(counted(*web_sessions, "login", "logins"));
    }
    if *invites > 0 {
        sign_in.push(counted(
            *invites,
            "invite or recovery link",
            "invite or recovery links",
        ));
    }
    if !sign_in.is_empty() {
        lines.push(format!("Their way to sign in: {}.", sign_in.join(", ")));
    }
    if *runtime_jobs > 0 {
        lines.push(format!(
            "{} they scheduled, which {}.",
            counted(*runtime_jobs, "job", "jobs"),
            if *runtime_jobs == 1 { "stops" } else { "stop" }
        ));
    }
    if *inbox > 0 || *outbox > 0 {
        lines.push(format!(
            "{} and {} for them.",
            counted(
                *inbox,
                "received message record",
                "received message records"
            ),
            counted(
                *outbox,
                "message waiting to be sent",
                "messages waiting to be sent"
            ),
        ));
    }
    if preview.approvals > 0 {
        lines.push(format!(
            "{} waiting for their answer, which {} denied.",
            counted(preview.approvals as u64, "approval", "approvals"),
            if preview.approvals == 1 { "is" } else { "are" },
        ));
    }
    if *ssh_keys > 0 || preview.secrets > 0 {
        lines.push(format!(
            "{} and {} in the secret store under users/{}/, which include their SSH keys' \
             private keys.",
            counted(*ssh_keys, "SSH key", "SSH keys"),
            counted(preview.secrets as u64, "secret", "secrets"),
            preview.user.id
        ));
    }
    if !preview.workspaces.is_empty() {
        lines.push(format!(
            "{}, with everything in {}.",
            counted(preview.workspaces.len() as u64, "workspace", "workspaces"),
            if preview.workspaces.len() == 1 {
                "it"
            } else {
                "them"
            },
        ));
    }
    if lines.is_empty() {
        lines.push("They have no chats, memories, or other data.".to_owned());
    }
    lines
}

/// The workspaces a removal deletes, one sentence each: which environment holds it and whose it
/// is.
fn workspace_lines(preview: &ForgetPreview) -> Vec<String> {
    preview
        .workspaces
        .iter()
        .map(|workspace| match &workspace.owner {
            WorkspaceOwner::Conversation {
                agent,
                channel,
                conversation,
                ..
            } => format!(
                "Workspace {} in the environment {}, for {agent} in {channel}, conversation \
                 {conversation}.",
                workspace.id, workspace.environment
            ),
            WorkspaceOwner::Job { session } => format!(
                "Workspace {} in the environment {}, for a job run (session {session}).",
                workspace.id, workspace.environment
            ),
        })
        .collect()
}

/// What stays after a removal.
fn stays(preview: &ForgetPreview) -> Vec<String> {
    let mut lines = Vec::new();
    if preview.data.usage > 0 {
        lines.push(format!(
            "{} {}, with their token counts and without the user.",
            counted(preview.data.usage, "usage record", "usage records"),
            if preview.data.usage == 1 {
                "stays"
            } else {
                "stay"
            }
        ));
    }
    if preview.data.invites_made > 0 {
        lines.push(format!(
            "{} they made for others {}, without a creator.",
            counted(
                preview.data.invites_made,
                "invite or recovery link",
                "invite or recovery links"
            ),
            if preview.data.invites_made == 1 {
                "stays"
            } else {
                "stay"
            }
        ));
    }
    lines.push(
        "A shared memory they wrote stays for everyone, without an author. Pictures and files \
         only their messages used are deleted by Doozers' regular clean-up of unused files."
            .to_owned(),
    );
    lines
}

/// The confirmation page for `preview`, which says once why a removal is refused, whether it was
/// asked for just now or not.
fn confirm_page(user: &WebUser, preview: ForgetPreview) -> Response {
    http::render(&RemovePage {
        shell: Shell::new(user, Nav::Users),
        csrf: &user.csrf,
        id: preview.user.id.to_string(),
        username: preview.user.username.clone(),
        display_name: preview.user.display_name.clone(),
        deletes: deletes(&preview),
        workspaces: workspace_lines(&preview),
        stays: stays(&preview),
        busy: preview.busy,
        refusal: preview.refusal.map(crate::forget::Refusal::message),
    })
}

pub(super) fn no_such_user() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such user",
        "There is no user with that id. They may have been removed.",
    )
}

/// The page for a preview that couldn't be made: nothing was changed.
fn preview_failed(error: &ForgetError, id: &UserId) -> Response {
    error!(user = %id, %error, "reading what removing a user deletes failed");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Couldn't read the user's data",
        "Doozers couldn't read what removing the user deletes. Nothing was changed. The error is in \
         the log.",
    )
}

/// The page for a removal that failed after it began, for a reason that isn't the admin's to fix.
fn failed(error: &ForgetError, id: &UserId) -> Response {
    error!(user = %id, %error, "removing a user failed");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "User not completely removed",
        "Doozers couldn't finish removing the user. They are disabled and some of their data may \
         be left. Open the user's Remove page and press the button again to finish. The error is \
         in the log.",
    )
}

/// The user in a path.
pub(super) fn user_id(raw: &str) -> Option<UserId> {
    UserId::new(raw).ok()
}

/// `GET /admin/users/<id>/remove`: what removing the user deletes, and the button.
async fn remove_page(
    State(pages): State<UserPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = user_id(&id) else {
        return no_such_user();
    };
    match pages.forgetter.preview(&user.user, &id).await {
        Ok(preview) => confirm_page(&user, preview),
        Err(ForgetError::NoSuchUser(_)) => no_such_user(),
        Err(err) => preview_failed(&err, &id),
    }
}

/// A form with nothing but the CSRF token.
#[derive(Debug, Default, Deserialize)]
pub(super) struct CsrfForm {
    #[serde(default)]
    pub(super) csrf: Option<String>,
}

/// `POST /admin/users/<id>/remove` (CSRF-checked): forgets the user, then 303 to the list. A
/// removal that is refused (it is the admin's own account, the last active admin, or the user has
/// work running) shows the confirmation page again with why (409), and nothing changes.
async fn remove(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = user_id(&id) else {
        return no_such_user();
    };
    // Their web chats, read before anything is deleted: the live state of a chat goes with it
    // whether the removal finishes or fails after deleting, so a retry doesn't need them.
    let channel = &pages.app.inner.channel;
    let mut chats = Vec::new();
    for archived in [false, true] {
        match pages
            .app
            .inner
            .store
            .channel_conversations(channel.instance().as_str(), &id, archived, u32::MAX)
            .await
        {
            Ok(found) => chats.extend(found.into_iter().map(|c| c.conversation)),
            Err(err) => return preview_failed(&err.into(), &id),
        }
    }
    let result = pages.forgetter.forget(&user.user, &id).await;
    if result.as_ref().is_ok() || result.as_ref().is_err_and(|err| !err.is_refusal()) {
        // The ids are free to claim, and mustn't replay a history that is gone.
        for chat in &chats {
            channel.hubs.remove(chat);
        }
    }
    match result {
        Ok(forgotten) => Redirect::to(&format!(
            "/admin/users?removed={}",
            url::form_urlencoded::byte_serialize(forgotten.user.username.as_bytes())
                .collect::<String>()
        ))
        .into_response(),
        Err(ForgetError::NoSuchUser(_)) => no_such_user(),
        Err(err) if err.is_refusal() => {
            // The page says why, from what is stored now.
            match pages.forgetter.preview(&user.user, &id).await {
                Ok(preview) => {
                    let mut response = confirm_page(&user, preview);
                    if response.status().is_success() {
                        *response.status_mut() = StatusCode::CONFLICT;
                    }
                    response
                }
                Err(ForgetError::NoSuchUser(_)) => no_such_user(),
                Err(err) => preview_failed(&err, &id),
            }
        }
        Err(err) => failed(&err, &id),
    }
}

#[cfg(test)]
mod tests {
    use doozers_store::{WorkspaceRecord, WorkspaceState};
    use doozers_types::{AgentId, ConversationId, Timestamp, WorkspaceId};

    use super::*;
    use crate::forget::AgentMemories;
    use crate::users::User;

    fn workspace() -> WorkspaceRecord {
        WorkspaceRecord {
            id: WorkspaceId::new("ws-1").unwrap(),
            environment: "box".into(),
            owner: WorkspaceOwner::Conversation {
                agent: AgentId::new("jeeves").unwrap(),
                channel: "web".into(),
                conversation: ConversationId::new("c1").unwrap(),
                user: Some(UserId::new("mia").unwrap()),
            },
            options: serde_json::json!({}),
            description: None,
            working_directory: None,
            persistence: None,
            key_user: None,
            state: WorkspaceState::Active,
            created_at: Timestamp::UNIX_EPOCH,
            last_used_at: Timestamp::UNIX_EPOCH,
            deleted_at: None,
            delete_reason: None,
        }
    }

    /// A preview where every kind of data has `n` of it.
    fn preview(n: u64) -> ForgetPreview {
        ForgetPreview {
            user: User::member("mia", &[]),
            data: UserData {
                direct_sessions: n,
                conversations: n,
                job_sessions: n,
                messages: n,
                summaries: n,
                group_messages: n,
                group_summaries: n,
                inbox: n,
                outbox: n,
                usage: n,
                runtime_jobs: n,
                web_sessions: n,
                identities: n,
                link_codes: n,
                passwords: n,
                passkeys: n,
                oidc_connections: n,
                invites: n,
                invites_made: n,
                ssh_keys: n,
            },
            memories: vec![
                AgentMemories {
                    agent: AgentId::new("bunter").unwrap(),
                    memories: n,
                },
                AgentMemories {
                    agent: AgentId::new("jeeves").unwrap(),
                    memories: n,
                },
            ],
            secrets: n as usize,
            profile: true,
            workspaces: (0..n).map(|_| workspace()).collect(),
            approvals: n as usize,
            busy: false,
            refusal: None,
        }
    }

    #[test]
    fn one_of_everything_reads_in_the_singular() {
        assert_eq!(
            deletes(&preview(1)),
            [
                "1 chat in 1 session, with 1 message and 1 summary in them and in job runs.",
                "1 run of the jobs they scheduled.",
                "In group chats, 1 message they sent is replaced with \"message removed\", and 1 \
                 summary covering them is deleted, to be written again.",
                "2 private memories, in bunter (1), jeeves (1).",
                "Their profile, USER.md, and everything else in the folder users/mia/.",
                "1 channel identity and 1 pending link code.",
                "Their way to sign in: their password, 1 passkey, 1 identity provider connection, \
                 1 login, 1 invite or recovery link.",
                "1 job they scheduled, which stops.",
                "1 received message record and 1 message waiting to be sent for them.",
                "1 approval waiting for their answer, which is denied.",
                "1 SSH key and 1 secret in the secret store under users/mia/, which include their \
                 SSH keys' private keys.",
                "1 workspace, with everything in it.",
            ]
        );
        assert_eq!(
            stays(&preview(1)),
            [
                "1 usage record stays, with their token counts and without the user.",
                "1 invite or recovery link they made for others stays, without a creator.",
                SHARED,
            ]
        );
    }

    #[test]
    fn several_of_everything_reads_in_the_plural() {
        assert_eq!(
            deletes(&preview(2)),
            [
                "2 chats in 2 sessions, with 2 messages and 2 summaries in them and in job runs.",
                "2 runs of the jobs they scheduled.",
                "In group chats, 2 messages they sent are replaced with \"message removed\", and \
                 2 summaries covering them are deleted, to be written again.",
                "4 private memories, in bunter (2), jeeves (2).",
                "Their profile, USER.md, and everything else in the folder users/mia/.",
                "2 channel identities and 2 pending link codes.",
                "Their way to sign in: their password, 2 passkeys, 2 identity provider \
                 connections, 2 logins, 2 invite or recovery links.",
                "2 jobs they scheduled, which stop.",
                "2 received message records and 2 messages waiting to be sent for them.",
                "2 approvals waiting for their answer, which are denied.",
                "2 SSH keys and 2 secrets in the secret store under users/mia/, which include \
                 their SSH keys' private keys.",
                "2 workspaces, with everything in them.",
            ]
        );
        assert_eq!(
            workspace_lines(&preview(1)),
            ["Workspace ws-1 in the environment box, for jeeves in web, conversation c1."]
        );
    }

    #[test]
    fn a_user_with_nothing_says_so_and_job_runs_are_named_only_when_there_are_some() {
        let mut empty = preview(0);
        empty.profile = false;
        empty.memories.clear();
        assert_eq!(
            deletes(&empty),
            ["They have no chats, memories, or other data."]
        );
        assert_eq!(stays(&empty), [SHARED]);

        let mut chats = preview(0);
        chats.profile = false;
        chats.memories.clear();
        chats.data.direct_sessions = 2;
        chats.data.conversations = 1;
        chats.data.messages = 3;
        assert_eq!(
            deletes(&chats),
            ["1 chat in 2 sessions, with 3 messages and 0 summaries in them."]
        );
        // Messages with no chat of the user's belong to a job run.
        let mut runs = preview(0);
        runs.profile = false;
        runs.memories.clear();
        runs.data.messages = 1;
        assert_eq!(
            deletes(&runs),
            ["1 message and 0 summaries in their job runs."]
        );
    }

    const SHARED: &str = "A shared memory they wrote stays for everyone, without an author. \
        Pictures and files only their messages used are deleted by Doozers' regular clean-up of \
        unused files.";
}
