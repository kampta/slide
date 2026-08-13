use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self' data:; worker-src 'self'; manifest-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";
const X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
const CONTENT_SECURITY_POLICY_HEADER: HeaderName =
    HeaderName::from_static("content-security-policy");

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
struct Asset;

pub async fn serve(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(f) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return static_response(path, mime.as_ref(), f.data.into_owned());
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
        Some(f) => static_response("index.html", "text/html", f.data.into_owned()),
        None => (
            StatusCode::NOT_FOUND,
            "web/dist is empty — run `npm run build` in web/",
        )
            .into_response(),
    }
}

fn apply_static_headers(response: &mut Response, path: &str) {
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY_HEADER,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(path)),
    );
}

fn static_response(path: &str, content_type: &str, data: Vec<u8>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(data))
        .unwrap();
    apply_static_headers(&mut response, path);
    response
}

fn cache_control(path: &str) -> &'static str {
    if path == "index.html" {
        "no-store"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
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
    use super::{
        cache_control, last_segment_has_extension, static_response, CONTENT_SECURITY_POLICY,
    };
    use axum::http::header;

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

    #[test]
    fn cache_policy_matches_asset_lifecycle() {
        assert_eq!(cache_control("index.html"), "no-store");
        assert_eq!(
            cache_control("assets/index-a1b2c3.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
    }

    #[test]
    fn csp_allows_the_spa_transport_but_denies_embedding() {
        assert!(CONTENT_SECURITY_POLICY.contains("connect-src 'self'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("connect-src 'self' ws:"));
        assert!(CONTENT_SECURITY_POLICY.contains("frame-ancestors 'none'"));
        assert!(CONTENT_SECURITY_POLICY.contains("object-src 'none'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("script-src 'self' 'unsafe-inline'"));
    }

    #[test]
    fn static_responses_receive_security_headers() {
        let response = static_response("index.html", "text/html", Vec::new());
        let headers = response.headers();
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["content-security-policy"], CONTENT_SECURITY_POLICY);
    }
}
