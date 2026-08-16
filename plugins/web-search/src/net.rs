//! Outbound HTTP: the client, the address guard, and body decoding.
//!
//! This plugin runs on hardware somebody donated to a mesh and makes requests
//! in their name, so every outbound request passes the same three checks: the
//! URL is shaped like something we are willing to fetch, the host does not
//! resolve into the operator's own network, and the response is text we can
//! actually turn into readable output.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::{Client, Url};

use crate::config::HttpConfig;

/// Build the one shared HTTP client.
///
/// Redirects are disabled on the client on purpose. `fetch` follows them by
/// hand so the address guard and `robots.txt` are re-checked at every hop — a
/// permitted URL that 302s to `http://169.254.169.254/` must not be followed
/// just because the first hop passed.
pub fn build_client(config: &HttpConfig) -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(config.user_agent.clone())
        .timeout(config.request_timeout)
        .connect_timeout(Duration::from_secs(10).min(config.request_timeout))
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .build()
}

/// Why a URL was refused. The message goes straight back to the caller, so it
/// says what was wrong and what would make it work.
pub fn check_url_shape(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "`{}` is not a fetchable scheme; only http and https are allowed.",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "URLs with embedded credentials are refused; pass the URL without user:password@."
                .to_string(),
        );
    }
    match url.host_str() {
        None | Some("") => Err("that URL has no host.".to_string()),
        Some(_) => Ok(()),
    }
}

/// Resolve the URL's host and refuse anything that lands inside the operator's
/// own network.
///
/// This is a guard, not a sandbox: the request re-resolves the name, so a DNS
/// answer that changes between this check and the connection (DNS rebinding)
/// can still slip through. It stops the ordinary cases — a model asking for
/// `http://127.0.0.1:9337/`, a link to `http://169.254.169.254/latest/meta-data/`,
/// a redirect into `10.0.0.0/8` — which is what it is for.
pub async fn check_url_destination(url: &Url, allow_private: bool) -> Result<(), String> {
    check_url_shape(url)?;
    if allow_private {
        return Ok(());
    }

    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(80);
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
            "`{host}` resolves to {blocked}, which is inside a private, loopback, or link-local \
             range. web-search refuses those by default so a tool call cannot reach the \
             operator's own services or a cloud metadata endpoint. Start the plugin with \
             `--allow-private-network` if reaching that network is genuinely the point."
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

/// The media type and charset from a `Content-Type` header, lowercased.
pub fn parse_content_type(header: &str) -> (String, Option<String>) {
    let mut parts = header.split(';');
    let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let charset = parts.find_map(|part| {
        let (name, value) = part.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("charset") {
            Some(value.trim().trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    });
    (media_type, charset)
}

/// Whether a media type is something we can render as text.
///
/// The list is an allowlist rather than a "not an image" denylist: returning a
/// megabyte of lossily-decoded PDF bytes as "readable text" would be a worse
/// answer than an honest refusal.
pub fn is_textual_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "text/html"
            | "text/plain"
            | "text/markdown"
            | "text/xml"
            | "text/csv"
            | "application/xhtml+xml"
            | "application/xml"
            | "application/json"
            | "application/ld+json"
            | "application/rss+xml"
            | "application/atom+xml"
    ) || media_type.is_empty()
}

/// Turn response bytes into a `String`.
///
/// UTF-8 is assumed unless the header says otherwise; `latin1` and
/// `windows-1252` are common enough on older pages to be worth handling, and
/// every other labelled charset falls back to lossy UTF-8 rather than failing
/// the whole fetch.
pub fn decode_body(bytes: &[u8], charset: Option<&str>) -> String {
    match charset.unwrap_or("utf-8") {
        "iso-8859-1" | "latin1" | "windows-1252" | "cp1252" => {
            bytes.iter().map(|byte| *byte as char).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test URL parses")
    }

    #[test]
    fn only_http_and_https_are_fetchable() {
        assert!(check_url_shape(&url("https://example.com/a")).is_ok());
        assert!(check_url_shape(&url("http://example.com/a")).is_ok());
        assert!(check_url_shape(&url("file:///etc/passwd")).is_err());
        assert!(check_url_shape(&url("ftp://example.com/a")).is_err());
        assert!(check_url_shape(&url("data:text/html,hi")).is_err());
    }

    #[test]
    fn embedded_credentials_are_refused() {
        let error = check_url_shape(&url("https://user:pass@example.com/")).unwrap_err();
        assert!(error.contains("credentials"), "{error}");
    }

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
        assert!(!is_private_address("::ffff:1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn content_type_splits_into_media_type_and_charset() {
        assert_eq!(
            parse_content_type("text/html; charset=UTF-8"),
            ("text/html".into(), Some("utf-8".into()))
        );
        assert_eq!(
            parse_content_type("Text/HTML;charset=\"ISO-8859-1\""),
            ("text/html".into(), Some("iso-8859-1".into()))
        );
        assert_eq!(
            parse_content_type("application/pdf"),
            ("application/pdf".into(), None)
        );
    }

    #[test]
    fn only_text_shaped_media_types_are_accepted() {
        assert!(is_textual_media_type("text/html"));
        assert!(is_textual_media_type("application/json"));
        assert!(is_textual_media_type(""));
        assert!(!is_textual_media_type("application/pdf"));
        assert!(!is_textual_media_type("image/png"));
        assert!(!is_textual_media_type("application/octet-stream"));
    }

    #[test]
    fn bodies_decode_as_utf8_by_default_and_latin1_when_labelled() {
        assert_eq!(decode_body("héllo".as_bytes(), None), "héllo");
        assert_eq!(decode_body(&[0x68, 0xe9, 0x6c], Some("iso-8859-1")), "hél");
        // Invalid UTF-8 degrades rather than failing the whole fetch.
        assert!(decode_body(&[0x68, 0xff, 0x6c], Some("utf-8")).contains('h'));
    }
}
