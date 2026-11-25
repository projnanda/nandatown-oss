//! This pilot is operator opt-in. Both URLs must be approved in full; a card
//! cannot nominate a different path to receive its owner's peer credential.
use reqwest::{Client, Url};
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub(super) const TARGETS_ENV: &str = "NANDA_EXTERNAL_A2A_TARGETS";
const FIXTURE_CARD: &str = "http://127.0.0.1:18916/.well-known/agent-card.json";
const FIXTURE_RPC: &str = "http://127.0.0.1:18916/a2a/v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    card_url: String,
    endpoint: String,
}

fn url(s: &str) -> Result<Url, &'static str> {
    if s.len() > 2048 || s.is_empty() || s.trim() != s || s.chars().any(char::is_control) {
        return Err("a2a_target_not_allowed");
    }
    let u = Url::parse(s).map_err(|_| "a2a_target_not_allowed")?;
    if u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err("a2a_target_not_allowed");
    }
    Ok(u)
}

fn approved(config: Option<&str>, input: &str) -> Result<(Url, Url, bool), &'static str> {
    let config = config
        .filter(|s| !s.trim().is_empty())
        .ok_or("a2a_disabled")?;
    if config.len() > 128 * 1024 {
        return Err("a2a_config_invalid");
    }
    let pairs: Vec<Pair> = serde_json::from_str(config).map_err(|_| "a2a_config_invalid")?;
    if pairs.is_empty() {
        return Err("a2a_disabled");
    }
    let requested = url(input)?;
    let mut found = None;
    for p in pairs {
        let card = url(&p.card_url).map_err(|_| "a2a_config_invalid")?;
        let end = url(&p.endpoint).map_err(|_| "a2a_config_invalid")?;
        let fixture =
            cfg!(debug_assertions) && card.as_str() == FIXTURE_CARD && end.as_str() == FIXTURE_RPC;
        if card.origin() != end.origin() || (!fixture && card.scheme() != "https") {
            return Err("a2a_config_invalid");
        }
        if card == requested {
            if found.is_some() {
                return Err("a2a_config_invalid");
            }
            found = Some((card, end, fixture));
        }
    }
    found.ok_or("a2a_target_not_allowed")
}

// Conservative public unicast ranges. IPv4-mapped/translated/local IPv6 is
// not necessary for external HTTPS, and cannot be used as a second address family.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || v.is_private()
                || v.is_link_local()
                || v.is_documentation()
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && ((b == 0 && c == 0) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19)))
        }
        IpAddr::V6(v) => {
            let [a, b, ..] = v.segments();
            (a & 0xe000) == 0x2000
                && a != 0x2002
                && !(a == 0x2001 && (b <= 0x01ff || b == 0x0db8))
                && !(a == 0x3fff && (b & 0xf000) == 0)
        }
    }
}

pub(super) async fn target(input: &str) -> Result<(Client, Url, Url), &'static str> {
    let config = std::env::var(TARGETS_ENV).ok();
    let (card, endpoint, fixture) = approved(config.as_deref(), input)?;
    let host = card
        .host_str()
        .ok_or("a2a_config_invalid")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = card.port_or_known_default().ok_or("a2a_config_invalid")?;
    let mut client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15));
    if !fixture {
        let addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            tokio::time::timeout(
                Duration::from_secs(4),
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| "a2a_target_unavailable")?
            .map_err(|_| "a2a_target_unavailable")?
            .collect()
        };
        if addrs.is_empty() || addrs.iter().any(|a| !public_ip(a.ip())) {
            return Err("a2a_target_not_allowed");
        }
        // Reqwest connects to these exact answers but still checks the TLS cert
        // for the original hostname. No system proxy or second DNS resolution.
        client = client.resolve_to_addrs(host, &addrs);
    }
    Ok((
        client.build().map_err(|_| "a2a_config_invalid")?,
        card,
        endpoint,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CARD: &str = "https://agent.example/.well-known/agent-card.json";
    const RPC: &str = "https://agent.example/a2a";
    fn config(card: &str, rpc: &str) -> String {
        serde_json::json!([{"card_url":card,"endpoint":rpc}]).to_string()
    }

    #[test]
    fn a_card_url_must_match_an_explicit_operator_pair() {
        assert_eq!(approved(None, CARD).unwrap_err(), "a2a_disabled");
        assert!(approved(Some(&config(CARD, RPC)), RPC).is_err());
        assert!(approved(Some(&config(CARD, "https://another.example/rpc")), CARD).is_err());
        let (card, end, local) = approved(Some(&config(CARD, RPC)), CARD).unwrap();
        assert_eq!((card.as_str(), end.as_str(), local), (CARD, RPC, false));
    }
    #[test]
    fn fixture_is_one_explicit_pair_in_debug_builds_only() {
        assert_eq!(
            approved(Some(&config(FIXTURE_CARD, FIXTURE_RPC)), FIXTURE_CARD).is_ok(),
            cfg!(debug_assertions)
        );
        assert!(
            approved(
                Some(&config(FIXTURE_CARD, "http://127.0.0.1:18916/")),
                FIXTURE_CARD
            )
            .is_err()
        );
    }
    #[test]
    fn credentials_and_queries_never_become_catalog_urls() {
        for suffix in ["?", "?access_token=example", "#anchor"] {
            assert!(url(&format!("{CARD}{suffix}")).is_err());
        }
        assert!(url("https://user:secret@agent.example/card").is_err());
    }
    #[test]
    fn public_transport_does_not_admit_private_or_special_addresses() {
        for s in [
            "0.1.2.3",
            "10.0.0.1",
            "127.0.0.1",
            "224.0.0.1",
            "192.0.2.1",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
            "2002::1",
        ] {
            assert!(!public_ip(s.parse().unwrap()), "not public: {s}");
        }
        for s in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(s.parse().unwrap()));
        }
    }
}
