use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use std::sync::LazyLock;

const INDEX_HTML_BR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.br"));

static INDEX_HTML: LazyLock<String> = LazyLock::new(|| {
    let mut decompressed = Vec::new();
    let mut reader = brotli::Decompressor::new(INDEX_HTML_BR, 4096);
    std::io::Read::read_to_end(&mut reader, &mut decompressed).expect("brotli decompression");
    String::from_utf8(decompressed).expect("index.html is valid UTF-8")
});

static INDEX_ETAG: LazyLock<String> = LazyLock::new(|| html_etag(INDEX_HTML_BR));

pub fn index_html() -> &'static str {
    &INDEX_HTML
}

pub fn index_etag() -> &'static str {
    &INDEX_ETAG
}

pub fn index_response(accept_encoding: Option<&str>, if_none_match: Option<&[u8]>) -> Response {
    let etag = index_etag();
    if let Some(inm) = if_none_match {
        if inm == etag.as_bytes() {
            return (
                axum::http::StatusCode::NOT_MODIFIED,
                [(header::ETAG, etag)],
            )
                .into_response();
        }
    }
    if accepts_brotli(accept_encoding) {
        (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CONTENT_ENCODING, "br"),
                (header::ETAG, etag),
            ],
            INDEX_HTML_BR,
        )
            .into_response()
    } else {
        (
            [(header::ETAG, etag)],
            Html(index_html().to_owned()),
        )
            .into_response()
    }
}

fn accepts_brotli(accept_encoding: Option<&str>) -> bool {
    accept_encoding
        .map(|ae| {
            ae.split(',').any(|e| {
                let e = e.trim();
                if !e.starts_with("br") {
                    return false;
                }
                let rest = &e[2..];
                if rest.is_empty() {
                    return true;
                }
                if !rest.starts_with(';') {
                    return false;
                }
                !rest.contains("q=0") || rest.contains("q=0.")
            })
        })
        .unwrap_or(false)
}

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
    if let Some(origin) = origin {
        if let Ok(val) = origin.parse() {
            resp.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
        }
    }
}

/// Try to match a font route from a raw request path (any prefix).
/// Handles `/fonts`, `/vt/fonts`, `/font/Name`, `/vt/font/Name%20With%20Spaces`.
/// Returns `Some(response)` if the path matched a font route, `None` otherwise.
pub fn try_font_route(path: &str, cors_origin: Option<&str>) -> Option<Response> {
    if path == "/fonts" || path.ends_with("/fonts") {
        return Some(fonts_list_response(cors_origin));
    }
    if let Some(raw) = path.rsplit_once("/font-metrics/").map(|(_, n)| n) {
        if !raw.contains('/') && !raw.is_empty() {
            let name = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
            return Some(font_metrics_response(&name, cors_origin));
        }
    }
    if let Some(raw) = path.rsplit_once("/font/").map(|(_, n)| n) {
        if !raw.contains('/') && !raw.is_empty() {
            let name = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
            return Some(font_response(&name, cors_origin));
        }
    }
    None
}

/// Compute an ETag string from content bytes.
pub fn html_etag(content: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    format!("\"blit-{:x}\"", h.finish())
}
