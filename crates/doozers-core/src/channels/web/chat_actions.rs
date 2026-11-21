//! Archiving and deleting chats: the buttons on a chat page. Every post is CSRF-checked and
//! answers 303.
//!
//! Archiving only moves a chat between the chat list and the archived list
//! ([`Store::archive_conversation`](doozers_store::Store::archive_conversation)), so it is never
//! refused and can be undone: Unarchive, or posting a message in the chat, puts it back. Nothing
//! else changes; the chat's workspace is left to the environment's sweep like any idle one.
//!
//! Deleting removes the conversation from the database
//! ([`Store::delete_conversation`](doozers_store::Store::delete_conversation)) and then deletes the
//! workspace of each agent it was with ([`DeleteReason::ChatDeleted`]). It is refused while the
//! chat is being worked on: a post the core hasn't taken, a turn running or a holder in one of
//! its sessions ([`ChatActivity`]), or an inbound message waiting for or in a turn (the store
//! checks that in the transaction that deletes). A message posted in the instant between the
//! check and the delete starts the chat again on its own.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use doozers_store::{ConversationDeletion, DeleteReason, WorkspaceOwner};
use doozers_types::ConversationId;
use serde::Deserialize;

use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, chat_id, error_page, internal_error, not_found, own_chat};
use crate::http::RouterBuilder;
use crate::sessions::Sessions;
use crate::workspaces::Workspaces;

/// Tells whether a conversation is being worked on. The daemon's is the session manager.
#[async_trait]
pub trait ChatActivity: Send + Sync + fmt::Debug {
    /// Whether a turn is running in a session of the conversation `conversation` on channel
    /// instance `channel`, someone holds one, or messages are queued for one.
    async fn is_busy(&self, channel: &str, conversation: &ConversationId) -> bool;
}

#[async_trait]
impl ChatActivity for Sessions {
    async fn is_busy(&self, channel: &str, conversation: &ConversationId) -> bool {
        self.conversation_busy(channel, conversation).await
    }
}

/// The delete route's state.
#[derive(Clone)]
struct Deletion {
    app: WebApp,
    activity: Arc<dyn ChatActivity>,
    workspaces: Workspaces,
}

impl WebApp {
    /// Adds `POST /chats/<id>/delete` to `builder`, behind the same authentication middleware as
    /// the chat pages. A chat isn't deleted while `activity` says it's being worked on; its
    /// workspaces are deleted through `workspaces`.
    pub fn install_chat_deletion(
        &self,
        builder: RouterBuilder,
        activity: Arc<dyn ChatActivity>,
        workspaces: Workspaces,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/chats/{chat}/delete", post(delete))
                .with_state(Deletion {
                    app: self.clone(),
                    activity,
                    workspaces,
                })
                .route_layer(auth),
        )
    }
}

/// The archive and unarchive routes, part of the chat pages.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/chats/{chat}/archive", post(archive))
        .route("/chats/{chat}/unarchive", post(unarchive))
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
}

pub(super) fn no_such_chat() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such chat",
        "There's no chat with that address. It may have been deleted.",
    )
}

/// `POST /chats/<id>/archive` (CSRF-checked): moves the user's chat to the archived list, then
/// 303 to the chat list. 404 for a chat with nothing stored.
async fn archive(
    State(app): State<WebApp>,
    user: WebUser,
    Path(chat): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    if let Err(response) = own_chat(&app, &user, &chat).await {
        return *response;
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let channel = app.inner.channel.instance().as_str();
    let now = app.inner.clock.now();
    match app
        .inner
        .store
        .archive_conversation(channel, &chat, &user.user, now)
        .await
    {
        Ok(true) => Redirect::to("/").into_response(),
        Ok(false) => no_such_chat(),
        Err(err) => internal_error("cannot archive a web chat", &err),
    }
}

/// `POST /chats/<id>/unarchive` (CSRF-checked): moves the chat back to the chat list, then 303
/// to the chat.
async fn unarchive(
    State(app): State<WebApp>,
    user: WebUser,
    Path(chat): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    if let Err(response) = own_chat(&app, &user, &chat).await {
        return *response;
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let channel = app.inner.channel.instance().as_str();
    match app
        .inner
        .store
        .unarchive_conversation(channel, &chat, &user.user)
        .await
    {
        Ok(_) => Redirect::to(&format!("/chats/{chat}")).into_response(),
        Err(err) => internal_error("cannot unarchive a web chat", &err),
    }
}

/// `POST /chats/<id>/delete` (CSRF-checked): deletes the user's chat and its workspaces, then
/// 303 to the list it was in. 409 while it's being worked on, 404 for a chat with nothing
/// stored.
async fn delete(
    State(deletion): State<Deletion>,
    user: WebUser,
    Path(chat): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    if let Err(response) = own_chat(&deletion.app, &user, &chat).await {
        return *response;
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let app = &deletion.app.inner;
    let channel = app.channel.instance().as_str();
    if !app.channel.unacked(&chat).is_empty() || deletion.activity.is_busy(channel, &chat).await {
        return busy();
    }
    let (agents, archived) = match app
        .store
        .delete_conversation(channel, &chat, &user.user)
        .await
    {
        Ok(ConversationDeletion::Deleted { agents, archived }) => {
            // Its live state goes with it: the id is free to claim, and mustn't replay the history.
            deletion.app.inner.channel.hubs.remove(&chat);
            (agents, archived)
        }
        Ok(ConversationDeletion::Busy) => return busy(),
        Ok(ConversationDeletion::NotFound) => return no_such_chat(),
        Err(err) => return internal_error("cannot delete a web chat", &err),
    };
    for agent in agents {
        let owner = WorkspaceOwner::Conversation {
            agent,
            channel: channel.to_owned(),
            conversation: chat.clone(),
            // A chat belongs to the user who started it, and so does its workspace.
            user: Some(user.user.clone()),
        };
        if let Err(err) = deletion
            .workspaces
            .delete(&owner, DeleteReason::ChatDeleted)
            .await
        {
            return internal_error("cannot delete a deleted web chat's workspace", &err);
        }
    }
    Redirect::to(if archived { "/chats/archived" } else { "/" }).into_response()
}

/// 409: the chat is being worked on.
fn busy() -> Response {
    error_page(
        StatusCode::CONFLICT,
        "Chat not deleted",
        "The agent is working in this chat. Wait for it to finish, or send /stop in the chat, \
         then try again.",
    )
}
