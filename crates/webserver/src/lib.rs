pub mod config;
pub mod passphrase;

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

/// Whether a client said it can take brotli.
///
/// `br;q=0` is a refusal, not an offer — the same header syntax says both.
fn accepts_br(accept_encoding: Option<&str>) -> bool {
    let Some(header) = accept_encoding else {
        return false;
    };
    header.split(',').any(|part| {
        let mut params = part.split(';').map(str::trim);
        if params.next() != Some("br") {
            return false;
        }
        // Any q but zero is an offer: `q=0.1` is "if you must", `q=0` is "no".
        !params.any(|p| {
            p.strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        })
    })
}

/// Compression settings for face CSS, measured on a real 25.4 MB PragmataPro
/// Mono stylesheet (four faces, base64 inside the CSS):
///
/// | quality | window | result                    |
/// |---------|--------|---------------------------|
/// | 4       | 22     | 10.6 MB in 429 ms         |
/// | 4       | 24     |  8.7 MB in 401 ms         |
/// | 5       | 24     | 10.3 MB in 870 ms         |
/// | 9       | 24     |  7.4 MB in 5.1 s          |
///
/// So quality 4 with the largest standard window: everything above it costs
/// multiples of the CPU, and 5 and 6 are *worse* on both counts here — at
/// quality 5 the encoder switches strategy and stops using the whole window,
/// which is where the wins on a payload this repetitive come from.
///
/// 24 is the ceiling for plain `Content-Encoding: br`; a larger one needs the
/// large-window extension, which a browser will not decode.
const FONT_CSS_BROTLI_QUALITY: i32 = 4;
const FONT_CSS_BROTLI_WINDOW: i32 = 24;

