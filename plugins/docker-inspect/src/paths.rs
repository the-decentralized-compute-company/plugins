//! Every request this plugin is able to make, and nothing else.
//!
//! [`ApiPath`] wraps a string, its field is private, and the only functions
//! that can build one live in this module. Nothing elsewhere in the crate can
//! hand [`crate::transport`] a path it invented, and there is no constructor
//! that takes a caller's string. That is what makes the read-only claim
//! structural rather than a matter of review: the module boundary is the
//! allowlist.
//!
//! Container ids reach these functions only after
//! [`crate::visibility::resolve`] has matched a caller's reference against the
//! containers the daemon reported, and [`hex_id`] re-checks the result before
//! it is spliced into a path. A daemon that answered with something strange
//! gets the same treatment as a caller that asked for something strange.

/// A path on the Docker Engine API that this plugin is willing to request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiPath(String);

impl ApiPath {
    /// Private on purpose — see the module documentation.
    fn new(path: String) -> Self {
        Self(path)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApiPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Keep only the characters a Docker id can contain.
///
/// Ids are hexadecimal, so anything else is either a daemon behaving oddly or
/// something that should never have got this far; either way the id is dropped
/// rather than cleaned up, because a "sanitised" identifier that still reaches
/// the wire is the bug this function exists to prevent.
fn hex_id(id: &str) -> Option<&str> {
    let trimmed = id.trim();
    (!trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| character.is_ascii_hexdigit()))
    .then_some(trimmed)
}

/// `GET /_ping` — the cheapest possible reachability check.
pub fn ping(api_version: &str) -> ApiPath {
    ApiPath::new(format!("/{api_version}/_ping"))
}

/// `GET /version` — daemon and API versions.
pub fn version(api_version: &str) -> ApiPath {
    ApiPath::new(format!("/{api_version}/version"))
}

/// `GET /info` — what the daemon reports about the host it runs on.
pub fn info(api_version: &str) -> ApiPath {
    ApiPath::new(format!("/{api_version}/info"))
}

/// `GET /containers/json` — the container list.
///
/// `all` is the only variable, and it is a bool rather than a string, so there
/// is no query parameter a caller can influence.
pub fn containers(api_version: &str, all: bool) -> ApiPath {
    ApiPath::new(format!(
        "/{api_version}/containers/json?all={}",
        if all { "1" } else { "0" }
    ))
}

/// `GET /containers/{id}/json` — the full inspect payload for one container.
pub fn container_inspect(api_version: &str, id: &str) -> Option<ApiPath> {
    Some(ApiPath::new(format!(
        "/{api_version}/containers/{}/json",
        hex_id(id)?
    )))
}

/// `GET /containers/{id}/logs` — a bounded, non-following log read.
///
/// `follow` is deliberately absent: this plugin makes one request and reads a
/// bounded body, so there is no code path that holds a stream open.
pub fn container_logs(
    api_version: &str,
    id: &str,
    tail: usize,
    timestamps: bool,
    since_unix: Option<u64>,
) -> Option<ApiPath> {
    let mut path = format!(
        "/{api_version}/containers/{}/logs?stdout=1&stderr=1&follow=0&tail={tail}&timestamps={}",
        hex_id(id)?,
        u8::from(timestamps)
    );
    if let Some(since) = since_unix {
        path.push_str(&format!("&since={since}"));
    }
    Some(ApiPath::new(path))
}

/// `GET /containers/{id}/stats` — one sample, not a stream.
///
/// `stream=0` makes the daemon collect two samples a second apart and return
/// once, which is what makes a CPU percentage computable; `one-shot` would
/// return faster but with an empty `precpu_stats`.
pub fn container_stats(api_version: &str, id: &str) -> Option<ApiPath> {
    Some(ApiPath::new(format!(
        "/{api_version}/containers/{}/stats?stream=0",
        hex_id(id)?
    )))
}

/// `GET /images/json` — the local image list.
pub fn images(api_version: &str) -> ApiPath {
    ApiPath::new(format!("/{api_version}/images/json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "v1.41";
    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn every_path_is_under_the_configured_api_version() {
        let built = vec![
            ping(V),
            version(V),
            info(V),
            containers(V, true),
            images(V),
            container_inspect(V, ID).unwrap(),
            container_logs(V, ID, 100, false, None).unwrap(),
            container_stats(V, ID).unwrap(),
        ];

        for path in built {
            assert!(
                path.as_str().starts_with("/v1.41/"),
                "{path} must be version prefixed"
            );
        }
    }

    #[test]
    fn a_non_hexadecimal_id_produces_no_path_at_all() {
        for id in [
            "../../secrets",
            "abc/json?follow=1",
            "zzzz",
            "",
            "   ",
            "abc def",
            "abc%2f",
        ] {
            assert_eq!(container_inspect(V, id), None, "{id}");
            assert_eq!(container_logs(V, id, 10, false, None), None, "{id}");
            assert_eq!(container_stats(V, id), None, "{id}");
        }
    }

    #[test]
    fn the_container_list_takes_a_bool_and_not_a_string() {
        assert!(containers(V, true).as_str().ends_with("all=1"));
        assert!(containers(V, false).as_str().ends_with("all=0"));
    }

    #[test]
    fn a_log_request_never_asks_the_daemon_to_follow() {
        let path = container_logs(V, ID, 250, true, Some(1_700_000_000)).unwrap();

        assert!(path.as_str().contains("follow=0"), "{path}");
        assert!(path.as_str().contains("tail=250"), "{path}");
        assert!(path.as_str().contains("timestamps=1"), "{path}");
        assert!(path.as_str().contains("since=1700000000"), "{path}");
    }

    #[test]
    fn a_stats_request_asks_for_one_sample_with_a_previous_one() {
        let path = container_stats(V, ID).unwrap();
        assert!(path.as_str().ends_with("/stats?stream=0"), "{path}");
        assert!(!path.as_str().contains("one-shot"), "{path}");
    }
}
