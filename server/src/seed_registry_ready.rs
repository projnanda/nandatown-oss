//! Waits for the embedded registry to be pullable over HTTPS before seed
//! agents are deployed.
//!
//! Seeding runs as soon as the server starts. On a fresh cluster the edge in
//! front of the registry is often still obtaining its certificate at that
//! moment (Let's Encrypt for an automatic `nip.io` host takes about a minute),
//! so a node pulling the just-mirrored image fails its TLS handshake and backs
//! off for up to five minutes. Mirroring is unaffected — the server writes into
//! its own registry directly — so only the deploy waits.

use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

/// How long seeding waits for the registry before deploying anyway.
pub const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether nodes pull `registry` over TLS that may not be ready yet. A blank
/// registry (agent images stay unqualified), a loopback one (local Docker)
/// and one listed in `INSECURE_REGISTRIES` (plain HTTP) have no certificate
/// to wait for.
pub fn needs_tls_wait(registry: &str, insecure: &[&str]) -> bool {
    let host = registry.rsplit_once(':').map_or(registry, |(h, _)| h);
    !registry.is_empty()
        && host != "localhost"
        && host != "127.0.0.1"
        && !insecure.contains(&registry)
}

/// Polls `{base_url}/v2/` until it answers — any HTTP status counts, since an
/// unauthenticated `401` already proves the TLS handshake succeeded. Returns
/// `false` when `timeout` elapses first; the caller deploys regardless, so a
/// broken edge delays seeding rather than blocking it.
pub async fn wait_until_ready(
    http: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
    interval: Duration,
) -> bool {
    let url = format!("{}/v2/", base_url.trim_end_matches('/'));
    let deadline = Instant::now() + timeout;
    let mut logged = false;
    loop {
        match http.get(&url).timeout(PROBE_TIMEOUT).send().await {
            Ok(_) => return true,
            Err(error) if !logged => {
                info!(%url, %error, "registry not reachable over TLS yet; waiting before deploying seed agents");
                logged = true;
            }
            Err(_) => {}
        }
        if Instant::now() + interval > deadline {
            warn!(%url, ?timeout, "registry still unreachable; deploying seed agents anyway");
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Reads `INSECURE_REGISTRIES` the way the EE runtime does (comma-separated).
pub fn insecure_registries_from_env() -> Vec<String> {
    std::env::var("INSECURE_REGISTRIES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_registry_host_is_waited_for() {
        assert!(needs_tls_wait("nasiko.34.170.214.102.nip.io", &[]));
        assert!(needs_tls_wait("registry.example.com:5000", &[]));
    }

    #[test]
    fn blank_loopback_and_plain_http_registries_are_not() {
        assert!(!needs_tls_wait("", &[]));
        assert!(!needs_tls_wait("localhost:5000", &[]));
        assert!(!needs_tls_wait("127.0.0.1:5000", &[]));
        assert!(!needs_tls_wait("10.0.0.5:30500", &["10.0.0.5:30500"]));
    }

    const FAST: Duration = Duration::from_millis(50);

    /// A registry that answers `/v2/` with the challenge a real one sends.
    async fn serve_registry(listener: tokio::net::TcpListener) {
        let app = axum::Router::new().route(
            "/v2/",
            axum::routing::get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        );
        axum::serve(listener, app).await.unwrap();
    }

    #[tokio::test]
    async fn an_unauthenticated_answer_counts_as_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve_registry(listener));

        let ready =
            wait_until_ready(&reqwest::Client::new(), &base, Duration::from_secs(5), FAST).await;

        assert!(ready);
    }

    #[tokio::test]
    async fn waits_for_a_registry_that_comes_up_later() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            serve_registry(tokio::net::TcpListener::bind(addr).await.unwrap()).await;
        });

        let ready = wait_until_ready(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            Duration::from_secs(5),
            FAST,
        )
        .await;

        assert!(ready);
    }

    #[tokio::test]
    async fn gives_up_after_the_timeout() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let ready = wait_until_ready(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            Duration::from_millis(300),
            FAST,
        )
        .await;

        assert!(!ready);
    }
}
