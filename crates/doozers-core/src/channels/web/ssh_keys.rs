//! The SSH keys pages: `/ssh-keys`, where each user generates, uploads, renames, and deletes their
//! own SSH keys and copies their public keys, and `/admin/ssh-keys`, where an admin sees every
//! user's keys and deletes one. The service behind them is [`SshKeys`]; the design is in
//! `Docs/technical/ssh.md`.
//!
//! Like the other pages: behind the login, every answer `no-store`, every change a CSRF-checked
//! form post, and every form working without JavaScript. No page, link, or header ever carries a
//! private key: a key's page shows its public half and its fingerprint. A form that fails shows
//! again with the reason and what was typed except the passphrase and the key text, which are
//! never sent back. A key id that isn't the user's own answers 404, as one that doesn't exist.
//!
//! The upload is a `multipart/form-data` post (a pasted key and an optional file, the passphrase,
//! a label), capped at [`UPLOAD_BODY_LIMIT`].

use askama::Template;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::SshKeyId;
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, no_store};
use super::shell::{Nav, Shell};
use super::toolsets::when;
use crate::http::{self, RouterBuilder};
use crate::ssh::{KeyType, MAX_UPLOAD_BYTES, SshKey, SshKeys, SshKeysError};

/// The most an upload form may send: the key's limit, with room for the other fields and the
/// multipart framing. A request over it is answered 413 before the key is read.
const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_BYTES * 2 + 16 * 1024;

#[derive(Clone)]
struct SshKeyPages {
    keys: SshKeys,
}

impl WebApp {
    /// Adds the SSH keys pages and the admin page to `builder`, behind the same
    /// authentication middleware as the chat pages. Every answer is `no-store`.
    pub fn install_ssh_keys(&self, builder: RouterBuilder, keys: SshKeys) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/ssh-keys", get(list_page))
                .route("/ssh-keys/generate", post(generate))
                .route(
                    "/ssh-keys/upload",
                    post(upload).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
                )
                .route("/ssh-keys/{id}/rename", get(rename_page).post(rename))
                .route("/ssh-keys/{id}/delete", get(delete_page).post(delete))
                .route("/admin/ssh-keys", get(admin_page))
                .route(
                    "/admin/ssh-keys/{id}/delete",
                    get(admin_delete_page).post(admin_delete),
                )
                .with_state(SshKeyPages { keys })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A key on the pages.
struct KeyRow {
    id: i64,
    user: String,
    label: String,
    type_label: String,
    fingerprint: String,
    /// `<algorithm> <base64> <label>`.
    authorized_key: String,
    created: String,
    last_used: String,
}

impl From<SshKey> for KeyRow {
    fn from(key: SshKey) -> Self {
        Self {
            id: key.id.get(),
            authorized_key: key.authorized_key(),
            created: when(key.created_at),
            last_used: key.last_used_at.map_or_else(|| "never".to_owned(), when),
            user: key.user.to_string(),
            label: key.label,
            type_label: key.type_label,
            fingerprint: key.fingerprint,
        }
    }
}

/// One type in the generate form's list.
struct KindOption {
    id: &'static str,
    label: &'static str,
}

/// What the SSH keys page shows besides the keys.
#[derive(Default)]
struct ListState {
    notice: Option<String>,
    error: Option<String>,
    generate_type: String,
    generate_label: String,
    upload_label: String,
}

#[derive(Template)]
#[template(path = "web/ssh_keys.html")]
struct ListPage<'a> {
    shell: Shell,
    csrf: &'a str,
    keys: Vec<KeyRow>,
    kinds: Vec<KindOption>,
    default_label: String,
    notice: Option<String>,
    error: Option<String>,
    generate_type: String,
    generate_label: String,
    upload_label: String,
}

/// `?notice=<what happened>`, after a change.
#[derive(Deserialize)]
struct Notice {
    notice: Option<String>,
}

/// The sentence for `?notice=`: only the values the pages redirect with.
fn notice_text(notice: Option<&str>) -> Option<String> {
    let text = match notice? {
        "generated" => "Generated the key.",
        "uploaded" => "Added the key.",
        "renamed" => "Renamed the key.",
        "deleted" => "Deleted the key.",
        _ => return None,
    };
    Some(text.to_owned())
}

/// The page for a call that failed for a reason the user can't fix by editing the form.
fn failed(err: &SshKeysError) -> Response {
    match err {
        SshKeysError::NotFound => no_such_key(),
        SshKeysError::Forbidden => forbidden(),
        SshKeysError::UserCantKeepKeys(_) => {
            error_page(StatusCode::CONFLICT, "No SSH keys", &err.to_string())
        }
        err => {
            error!(error = %err, "using the SSH keys failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Doozers couldn't use your SSH keys. The error is in its log.",
            )
        }
    }
}

fn no_such_key() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such SSH key",
        "You have no SSH key with that id. It may have been deleted.",
    )
}

/// The key id in a path, if it is one.
fn key_id(id: &str) -> Option<SshKeyId> {
    id.parse().ok().map(SshKeyId::new)
}

