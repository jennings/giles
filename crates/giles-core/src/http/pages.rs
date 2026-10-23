//! Server-rendered pages. Every page template extends `templates/layout.html`, which links the
//! stylesheet and the `htmx` scripts.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use tracing::error;

/// Renders `page` as an HTML response, or logs the error and answers 500.
pub fn render<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            error!(error = %err, "cannot render a page");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    version: &'static str,
}

/// `GET /`: the home page.
pub(super) async fn index() -> Response {
    render(&IndexPage {
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundPage;

/// The fallback for any path without a route.
pub(super) async fn not_found() -> Response {
    let mut response = render(&NotFoundPage);
    if response.status().is_success() {
        *response.status_mut() = StatusCode::NOT_FOUND;
    }
    response
}