/// Serve a font's @font-face CSS by family name, or 404.
///
/// Compressed when the client can take it, because the face arrives inlined as
/// base64 — a real family is tens of megabytes of it, a third of which is the
/// encoding rather than the font, and what is left is repetitive enough that
/// the whole thing goes out at roughly a third of its size.
pub fn font_response(
    name: &str,
    cors_origin: Option<&str>,
    accept_encoding: Option<&str>,
) -> Response {
    let Some(css) = blit_fonts::font_face_css(name) else {
        return (axum::http::StatusCode::NOT_FOUND, "font not found").into_response();
    };
    let headers = [
        (header::CONTENT_TYPE, "text/css"),
        (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
    ];
    let mut resp = match compress_br(css.as_bytes(), accept_encoding) {
        Some(compressed) => {
            let mut resp = (headers, compressed).into_response();
            resp.headers_mut().insert(
                header::CONTENT_ENCODING,
                header::HeaderValue::from_static("br"),
            );
            resp
        }
        None => (headers, css).into_response(),
    };
    add_cors(&mut resp, cors_origin);
    resp
}

/// Brotli-compress a body, or `None` when the client did not offer `br` —
/// spending the CPU on bytes the client would then have to reject is worse
/// than sending it raw.
fn compress_br(body: &[u8], accept_encoding: Option<&str>) -> Option<Vec<u8>> {
    if !accepts_br(accept_encoding) {
        return None;
    }
    let mut out = Vec::new();
    brotli::BrotliCompress(
        &mut std::io::Cursor::new(body),
        &mut out,
        &brotli::enc::BrotliEncoderParams {
            quality: FONT_CSS_BROTLI_QUALITY,
            lgwin: FONT_CSS_BROTLI_WINDOW,
            size_hint: body.len(),
            ..Default::default()
        },
    )
    .ok()?;
    Some(out)
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
    if accepts_br(accept_encoding) {
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

/// The reserved preview path prefix, mirroring `PREVIEW_PREFIX` in
/// js/core/src/preview.ts. Kept here so the gateway and the worker cannot
/// disagree about which paths belong to a preview.
pub const PREVIEW_PATH_PREFIX: &str = "/x/";

/// `Service-Worker-Allowed`, which widens a worker's scope from the directory
/// it was served from to the whole origin. Without it a worker at `/sw.js`
/// could only claim `/`-rooted requests by accident of path.
const SERVICE_WORKER_ALLOWED: axum::http::HeaderName =
    axum::http::HeaderName::from_static("service-worker-allowed");

/// Serve a brotli-compressed JavaScript asset, decompressing when the client
/// cannot take `br`.
///
/// Used for the preview service worker (docs/design/net.md § Client: service
/// worker), which needs three things a route for it must not get wrong: a
/// JavaScript content type, `Service-Worker-Allowed: /` so its scope covers
/// the whole origin rather than its own directory, and `no-cache` so a stale
/// worker cannot outlive an upgrade of the app it serves.
pub fn service_worker_response(
    js_br: &'static [u8],
    etag: &str,
    if_none_match: Option<&[u8]>,
    accept_encoding: Option<&str>,
) -> Response {
    if let Some(inm) = if_none_match
        && inm == etag.as_bytes()
    {
        return (
            axum::http::StatusCode::NOT_MODIFIED,
            [
                (axum::http::header::ETAG, etag),
                (SERVICE_WORKER_ALLOWED, "/"),
            ],
        )
            .into_response();
    }
    let common = [
        (header::ETAG, etag.to_owned()),
        (
            header::CONTENT_TYPE,
            "text/javascript; charset=utf-8".to_owned(),
        ),
        (header::CACHE_CONTROL, "no-cache".to_owned()),
        (SERVICE_WORKER_ALLOWED, "/".to_owned()),
    ];
    if accepts_br(accept_encoding) {
        let mut resp = (common, js_br).into_response();
        resp.headers_mut().insert(
            header::CONTENT_ENCODING,
            header::HeaderValue::from_static("br"),
        );
        resp
    } else {
        let mut decompressed = Vec::new();
        let _ = brotli::BrotliDecompress(&mut std::io::Cursor::new(js_br), &mut decompressed);
        (common, decompressed).into_response()
    }
}

/// Answer a reserved preview path when no worker is installed
/// (docs/design/net.md § Reserve the prefix server-side).
///
/// Without this the gateway's SPA fallback would render the blit UI inside the
/// preview frame — a failure mode that looks like anything but "the worker did
/// not intercept this".
/// The routes every origin serving the blit UI must answer the same way,
/// before its own WebSocket upgrade and before its SPA fallback: the preview
/// service worker, and the preview path prefix.
///
/// One function rather than a copy per server, because the copies diverged:
/// the gateway had them and the local `blit web` server did not, so there
/// `/sw.js` fell through to `index.html` and the browser rejected it on its
/// MIME type. That is not a stray console line — the preview worker never
/// installs, so web previews do not work on that origin at all.
///
/// Returns `None` for paths the caller still owns.
pub fn try_ui_route(
    path: &str,
    sw_js_br: &'static [u8],
    sw_etag: &str,
    if_none_match: Option<&[u8]>,
    accept_encoding: Option<&str>,
) -> Option<Response> {
    if path == "/sw.js" {
        return Some(service_worker_response(
            sw_js_br,
            sw_etag,
            if_none_match,
            accept_encoding,
        ));
    }
    // `/x/…` reaching a SPA fallback renders the blit UI inside a preview
    // frame, which reads as anything but the failure it is.
    if path.starts_with(PREVIEW_PATH_PREFIX) {
        return Some(preview_unavailable_response());
    }
    None
}

pub fn preview_unavailable_response() -> Response {
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "blit preview: the preview service worker is not installed for this \
         origin, so this request reached the server instead of a relayed \
         socket. Open the blit UI in a top-level tab first; previews need a \
         secure context (https, or http on localhost).\n",
    )
        .into_response()
}

/// Try to match a font route from a raw request path (any prefix).
/// Handles `/fonts`, `/vt/fonts`, `/font/Name`, `/vt/font/Name%20With%20Spaces`.
/// Returns `Some(response)` if the path matched a font route, `None` otherwise.
pub fn try_font_route(
    path: &str,
    cors_origin: Option<&str>,
    accept_encoding: Option<&str>,
) -> Option<Response> {
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
        return Some(font_response(&name, cors_origin, accept_encoding));
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
        assert!(try_font_route("/fonts", None, None).is_some());
    }

    #[test]
    fn font_route_fonts_prefixed() {
        assert!(try_font_route("/vt/fonts", None, None).is_some());
    }

    #[test]
    fn font_route_font_name() {
        let resp = try_font_route("/font/Menlo", None, None);
        assert!(resp.is_some());
    }

    // ── face CSS compression ──
    //
    // The face arrives as base64 inside the stylesheet, so a real family is
    // tens of megabytes on the wire — measured, 25.4 MB for PragmataPro Mono,
    // a quarter of which is the encoding rather than the font.

    #[test]
    fn accept_encoding_reading() {
        assert!(accepts_br(Some("br")));
        assert!(accepts_br(Some("gzip, deflate, br")));
        assert!(accepts_br(Some("br;q=0.1")), "a low q is still an offer");
        assert!(!accepts_br(None), "a client that said nothing gets bytes");
        assert!(!accepts_br(Some("gzip, deflate")));
        assert!(
            !accepts_br(Some("br;q=0")),
            "q=0 is a refusal, not an offer"
        );
        assert!(
            !accepts_br(Some("brotli")),
            "the token is `br` — a prefix match would take anything"
        );
    }

    #[test]
    fn compression_round_trips_and_is_skipped_when_unwanted() {
        // Shaped like what the route actually serves: base64 of font bytes.
        let body = "@font-face { src: url('data:font/ttf;base64,".to_owned()
            + &"AAEAAAALAIAAAwAwT1MvMg8SBPsAAAC8AAAAYGNtYXAX".repeat(20_000)
            + "'); }";
        assert!(compress_br(body.as_bytes(), None).is_none());
        let compressed = compress_br(body.as_bytes(), Some("gzip, br")).expect("br was on offer");
        assert!(
            compressed.len() < body.len() / 2,
            "{} bytes from {} is not worth the CPU",
            compressed.len(),
            body.len()
        );
        let mut back = Vec::new();
        brotli::BrotliDecompress(&mut std::io::Cursor::new(&compressed), &mut back).unwrap();
        assert_eq!(
            back,
            body.as_bytes(),
            "a stylesheet that lost bytes is a broken font"
        );
    }

    /// Only runs where a monospace family is installed — the payload has to be
    /// a real face for the response to be one.
    #[tokio::test]
    async fn font_route_compresses_a_real_face() {
        let Some(family) = blit_fonts::list_monospace_font_families()
            .into_iter()
            .next()
        else {
            eprintln!("no monospace font installed; skipping");
            return;
        };
        let Some(css) = blit_fonts::font_face_css(&family) else {
            eprintln!("{family} is listed but serves no face; skipping");
            return;
        };
        let path = format!(
            "/font/{}",
            percent_encoding::utf8_percent_encode(&family, percent_encoding::NON_ALPHANUMERIC)
        );
        let plain = try_font_route(&path, None, None).expect("a font route");
        assert_eq!(plain.status(), StatusCode::OK);
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());

        let compressed =
            try_font_route(&path, None, Some("gzip, deflate, br")).expect("a font route");
        assert_eq!(compressed.status(), StatusCode::OK);
        assert_eq!(
            compressed.headers().get(header::CONTENT_ENCODING).unwrap(),
            "br"
        );
        assert_eq!(
            compressed.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css",
            "the encoding changes, the type does not"
        );
        let body = axum::body::to_bytes(compressed.into_body(), usize::MAX)
            .await
            .expect("a body");
        let mut back = Vec::new();
        brotli::BrotliDecompress(&mut std::io::Cursor::new(body.as_ref()), &mut back).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&back),
            css,
            "the browser has to end up with the stylesheet the server generated"
        );
        assert!(
            back.len() > body.len(),
            "compression that grows the face is a regression"
        );
    }

    #[test]
    fn font_route_font_metrics() {
        let resp = try_font_route("/font-metrics/Menlo", None, None);
        assert!(resp.is_some());
    }

    // ── service worker + preview routes ──

    /// Every origin serving the blit UI routes these, and the copies used to
    /// diverge: the gateway had them, the local `blit web` server did not, so
    /// `/sw.js` there fell through to `index.html` and the browser rejected it
    /// on its MIME type — which meant no preview worker, so no previews at all
    /// on that origin.
    #[tokio::test]
    async fn ui_routes_cover_the_worker_and_the_preview_prefix() {
        let sw = try_ui_route("/sw.js", b"fake-br", "\"e\"", None, None)
            .expect("/sw.js belongs to every UI origin");
        assert_eq!(sw.status(), 200);
        assert!(
            sw.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/javascript"),
            "a worker served as anything else is refused by the browser"
        );
        assert_eq!(sw.headers().get("service-worker-allowed").unwrap(), "/");

        // A preview path must never reach a SPA fallback.
        for path in [PREVIEW_PATH_PREFIX, "/x/local/http/localhost:3000/"] {
            let resp = try_ui_route(path, b"x", "\"e\"", None, None)
                .unwrap_or_else(|| panic!("{path} belongs to every UI origin"));
            assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        }

        // Conditional requests still work through the shared route.
        let cached = try_ui_route("/sw.js", b"x", "\"e\"", Some(b"\"e\""), None).unwrap();
        assert_eq!(cached.status(), axum::http::StatusCode::NOT_MODIFIED);

        // Everything else stays the caller's.
        for path in ["/", "/index.html", "/mux", "/config", "/xylophone"] {
            assert!(
                try_ui_route(path, b"x", "\"e\"", None, None).is_none(),
                "{path} is not a shared route"
            );
        }
    }

    #[tokio::test]
    async fn service_worker_declares_root_scope_and_js_type() {
        // All three matter: a worker served without root scope cannot claim
        // `/`, one served as text/plain is rejected outright, and a cached one
        // outlives the app it serves.
        let resp = service_worker_response(b"fake-br", "\"e\"", None, None);
        assert_eq!(resp.status(), 200);
        let headers = resp.headers();
        assert_eq!(headers.get("service-worker-allowed").unwrap(), "/");
        assert!(
            headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-cache");
    }

    #[tokio::test]
    async fn service_worker_sends_br_only_when_accepted() {
        let plain = service_worker_response(b"x", "\"e\"", None, None);
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        let compressed = service_worker_response(b"x", "\"e\"", None, Some("gzip, br"));
        assert_eq!(
            compressed.headers().get(header::CONTENT_ENCODING).unwrap(),
            "br"
        );
    }

    #[tokio::test]
    async fn service_worker_304_keeps_the_scope_header() {
        // A revalidated worker still needs its scope declared, or the browser
        // narrows it on re-registration.
        let resp = service_worker_response(b"x", "\"e\"", Some(b"\"e\""), None);
        assert_eq!(resp.status(), 304);
        assert_eq!(resp.headers().get("service-worker-allowed").unwrap(), "/");
    }

    #[tokio::test]
    async fn preview_miss_is_a_legible_503_not_the_app() {
        let resp = preview_unavailable_response();
        assert_eq!(resp.status(), 503);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
    }

    #[test]
    fn preview_prefix_matches_the_client_constant() {
        // js/core/src/preview.ts exports the same string; they must agree or
        // the gateway reserves a path the worker never claims.
        assert_eq!(PREVIEW_PATH_PREFIX, "/x/");
    }

    #[test]
    fn font_route_no_match() {
        assert!(try_font_route("/api/sessions", None, None).is_none());
        assert!(try_font_route("/", None, None).is_none());
    }

    #[test]
    fn font_route_rejects_empty_name() {
        assert!(try_font_route("/font/", None, None).is_none());
        assert!(try_font_route("/font-metrics/", None, None).is_none());
    }

    #[test]
    fn font_route_rejects_nested_path() {
        assert!(try_font_route("/font/a/b", None, None).is_none());
    }
}