/// Renders the SSH keys page for `user` with `state`, answering `status`.
async fn render_list(
    pages: &SshKeyPages,
    user: &WebUser,
    status: StatusCode,
    state: ListState,
) -> Response {
    let keys = match pages.keys.list(&user.user).await {
        Ok(keys) => keys,
        Err(err) => return failed(&err),
    };
    let mut response = http::render(&ListPage {
        shell: Shell::new(user, Nav::SshKeys),
        csrf: &user.csrf,
        keys: keys.into_iter().map(KeyRow::from).collect(),
        kinds: KeyType::ALL
            .into_iter()
            .map(|kind| KindOption {
                id: kind.id(),
                label: kind.label(),
            })
            .collect(),
        default_label: format!("{}@doozers", user.user),
        notice: state.notice,
        error: state.error,
        generate_type: if state.generate_type.is_empty() {
            KeyType::default().id().to_owned()
        } else {
            state.generate_type
        },
        generate_label: state.generate_label,
        upload_label: state.upload_label,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /ssh-keys`: the user's keys, with the forms to generate and upload one.
async fn list_page(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    let state = ListState {
        notice: notice_text(notice.notice.as_deref()),
        ..ListState::default()
    };
    render_list(&pages, &user, StatusCode::OK, state).await
}

/// How a failed generate or upload is shown: on the page again, with the reason.
fn form_error(err: &SshKeysError) -> Option<(StatusCode, String)> {
    match err {
        SshKeysError::InvalidLabel(_) | SshKeysError::Upload(_) => {
            Some((StatusCode::BAD_REQUEST, err.to_string()))
        }
        SshKeysError::Duplicate { .. } => Some((StatusCode::CONFLICT, err.to_string())),
        _ => None,
    }
}

#[derive(Debug, Default, Deserialize)]
struct GenerateForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    label: String,
}

/// `POST /ssh-keys/generate` (CSRF-checked): generates a key of the chosen type, then 303 to the
/// page.
async fn generate(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<GenerateForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let state = |error: String| ListState {
        error: Some(error),
        generate_type: form.kind.clone(),
        generate_label: form.label.clone(),
        ..ListState::default()
    };
    let Some(kind) = KeyType::from_id(&form.kind) else {
        let state = state("Choose a key type.".to_owned());
        return render_list(&pages, &user, StatusCode::BAD_REQUEST, state).await;
    };
    match pages
        .keys
        .generate(&user.user, kind, Some(&form.label))
        .await
    {
        Ok(_) => Redirect::to("/ssh-keys?notice=generated").into_response(),
        Err(err) => match form_error(&err) {
            Some((status, message)) => render_list(&pages, &user, status, state(message)).await,
            None => failed(&err),
        },
    }
}

/// The fields of the upload form.
#[derive(Default)]
struct UploadForm {
    csrf: Option<String>,
    key_text: Vec<u8>,
    key_file: Vec<u8>,
    passphrase: String,
    label: String,
}

/// Reads the upload form's fields. Unknown fields are ignored.
async fn read_upload(
    multipart: &mut Multipart,
) -> Result<UploadForm, axum::extract::multipart::MultipartError> {
    let mut form = UploadForm::default();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "csrf" => form.csrf = Some(field.text().await?),
            "label" => form.label = field.text().await?,
            "passphrase" => form.passphrase = field.text().await?,
            "key_text" => form.key_text = field.bytes().await?.to_vec(),
            "key_file" => form.key_file = field.bytes().await?.to_vec(),
            _ => {}
        }
    }
    Ok(form)
}

/// `POST /ssh-keys/upload` (CSRF-checked, `multipart/form-data`): adds the private key in the
/// chosen file, or else the pasted text, then 303 to the page. A form that fails is shown again
/// without the key or the passphrase.
async fn upload(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let form = match read_upload(&mut multipart).await {
        Ok(form) => form,
        Err(err) => {
            let (status, message) = if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "That upload is too large. A private key is a few kilobytes, and the limit is {} KiB.",
                        MAX_UPLOAD_BYTES / 1024
                    ),
                )
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    "Doozers couldn't read the form. Try again.".to_owned(),
                )
            };
            let state = ListState {
                error: Some(message),
                ..ListState::default()
            };
            return render_list(&pages, &user, status, state).await;
        }
    };
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let key = if form.key_file.is_empty() {
        &form.key_text
    } else {
        &form.key_file
    };
    let passphrase = Some(form.passphrase.as_str()).filter(|p| !p.is_empty());
    let label = Some(form.label.as_str());
    match pages.keys.upload(&user.user, key, passphrase, label).await {
        Ok(_) => Redirect::to("/ssh-keys?notice=uploaded").into_response(),
        Err(err) => match form_error(&err) {
            Some((status, message)) => {
                let state = ListState {
                    error: Some(message),
                    upload_label: form.label,
                    ..ListState::default()
                };
                render_list(&pages, &user, status, state).await
            }
            None => failed(&err),
        },
    }
}

#[derive(Template)]
#[template(path = "web/ssh_key_rename.html")]
struct RenamePage<'a> {
    shell: Shell,
    csrf: &'a str,
    key: KeyRow,
    label: String,
    error: Option<String>,
}

fn render_rename(user: &WebUser, key: SshKey, label: String, error: Option<String>) -> Response {
    let status = if error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let mut response = http::render(&RenamePage {
        shell: Shell::new(user, Nav::SshKeys),
        csrf: &user.csrf,
        key: key.into(),
        label,
        error,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /ssh-keys/<id>/rename`: the label form.
async fn rename_page(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = key_id(&id) else {
        return no_such_key();
    };
    match pages.keys.get(&user.user, id).await {
        Ok(key) => {
            let label = key.label.clone();
            render_rename(&user, key, label, None)
        }
        Err(err) => failed(&err),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RenameForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    label: String,
}

/// `POST /ssh-keys/<id>/rename` (CSRF-checked): sets the label, then 303 to the page.
async fn rename(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RenameForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = key_id(&id) else {
        return no_such_key();
    };
    match pages.keys.rename(&user.user, id, &form.label).await {
        Ok(_) => Redirect::to("/ssh-keys?notice=renamed").into_response(),
        Err(err @ SshKeysError::InvalidLabel(_)) => match pages.keys.get(&user.user, id).await {
            Ok(key) => render_rename(&user, key, form.label, Some(err.to_string())),
            Err(err) => failed(&err),
        },
        Err(err) => failed(&err),
    }
}

#[derive(Template)]
#[template(path = "web/ssh_key_delete.html")]
struct DeletePage<'a> {
    shell: Shell,
    csrf: &'a str,
    key: KeyRow,
    /// Whether an admin is deleting someone else's key from the admin page.
    admin: bool,
    /// Where the form posts.
    action: String,
    /// Where Cancel and the back link go.
    back: String,
}

fn render_delete(user: &WebUser, key: SshKey, admin: bool) -> Response {
    let (action, back, nav) = if admin {
        (
            format!("/admin/ssh-keys/{}/delete", key.id),
            format!("/admin/ssh-keys#key-{}", key.id),
            Nav::AdminSshKeys,
        )
    } else {
        (
            format!("/ssh-keys/{}/delete", key.id),
            format!("/ssh-keys#key-{}", key.id),
            Nav::SshKeys,
        )
    };
    http::render(&DeletePage {
        shell: Shell::new(user, nav),
        csrf: &user.csrf,
        key: key.into(),
        admin,
        action,
        back,
    })
}

/// `GET /ssh-keys/<id>/delete`: what deleting does, and the button.
async fn delete_page(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = key_id(&id) else {
        return no_such_key();
    };
    match pages.keys.get(&user.user, id).await {
        Ok(key) => render_delete(&user, key, false),
        Err(err) => failed(&err),
    }
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
}

/// `POST /ssh-keys/<id>/delete` (CSRF-checked): deletes the user's key, then 303 to the page.
async fn delete(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = key_id(&id) else {
        return no_such_key();
    };
    match pages.keys.delete(&user.user, id).await {
        Ok(()) => Redirect::to("/ssh-keys?notice=deleted").into_response(),
        Err(err) => failed(&err),
    }
}

#[derive(Template)]
#[template(path = "web/admin_ssh_keys.html")]
struct AdminPage {
    shell: Shell,
    keys: Vec<KeyRow>,
    notice: Option<String>,
}

/// `GET /admin/ssh-keys`: every user's keys (details only), each with Delete. Admins only.
async fn admin_page(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    match pages.keys.all_keys(&user.user).await {
        Ok(keys) => http::render(&AdminPage {
            shell: Shell::new(&user, Nav::AdminSshKeys),
            keys: keys.into_iter().map(KeyRow::from).collect(),
            notice: notice_text(notice.notice.as_deref()),
        }),
        Err(err) => failed(&err),
    }
}

/// The page for an admin's key id that doesn't exist.
fn admin_no_such_key() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such SSH key",
        "There is no SSH key with that id. It may have been deleted.",
    )
}

/// `GET /admin/ssh-keys/<id>/delete`: the confirmation. Admins only.
async fn admin_delete_page(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = key_id(&id) else {
        return admin_no_such_key();
    };
    match pages.keys.any_key(&user.user, id).await {
        Ok(key) => render_delete(&user, key, true),
        Err(SshKeysError::NotFound) => admin_no_such_key(),
        Err(err) => failed(&err),
    }
}

/// `POST /admin/ssh-keys/<id>/delete` (CSRF-checked): deletes any user's key, then 303 to the
/// admin list. Admins only.
async fn admin_delete(
    State(pages): State<SshKeyPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = key_id(&id) else {
        return admin_no_such_key();
    };
    match pages.keys.delete_any(&user.user, id).await {
        Ok(_) => Redirect::to("/admin/ssh-keys?notice=deleted").into_response(),
        Err(SshKeysError::NotFound) => admin_no_such_key(),
        Err(err) => failed(&err),
    }
}
