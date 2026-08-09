use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
struct Asset;

pub async fn serve(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(f) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(f.data.into_owned()))
            .unwrap();
    }

    // Asset not found. Only fall back to the SPA shell for routes whose
    // last segment looks like a page (no file extension); explicit asset
    // requests that miss must 404 instead of being served as HTML, or a
    // missing manifest / service worker / icon would silently parse as
    // an HTML document on the client.
    if last_segment_has_extension(raw) {
        return (StatusCode::NOT_FOUND, format!("not found: /{raw}")).into_response();
    }

    match Asset::get("index.html") {
        Some(f) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(f.data.into_owned()))
            .unwrap(),
        None => (
            StatusCode::NOT_FOUND,
            "web/dist is empty — run `npm run build` in web/",
        )
            .into_response(),
    }
}

fn last_segment_has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .map(|seg| seg.contains('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::last_segment_has_extension;

    #[test]
    fn detects_extensions() {
        assert!(last_segment_has_extension("manifest.webmanifest"));
        assert!(last_segment_has_extension("static/icons/icon-192.png"));
        assert!(last_segment_has_extension("sw.js"));
    }

    #[test]
    fn page_routes_are_extensionless() {
        assert!(!last_segment_has_extension(""));
        assert!(!last_segment_has_extension("sessions"));
        assert!(!last_segment_has_extension("nested/route"));
    }
}
