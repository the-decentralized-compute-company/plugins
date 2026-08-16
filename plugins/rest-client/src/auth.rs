//! Credentials: where they come from, how they are attached, and how they are
//! kept out of everything else.
//!
//! Auth is configuration, never a model argument. The declaration in
//! `rest-client.toml` names an environment variable; this module reads that
//! variable once at startup and holds the value in a [`Secret`], which has no
//! `Display`, no `Serialize`, and a `Debug` that prints `<redacted>`. Every
//! string this plugin returns — tool results, error messages, the URL echoed
//! back to the caller — is passed through a [`Redactor`] built from those same
//! values, so a credential that leaks into a transport error on its way out is
//! removed before anybody sees it.
//!
//! The environment is read exactly once, in `main`. Nothing here calls
//! `std::env` so the whole module is testable from a map.

use std::collections::BTreeMap;

use crate::catalog::{AuthDecl, Catalog};

/// Values read from the process environment, as a map so resolution stays a
/// pure function.
pub type EnvMap = BTreeMap<String, String>;

/// A credential.
///
/// Deliberately missing: `Display`, `Serialize`, `PartialEq`, and a derived
/// `Debug`. The only way to reach the value is [`Secret::expose`], which is
/// easy to grep for.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the credential. Every call site should be attaching it to an
    /// outbound request and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// A credential resolved from the environment, ready to attach.
#[derive(Clone, Debug)]
pub enum ResolvedAuth {
    None,
    Bearer(Secret),
    Basic { username: String, password: Secret },
    Header { name: String, value: Secret },
    Query { param: String, value: Secret },
}

impl ResolvedAuth {
    /// The header this auth adds, if any. `(name, value)`.
    pub fn header(&self) -> Option<(String, String)> {
        match self {
            Self::None | Self::Query { .. } => None,
            Self::Bearer(token) => Some((
                "Authorization".to_string(),
                format!("Bearer {}", token.expose()),
            )),
            Self::Basic { username, password } => Some((
                "Authorization".to_string(),
                format!(
                    "Basic {}",
                    base64_encode(format!("{username}:{}", password.expose()).as_bytes())
                ),
            )),
            Self::Header { name, value } => Some((name.clone(), value.expose().to_string())),
        }
    }

    /// The query parameter this auth adds, if any. `(name, value)`.
    pub fn query(&self) -> Option<(String, String)> {
        match self {
            Self::Query { param, value } => Some((param.clone(), value.expose().to_string())),
            _ => None,
        }
    }

    /// The query parameter name this auth uses, so a URL can be echoed back
    /// with the value replaced rather than removed.
    pub fn query_param_name(&self) -> Option<&str> {
        match self {
            Self::Query { param, .. } => Some(param.as_str()),
            _ => None,
        }
    }
}

/// Whether an endpoint can authenticate right now.
#[derive(Clone, Debug)]
pub enum AuthState {
    /// Either no auth is declared or the credential was found.
    Ready(ResolvedAuth),
    /// Auth is declared but the environment variable is unset or empty. The
    /// endpoint stays declared and every call to it fails with this message —
    /// one endpoint missing a key does not take the other endpoints down.
    Missing(String),
}

impl AuthState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// Resolve every endpoint's credential and build the redaction list.
///
/// Returns a map keyed by endpoint name; an endpoint always has an entry.
pub fn resolve(catalog: &Catalog, env: &EnvMap) -> (BTreeMap<String, AuthState>, Redactor) {
    let mut states = BTreeMap::new();
    let mut secrets: Vec<String> = Vec::new();

    for endpoint in &catalog.endpoints {
        let name = endpoint.name.as_str();
        let state = match &endpoint.auth {
            AuthDecl::None => AuthState::Ready(ResolvedAuth::None),
            AuthDecl::Bearer { token_env } => match read(env, name, token_env) {
                Ok(value) => {
                    secrets.push(value.clone());
                    AuthState::Ready(ResolvedAuth::Bearer(Secret::new(value)))
                }
                Err(problem) => AuthState::Missing(problem),
            },
            AuthDecl::Basic {
                username,
                password_env,
            } => match read(env, name, password_env) {
                Ok(value) => {
                    secrets.push(value.clone());
                    AuthState::Ready(ResolvedAuth::Basic {
                        username: username.clone(),
                        password: Secret::new(value),
                    })
                }
                Err(problem) => AuthState::Missing(problem),
            },
            AuthDecl::Header { header, value_env } => match read(env, name, value_env) {
                Ok(value) => {
                    secrets.push(value.clone());
                    AuthState::Ready(ResolvedAuth::Header {
                        name: header.clone(),
                        value: Secret::new(value),
                    })
                }
                Err(problem) => AuthState::Missing(problem),
            },
            AuthDecl::Query { param, value_env } => match read(env, name, value_env) {
                Ok(value) => {
                    secrets.push(value.clone());
                    AuthState::Ready(ResolvedAuth::Query {
                        param: param.clone(),
                        value: Secret::new(value),
                    })
                }
                Err(problem) => AuthState::Missing(problem),
            },
        };
        states.insert(endpoint.name.clone(), state);
    }

    (states, Redactor::new(secrets))
}

