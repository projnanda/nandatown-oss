//! NANDA-specific external A2A connections. No external endpoint is routed
//! through the general managed-agent proxy or placed in the public registry.
mod net;
mod routes;
mod wire;

pub(crate) fn router() -> axum::Router<crate::state::AppState> {
    routes::router()
}

pub(crate) async fn page() -> impl axum::response::IntoResponse {
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
            ("x-frame-options", "DENY"),
            (
                "content-security-policy",
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ],
        axum::response::Html(include_str!("operator.html")),
    )
}
