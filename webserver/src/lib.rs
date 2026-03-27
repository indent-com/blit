use axum::response::{Html, IntoResponse, Response};

/// Serve the font family list as JSON.
pub fn fonts_list_response() -> Response {
    let families = blit_fonts::list_font_families();
    let json = format!(
        "[{}]",
        families
            .iter()
            .map(|f| format!("\"{}\"", f.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",")
    );
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        json,
    )
        .into_response()
}

/// Serve a font's @font-face CSS by family name, or 404.
pub fn font_response(name: &str) -> Response {
    match blit_fonts::font_face_css(name) {
        Some(css) => (
            [
                (axum::http::header::CONTENT_TYPE, "text/css"),
                (
                    axum::http::header::CACHE_CONTROL,
                    "public, max-age=86400, immutable",
                ),
            ],
            css,
        )
            .into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "font not found").into_response(),
    }
}

/// Serve HTML with ETag support. Returns 304 if the client's `If-None-Match`
/// matches `etag`.
pub fn html_response(html: &'static str, etag: &str, if_none_match: Option<&[u8]>) -> Response {
    if let Some(inm) = if_none_match {
        if inm == etag.as_bytes() {
            return (
                axum::http::StatusCode::NOT_MODIFIED,
                [(axum::http::header::ETAG, etag)],
            )
                .into_response();
        }
    }
    ([(axum::http::header::ETAG, etag)], Html(html)).into_response()
}

/// Try to match a font route from a raw request path (any prefix).
/// Handles `/fonts`, `/vt/fonts`, `/font/Name`, `/vt/font/Name%20With%20Spaces`.
/// Returns `Some(response)` if the path matched a font route, `None` otherwise.
pub fn try_font_route(path: &str) -> Option<Response> {
    if path == "/fonts" || path.ends_with("/fonts") {
        return Some(fonts_list_response());
    }
    if let Some(raw) = path.rsplit_once("/font/").map(|(_, n)| n) {
        if !raw.contains('/') && !raw.is_empty() {
            let name = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
            return Some(font_response(&name));
        }
    }
    None
}

/// Compute an ETag string from HTML content.
pub fn html_etag(html: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut h);
    format!("\"blit-{:x}\"", h.finish())
}