/// Read one variable, or say why it is unusable.
///
/// A credential with an embedded control character is refused here rather than
/// left for the HTTP client to reject at send time. Two reasons: `\r\n` in a
/// header value is the shape of a header-injection attempt, and a diagnostic
/// naming the variable is far more use to an operator than a transport error
/// three layers down. Note that the message names the *variable*, never the
/// value — the value is exactly what must not be quoted back.
fn read(env: &EnvMap, endpoint: &str, name: &str) -> Result<String, String> {
    let value = env
        .get(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_message(endpoint, name))?;

    if value.chars().any(char::is_control) {
        return Err(format!(
            "endpoint `{endpoint}` reads its credential from `{name}`, whose value contains a \
             control character. Header values may not, so this endpoint is unusable. Re-export \
             the variable without embedded newlines or tabs."
        ));
    }
    Ok(value)
}

fn missing_message(endpoint: &str, variable: &str) -> String {
    format!(
        "endpoint `{endpoint}` needs a credential from `{variable}`, which is unset or empty in \
         the environment of the tdcc process. Export it there and restart the node — it is \
         deliberately not readable from rest-client.toml or [[plugin]].args, both of which are \
         stored on disk and echoed back by `tdcc plugins info`."
    )
}

/// Removes known credential values from any string on its way out.
///
/// This is belt and braces. Nothing in this plugin puts a credential into a
/// message on purpose; the redactor is here so that a `reqwest` error quoting a
/// URL, or a future edit, cannot turn into a leak. The repository holding this
/// plugin is public, so the cost of being wrong here is immediate.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Sorted longest-first so a credential that contains another one is
    /// replaced whole rather than leaving a fragment behind.
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(mut secrets: Vec<String>) -> Self {
        // A very short "secret" would turn every message into a wall of
        // `<redacted>`; anything that short is not protecting anything either.
        secrets.retain(|secret| secret.len() >= 4);
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn redact(&self, message: impl Into<String>) -> String {
        let mut message = message.into();
        for secret in &self.secrets {
            if message.contains(secret.as_str()) {
                message = message.replace(secret.as_str(), "<redacted>");
            }
        }
        message
    }
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

/// Standard base64, RFC 4648 §4, with padding.
///
/// Twenty lines rather than a dependency: HTTP Basic is the only thing in this
/// plugin that needs base64, the alphabet has not changed since 2006, and the
/// test below is the RFC's own vector table.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let triple = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        out.push(ALPHABET[(triple >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn base64_matches_the_rfc_4648_test_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
        // RFC 7617's own example.
        assert_eq!(
            base64_encode(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("super-secret-token");

        let rendered = format!("{secret:?}");

        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_resolved_auth_never_prints_its_credential_either() {
        let auth = ResolvedAuth::Basic {
            username: "operator".into(),
            password: Secret::new("hunter2-hunter2"),
        };

        let rendered = format!("{auth:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("operator"), "{rendered}");
    }

    #[test]
    fn a_present_variable_becomes_a_ready_bearer_credential() {
        let catalog = catalog::parse(catalog::SAMPLE).expect("the sample parses");
        let (states, _) = resolve(
            &catalog,
            &env(&[("TDCC_REST_CLIENT_EXAMPLE_TOKEN", "token-value")]),
        );

        let AuthState::Ready(ResolvedAuth::Bearer(token)) = &states["example"] else {
            panic!("expected a ready bearer credential");
        };
        assert_eq!(token.expose(), "token-value");
    }

    #[test]
    fn a_missing_variable_leaves_the_endpoint_declared_and_names_the_variable() {
        let catalog = catalog::parse(catalog::SAMPLE).expect("the sample parses");

        let (states, _) = resolve(&catalog, &env(&[]));
        let AuthState::Missing(message) = &states["example"] else {
            panic!("expected a missing credential");
        };
        assert!(
            message.contains("TDCC_REST_CLIENT_EXAMPLE_TOKEN"),
            "{message}"
        );
        assert!(message.contains("environment"), "{message}");

        // An empty or whitespace-only variable counts as missing, not as an
        // empty credential that would produce a confusing 401.
        let (states, _) = resolve(&catalog, &env(&[("TDCC_REST_CLIENT_EXAMPLE_TOKEN", "   ")]));
        assert!(!states["example"].is_ready());
    }

    #[test]
    fn bearer_and_basic_produce_the_headers_the_rfcs_specify() {
        let bearer = ResolvedAuth::Bearer(Secret::new("abc123"));
        assert_eq!(
            bearer.header(),
            Some(("Authorization".into(), "Bearer abc123".into()))
        );
        assert_eq!(bearer.query(), None);

        let basic = ResolvedAuth::Basic {
            username: "Aladdin".into(),
            password: Secret::new("open sesame"),
        };
        assert_eq!(
            basic.header(),
            Some((
                "Authorization".into(),
                "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==".into()
            ))
        );
    }

    #[test]
    fn a_query_credential_travels_as_a_query_pair_and_never_as_a_header() {
        let auth = ResolvedAuth::Query {
            param: "api_key".into(),
            value: Secret::new("key-value"),
        };

        assert_eq!(auth.header(), None);
        assert_eq!(auth.query(), Some(("api_key".into(), "key-value".into())));
        assert_eq!(auth.query_param_name(), Some("api_key"));
    }

    #[test]
    fn a_credential_carrying_a_control_character_disables_its_endpoint_without_quoting_it() {
        let catalog = catalog::parse(catalog::SAMPLE).expect("the sample parses");

        let (states, _) = resolve(
            &catalog,
            &env(&[("TDCC_REST_CLIENT_EXAMPLE_TOKEN", "abc\r\nX-Injected: 1")]),
        );

        let AuthState::Missing(message) = &states["example"] else {
            panic!("a header-injection-shaped credential must not be usable");
        };
        assert!(message.contains("control character"), "{message}");
        assert!(!message.contains("X-Injected"), "{message}");
    }

    #[test]
    fn the_redactor_removes_every_resolved_credential_from_a_message() {
        let catalog = catalog::parse(catalog::SAMPLE).expect("the sample parses");
        let (_, redactor) = resolve(
            &catalog,
            &env(&[("TDCC_REST_CLIENT_EXAMPLE_TOKEN", "s3cret-token-value")]),
        );

        let message = redactor.redact(
            "error sending request for url (https://api.example.com/v2/things?k=s3cret-token-value)",
        );

        assert!(!message.contains("s3cret-token-value"), "{message}");
        assert!(message.contains("<redacted>"), "{message}");
    }

    #[test]
    fn the_redactor_replaces_the_longest_match_first() {
        // A short credential that is a prefix of a longer one must not leave
        // the longer one's tail behind.
        let redactor = Redactor::new(vec!["abcd".into(), "abcdefgh".into()]);

        assert_eq!(redactor.redact("abcdefgh"), "<redacted>");
        assert_eq!(redactor.redact("abcdxyz"), "<redacted>xyz");
    }

    #[test]
    fn the_redactor_ignores_values_too_short_to_be_credentials() {
        let redactor = Redactor::new(vec!["ab".into()]);

        assert_eq!(redactor.redact("about"), "about");
    }

    #[test]
    fn the_redactor_never_prints_what_it_is_redacting() {
        let redactor = Redactor::new(vec!["super-secret".into()]);

        let rendered = format!("{redactor:?}");

        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains('1'), "{rendered}");
    }
}
