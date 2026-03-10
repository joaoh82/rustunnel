//! Serve the embedded dashboard SPA.
//!
//! All files under `assets/` are embedded at compile time via `rust-embed`.
//! Unknown paths fall back to `index.html` so client-side routing works.

use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

/// Embed everything under `src/dashboard/assets/` at compile time.
/// The directory is created empty here; real assets are added later.
#[derive(RustEmbed)]
#[folder = "src/dashboard/assets/"]
struct Assets;

pub fn router() -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/*path", get(static_handler))
}

async fn static_handler(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    serve_asset(&path)
}

async fn index_handler() -> impl IntoResponse {
    serve_asset("index.html")
}

fn serve_asset(path: &str) -> Response<Body> {
    // Strip leading slash.
    let path = path.trim_start_matches('/');

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let body = Body::from(content.data.into_owned());
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap_or_else(|_| {
                        HeaderValue::from_static("application/octet-stream")
                    }),
                )
                .body(body)
                .unwrap()
        }
        None => {
            // SPA fallback — serve index.html for any unknown path.
            match Assets::get("index.html") {
                Some(content) => {
                    let body = Body::from(content.data.into_owned());
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(body)
                        .unwrap()
                }
                None => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("dashboard assets not found"))
                    .unwrap(),
            }
        }
    }
}
