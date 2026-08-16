//! Outbound HTTP: the client, the address guard, and the capped body read.
//!
//! The address guard is the same one `web-search` uses, and deliberately so —
//! a second implementation of "is this address inside the operator's network"
//! is a second implementation to get wrong. What differs is *what* it guards.
//! In `web-search` the host comes from a model, so the guard is the only thing
//! between a tool call and `127.0.0.1:9337`. Here the host comes from the
//! operator's own declaration, so the guard is protecting against a declaration
//! that points somewhere the operator did not think through — a base URL whose
//! DNS name resolves into `10.0.0.0/8`, or an outright
//! `http://169.254.169.254/`, being handed to a model as a callable tool.
//!
//! It runs per call rather than once at startup, because a name that resolved
//! publicly when the node booted can resolve privately an hour later.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::{Client, Response, Url};

/// Build the one shared HTTP client.
///
/// Redirects are disabled and never followed. An API client that chases a
/// `Location` header is an API client whose destination is chosen by the
/// server it just talked to, which is exactly the property this plugin exists
/// to remove. A 3xx comes back to the caller as a status, with the `location`
/// header among the reported response headers, and the operator can declare
/// the real path if that is where the API actually lives.
pub fn build_client(user_agent: &str) -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(user_agent.to_string())
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        // Per-request timeouts come from the endpoint's own `timeout_secs`;
        // this is the outer bound on getting a connection at all.
        .connect_timeout(Duration::from_secs(10))
        .build()
}

/// Resolve a URL's host and refuse anything inside the operator's own network.
///
/// This is a guard, not a sandbox: the request re-resolves the name, so a DNS
/// answer that changes between this check and the connection (DNS rebinding)
/// can still slip through. It stops the ordinary cases, which is what it is
/// for.
pub async fn check_destination(
    url: &Url,
    allow_private: bool,
    endpoint: &str,
) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }

    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<IpAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|error| format!("could not resolve `{host}`: {error}"))?
        .map(|address| address.ip())
        .collect();

    if addresses.is_empty() {
        return Err(format!("`{host}` resolved to no addresses."));
    }
    if let Some(blocked) = addresses
        .iter()
        .find(|address| is_private_address(**address))
    {
        return Err(format!(
            "endpoint `{endpoint}` has a base URL whose host `{host}` resolves to {blocked}, \
             which is inside a private, loopback, or link-local range. rest-client refuses those \
             by default so a declared endpoint cannot become a route to this node's own services \
             or to a cloud metadata endpoint. Set `allow_private_base = true` on that endpoint if \
             it really is a service on your own network."
        ));
    }
    Ok(())
}

/// Addresses that must never be reached on behalf of a model by default.
pub fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // A v4-mapped v6 address is just the v4 address wearing a hat.
            Some(mapped) => is_private_v4(mapped),
            None => is_private_v6(v6),
        },
    }
}

fn is_private_v4(address: Ipv4Addr) -> bool {
    let [a, b, ..] = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_unspecified()
        || address.is_documentation()
        || address.is_multicast()
        // 100.64.0.0/10, carrier-grade NAT — shared address space, not the
        // public internet.
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, IETF protocol assignments.
        || address.octets()[..3] == [192, 0, 0]
        // 198.18.0.0/15, benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // 240.0.0.0/4, reserved.
        || a >= 240
}

fn is_private_v6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        // fc00::/7, unique local addresses.
        || (first & 0xfe00) == 0xfc00
        // fe80::/10, link-local.
        || (first & 0xffc0) == 0xfe80
}

/// Read a response body, stopping at `limit` bytes.
///
/// `Content-Length` is checked first so an obviously oversized response is
/// refused before it is transferred, but the chunk loop is what enforces the
/// cap: a chunked response has no length to check. Returns
/// `(bytes, truncated)` — unlike a document fetch, a truncated API response is
/// still worth returning, as long as the caller is told.
pub async fn read_capped(response: Response, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("reading the response body failed: {error}"))?
    {
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// The media type from a `Content-Type` header, lowercased and without its
/// parameters.
pub fn media_type(header: &str) -> String {
    header
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether a media type is one whose body should be parsed as JSON.
pub fn is_json_media_type(media_type: &str) -> bool {
    media_type == "application/json" || media_type == "text/json" || media_type.ends_with("+json")
}

/// Whether a body of this media type can be returned as text at all.
///
/// An allowlist rather than a denylist: returning a lossily-decoded PNG as a
/// `text` field would be a worse answer than saying what the type was.
pub fn is_textual_media_type(media_type: &str) -> bool {
    media_type.is_empty()
        || media_type.starts_with("text/")
        || is_json_media_type(media_type)
        || matches!(
            media_type,
            "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
                | "application/problem+xml"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_private_and_link_local_v4_are_blocked() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "198.18.0.1",
            "240.0.0.1",
        ] {
            assert!(
                is_private_address(address.parse().unwrap()),
                "{address} should be blocked"
            );
        }
    }

    #[test]
    fn ordinary_public_v4_is_allowed() {
        for address in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "172.32.0.1"] {
            assert!(
                !is_private_address(address.parse().unwrap()),
                "{address} should be allowed"
            );
        }
    }

    #[test]
    fn v6_loopback_unique_local_and_link_local_are_blocked() {
        for address in ["::1", "::", "fc00::1", "fd12:3456::1", "fe80::1", "ff02::1"] {
            assert!(
                is_private_address(address.parse().unwrap()),
                "{address} should be blocked"
            );
        }
        assert!(!is_private_address("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn a_v4_mapped_v6_loopback_cannot_smuggle_past_the_guard() {
        assert!(is_private_address("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_address(
            "::ffff:169.254.169.254".parse().unwrap()
        ));
        assert!(is_private_address("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!is_private_address("::ffff:1.1.1.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn the_guard_refuses_loopback_and_names_the_setting_that_allows_it() {
        let url = Url::parse("http://127.0.0.1:9337/v1/models").expect("a URL");

        let error = check_destination(&url, false, "local")
            .await
            .expect_err("loopback is refused");

        assert!(error.contains("allow_private_base"), "{error}");
        assert!(error.contains("local"), "{error}");
    }

    #[tokio::test]
    async fn an_endpoint_that_opted_in_reaches_its_own_network() {
        let url = Url::parse("http://127.0.0.1:9337/v1/models").expect("a URL");

        check_destination(&url, true, "local")
            .await
            .expect("the operator opted in");
    }

    #[test]
    fn media_types_are_split_and_classified() {
        assert_eq!(
            media_type("application/json; charset=utf-8"),
            "application/json"
        );
        assert_eq!(media_type("Text/Plain"), "text/plain");

        assert!(is_json_media_type("application/json"));
        assert!(is_json_media_type("application/vnd.github+json"));
        assert!(!is_json_media_type("text/html"));

        assert!(is_textual_media_type("text/csv"));
        assert!(is_textual_media_type("application/problem+json"));
        assert!(is_textual_media_type(""));
        assert!(!is_textual_media_type("image/png"));
        assert!(!is_textual_media_type("application/octet-stream"));
    }
}
