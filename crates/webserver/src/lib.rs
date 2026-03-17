pub mod config;

use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

/// Serve the monospace font family list as JSON.
pub fn fonts_list_response(cors_origin: Option<&str>) -> Response {
    let families = blit_fonts::list_monospace_font_families();
    let json = format!(
        "[{}]",
        families
            .iter()
            .map(|f| format!("\"{}\"", f.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut resp = (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        json,
    )
        .into_response();
    add_cors(&mut resp, cors_origin);
    resp
}

/// Serve a font's @font-face CSS by family name, or 404.
pub fn font_response(name: &str, cors_origin: Option<&str>) -> Response {
    match blit_fonts::font_face_css(name) {
        Some(css) => {
            let mut resp = (
                [
                    (header::CONTENT_TYPE, "text/css"),
                    (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
                ],
                css,
            )
                .into_response();
            add_cors(&mut resp, cors_origin);
            resp
        }
        None => (axum::http::StatusCode::NOT_FOUND, "font not found").into_response(),
    }
}

/// Serve font metrics (advance ratio) as JSON.
pub fn font_metrics_response(name: &str, cors_origin: Option<&str>) -> Response {
    match blit_fonts::font_advance_ratio(name) {
        Some(ratio) => {
            let json = format!("{{\"advanceRatio\":{}}}", ratio);
            let mut resp = (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
                ],
                json,
            )
                .into_response();
            add_cors(&mut resp, cors_origin);
            resp
        }
        None => (axum::http::StatusCode::NOT_FOUND, "font not found").into_response(),
    }
}

fn add_cors(resp: &mut Response, origin: Option<&str>) {
    if let Some(origin) = origin
        && let Ok(val) = origin.parse()
    {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
    }
}

/// Serve brotli-compressed HTML with ETag support. If the client accepts `br`
/// encoding, the raw compressed bytes are sent; otherwise they are decompressed.
/// Returns 304 if the client's `If-None-Match` matches `etag`.
pub fn html_response(
    html_br: &'static [u8],
    etag: &str,
    if_none_match: Option<&[u8]>,
    accept_encoding: Option<&str>,
) -> Response {
    if let Some(inm) = if_none_match
        && inm == etag.as_bytes()
    {
        return (
            axum::http::StatusCode::NOT_MODIFIED,
            [(axum::http::header::ETAG, etag)],
        )
            .into_response();
    }
    let accepts_br = accept_encoding
        .map(|ae| ae.split(',').any(|p| p.trim().starts_with("br")))
        .unwrap_or(false);
    if accepts_br {
        (
            [
                (header::ETAG, etag.to_owned()),
                (header::CONTENT_ENCODING, "br".to_owned()),
                (header::CONTENT_TYPE, "text/html".to_owned()),
            ],
            html_br,
        )
            .into_response()
    } else {
        let mut decompressed = Vec::new();
        let _ = brotli::BrotliDecompress(&mut std::io::Cursor::new(html_br), &mut decompressed);
        (
            [(header::ETAG, etag.to_owned())],
            Html(String::from_utf8_lossy(&decompressed).into_owned()),
        )
            .into_response()
    }
}

/// Try to match a font route from a raw request path (any prefix).
/// Handles `/fonts`, `/vt/fonts`, `/font/Name`, `/vt/font/Name%20With%20Spaces`.
/// Returns `Some(response)` if the path matched a font route, `None` otherwise.
pub fn try_font_route(path: &str, cors_origin: Option<&str>) -> Option<Response> {
    if path == "/fonts" || path.ends_with("/fonts") {
        return Some(fonts_list_response(cors_origin));
    }
    if let Some(raw) = path.rsplit_once("/font-metrics/").map(|(_, n)| n)
        && !raw.contains('/')
        && !raw.is_empty()
    {
        let name = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
        return Some(font_metrics_response(&name, cors_origin));
    }
    if let Some(raw) = path.rsplit_once("/font/").map(|(_, n)| n)
        && !raw.contains('/')
        && !raw.is_empty()
    {
        let name = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
        return Some(font_response(&name, cors_origin));
    }
    None
}

/// Compute an ETag string from content bytes.
pub fn html_etag(data: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    format!("\"blit-{:x}\"", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    // ── html_etag ──

    #[test]
    fn etag_deterministic() {
        let a = html_etag(b"<html>hello</html>");
        let b = html_etag(b"<html>hello</html>");
        assert_eq!(a, b);
    }

    #[test]
    fn etag_different_for_different_content() {
        let a = html_etag(b"aaa");
        let b = html_etag(b"bbb");
        assert_ne!(a, b);
    }

    #[test]
    fn etag_format() {
        let tag = html_etag(b"test");
        assert!(
            tag.starts_with("\"blit-"),
            "expected quoted blit- prefix, got {tag}"
        );
        assert!(tag.ends_with('"'));
    }

    // ── html_response ──

    #[tokio::test]
    async fn html_response_200_without_etag_match() {
        let etag = html_etag(b"hello");
        let resp = html_response(b"hello", &etag, None, None);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
    }

    #[tokio::test]
    async fn html_response_304_with_matching_etag() {
        let etag = html_etag(b"hello");
        let resp = html_response(b"hello", &etag, Some(etag.as_bytes()), None);
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn html_response_200_with_mismatched_etag() {
        let etag = html_etag(b"hello");
        let resp = html_response(b"hello", &etag, Some(b"\"wrong\""), None);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── try_font_route ──

    #[test]
    fn font_route_fonts_bare() {
        assert!(try_font_route("/fonts", None).is_some());
    }

    #[test]
    fn font_route_fonts_prefixed() {
        assert!(try_font_route("/vt/fonts", None).is_some());
    }

    #[test]
    fn font_route_font_name() {
        let resp = try_font_route("/font/Menlo", None);
        assert!(resp.is_some());
    }

    #[test]
    fn font_route_font_metrics() {
        let resp = try_font_route("/font-metrics/Menlo", None);
        assert!(resp.is_some());
    }

    #[test]
    fn font_route_no_match() {
        assert!(try_font_route("/api/sessions", None).is_none());
        assert!(try_font_route("/", None).is_none());
    }

    #[test]
    fn font_route_rejects_empty_name() {
        assert!(try_font_route("/font/", None).is_none());
        assert!(try_font_route("/font-metrics/", None).is_none());
    }

    #[test]
    fn font_route_rejects_nested_path() {
        assert!(try_font_route("/font/a/b", None).is_none());
    }
}
