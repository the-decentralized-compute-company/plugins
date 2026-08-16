//! Turning a named operation and a bag of values into one concrete request.
//!
//! This is the module that has to be right. A caller supplies an endpoint name,
//! an operation name, and parameter values; everything else — scheme, host,
//! port, path shape, method, headers, credential — comes from the operator's
//! declaration. There is no code path here that lets a caller-supplied string
//! become a host.
//!
//! Confinement is checked twice, by two different mechanisms:
//!
//! 1. **Before the URL exists**, each path parameter is refused if it could
//!    change the shape of the path (`/`, `.`, `..`, control characters) and is
//!    then percent-encoded down to the unreserved set.
//! 2. **After the URL exists**, the assembled `Url` is checked against the
//!    declaration: same origin as the base, path still under the base path,
//!    method in the endpoint's list, and the path relative to the base matching
//!    one of the endpoint's `paths` patterns. That second check runs on the URL
//!    that is about to be sent, so it does not matter what any earlier step got
//!    wrong.
//!
//! Everything in this module is pure: no network, no clock, no environment.

use std::collections::BTreeMap;

use reqwest::Url;
use serde_json::Value;

use crate::auth::ResolvedAuth;
use crate::catalog::{Endpoint, Literal, Operation, Parameter, ParameterIn, ParameterType};
use crate::pathmatch;

/// One outbound request, fully resolved.
///
/// `Debug` is hand-written: `headers` may hold an `Authorization` value and
/// `url` may carry an API key in its query string, so the derived form would be
/// a credential printed into whatever log or panic message asked for it.
#[derive(Clone)]
pub struct PreparedRequest {
    pub method: String,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    /// `(content type, bytes)`.
    pub body: Option<(String, Vec<u8>)>,
    /// The URL with any credential in the query string replaced. This is the
    /// only form that goes into a tool result or an error message.
    pub display_url: String,
}

impl std::fmt::Debug for PreparedRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedRequest")
            .field("method", &self.method)
            .field("url", &self.display_url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "body_bytes",
                &self.body.as_ref().map(|(_, bytes)| bytes.len()),
            )
            .finish()
    }
}

/// Build the request for one call, or explain exactly what was wrong with it.
///
/// `params` and `body` are the only caller-controlled inputs.
pub fn build(
    endpoint: &Endpoint,
    operation: &Operation,
    params: &BTreeMap<String, Value>,
    body: Option<&Value>,
    auth: &ResolvedAuth,
) -> Result<PreparedRequest, String> {
    let placed = resolve_parameters(operation, params)?;

    let relative = pathmatch::expand(&operation.path, &placed.path)?;
    let assembled = format!("{}{relative}", endpoint.base_path);
    pathmatch::check_assembled_path(&assembled)?;

    let base = Url::parse(&endpoint.base_url)
        .map_err(|error| format!("endpoint base_url is unusable: {error}"))?;
    let mut url = base.clone();
    url.set_path(&assembled);

    for (name, value) in &placed.query {
        url.query_pairs_mut().append_pair(name, value);
    }
    if let Some((name, value)) = auth.query() {
        url.query_pairs_mut().append_pair(&name, &value);
    }
    // `query_pairs_mut` leaves a trailing `?` behind when nothing was appended.
    if url.query() == Some("") {
        url.set_query(None);
    }

    check_confinement(endpoint, operation, &url, &base)?;

    let mut headers: Vec<(String, String)> = endpoint.headers.clone();
    if let Some((name, value)) = auth.header() {
        headers.push((name, value));
    }

    let body = build_body(endpoint, operation, body)?;
    let display_url = redacted_url(&url, auth);

    Ok(PreparedRequest {
        method: operation.method.clone(),
        url,
        headers,
        body,
        display_url,
    })
}

/// Check the finished URL against the declaration.
///
/// Deliberately last, and deliberately reading from the `Url` rather than from
/// the strings that built it: whatever the expansion produced, this is the
/// thing that will go on the wire.
fn check_confinement(
    endpoint: &Endpoint,
    operation: &Operation,
    url: &Url,
    base: &Url,
) -> Result<(), String> {
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port_or_known_default() != base.port_or_known_default()
    {
        return Err(format!(
            "the assembled request would go to {} rather than the declared base {}. Refused.",
            url.origin().ascii_serialization(),
            base.origin().ascii_serialization()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the assembled request carries embedded credentials. Refused.".to_string());
    }

    let path = url.path();
    pathmatch::check_assembled_path(path)?;

    let relative = match path.strip_prefix(endpoint.base_path.as_str()) {
        Some(relative) if endpoint.base_path.is_empty() || relative.starts_with('/') => relative,
        _ => {
            return Err(format!(
                "the assembled path {path} escaped the base path {}. Refused.",
                endpoint.base_path
            ));
        }
    };

    if !endpoint.methods.contains(&operation.method) {
        return Err(format!(
            "method {} is not allowed on endpoint `{}` ({})",
            operation.method,
            endpoint.name,
            endpoint.methods.join(", ")
        ));
    }
    if !pathmatch::matches_any(&endpoint.paths, relative) {
        return Err(format!(
            "the request path {relative} is not allowed on endpoint `{}`. That endpoint permits \
             {}. Refused.",
            endpoint.name,
            endpoint.paths.join(", ")
        ));
    }
    Ok(())
}

/// Values ready to be placed: path substitutions keyed by placeholder name, and
/// query pairs in declaration order so a request is reproducible.
struct PlacedValues {
    path: BTreeMap<String, String>,
    query: Vec<(String, String)>,
}

/// Validate every supplied value against the declaration and split it by where
/// it goes.
fn resolve_parameters(
    operation: &Operation,
    params: &BTreeMap<String, Value>,
) -> Result<PlacedValues, String> {
    for name in params.keys() {
        if operation.parameter(name).is_none() {
            let declared: Vec<&str> = operation
                .parameters
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect();
            return Err(format!(
                "`{name}` is not a parameter of operation `{}`. Declared parameters: {}. Call \
                 `rest-client.describe` for the full schema.",
                operation.name,
                if declared.is_empty() {
                    "none".to_string()
                } else {
                    declared.join(", ")
                }
            ));
        }
    }

    let mut path_values = BTreeMap::new();
    let mut query_values = Vec::new();

    for parameter in &operation.parameters {
        let rendered = match params.get(&parameter.name) {
            Some(Value::Null) | None => match &parameter.default {
                Some(default) => Some(default.render()),
                None if parameter.required => {
                    return Err(format!(
                        "parameter `{}` is required by operation `{}` but was not supplied: {}",
                        parameter.name, operation.name, parameter.description
                    ));
                }
                None => None,
            },
            Some(value) => Some(render(parameter, value)?),
        };

        let Some(rendered) = rendered else {
            continue;
        };
        match parameter.location {
            ParameterIn::Path => {
                path_values.insert(parameter.name.clone(), rendered);
            }
            ParameterIn::Query => query_values.push((parameter.name.clone(), rendered)),
        }
    }

    Ok(PlacedValues {
        path: path_values,
        query: query_values,
    })
}

/// Check one supplied value against its declared type and constraints, and
/// render it into the string that goes in the URL.
fn render(parameter: &Parameter, value: &Value) -> Result<String, String> {
    let name = &parameter.name;
    let rendered = match parameter.parameter_type {
        ParameterType::String => {
            let text = value.as_str().ok_or_else(|| {
                format!(
                    "parameter `{name}` must be a string, got {}",
                    kind_of(value)
                )
            })?;
            let length = text.chars().count();
            if let Some(min) = parameter.min_length
                && length < min
            {
                return Err(format!(
                    "parameter `{name}` is {length} characters; the minimum is {min}"
                ));
            }
            if let Some(max) = parameter.max_length
                && length > max
            {
                return Err(format!(
                    "parameter `{name}` is {length} characters; the maximum is {max}"
                ));
            }
            text.to_string()
        }
        ParameterType::Integer => {
            let number = value.as_i64().ok_or_else(|| {
                format!(
                    "parameter `{name}` must be a whole number, got {}",
                    kind_of(value)
                )
            })?;
            check_range(parameter, number as f64)?;
            number.to_string()
        }
        ParameterType::Number => {
            let number = value.as_f64().ok_or_else(|| {
                format!(
                    "parameter `{name}` must be a number, got {}",
                    kind_of(value)
                )
            })?;
            if !number.is_finite() {
                return Err(format!("parameter `{name}` must be a finite number"));
            }
            check_range(parameter, number)?;
            Literal::Float(number).render()
        }
        ParameterType::Boolean => {
            let flag = value.as_bool().ok_or_else(|| {
                format!(
                    "parameter `{name}` must be true or false, got {}",
                    kind_of(value)
                )
            })?;
            flag.to_string()
        }
    };

    if !parameter.allowed.is_empty()
        && !parameter
            .allowed
            .iter()
            .any(|allowed| allowed.render() == rendered)
    {
        return Err(format!(
            "parameter `{name}` is `{rendered}`, which the operator did not allow. Permitted \
             values: {}.",
            parameter
                .allowed
                .iter()
                .map(Literal::render)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(rendered)
}

fn check_range(parameter: &Parameter, number: f64) -> Result<(), String> {
    if let Some(min) = parameter.minimum
        && number < min
    {
        return Err(format!(
            "parameter `{}` is {number}; the minimum is {min}",
            parameter.name
        ));
    }
    if let Some(max) = parameter.maximum
        && number > max
    {
        return Err(format!(
            "parameter `{}` is {number}; the maximum is {max}",
            parameter.name
        ));
    }
    Ok(())
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn build_body(
    endpoint: &Endpoint,
    operation: &Operation,
    body: Option<&Value>,
) -> Result<Option<(String, Vec<u8>)>, String> {
    match (&operation.body, body) {
        (None, None) => Ok(None),
        (None, Some(Value::Null)) => Ok(None),
        (None, Some(_)) => Err(format!(
            "operation `{}` does not accept a body. Remove `body` from the call.",
            operation.name
        )),
        (Some(spec), None) | (Some(spec), Some(Value::Null)) => {
            if spec.required {
                Err(format!(
                    "operation `{}` requires a body: {}",
                    operation.name, spec.description
                ))
            } else {
                Ok(None)
            }
        }
        (Some(spec), Some(value)) => {
            let bytes = serde_json::to_vec(value)
                .map_err(|error| format!("the body could not be serialized as JSON: {error}"))?;
            if bytes.len() > endpoint.limits.max_request_bytes {
                return Err(format!(
                    "the body is {} bytes, over the {}-byte limit for endpoint `{}`. Raise \
                     `max_request_bytes` on that endpoint if the limit is wrong.",
                    bytes.len(),
                    endpoint.limits.max_request_bytes,
                    endpoint.name
                ));
            }
            Ok(Some((spec.content_type.clone(), bytes)))
        }
    }
}

/// The URL with a credential in the query string replaced by `<redacted>`.
///
/// Rebuilt pair by pair rather than by string replacement, so the redaction
/// cannot be defeated by an encoding that does not match the raw value.
fn redacted_url(url: &Url, auth: &ResolvedAuth) -> String {
    let Some(secret_param) = auth.query_param_name() else {
        return url.to_string();
    };
    if url.query().is_none() {
        return url.to_string();
    }

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| {
            if name == secret_param {
                (name.into_owned(), "<redacted>".to_string())
            } else {
                (name.into_owned(), value.into_owned())
            }
        })
        .collect();

    let mut redacted = url.clone();
    redacted.set_query(None);
    for (name, value) in pairs {
        redacted.query_pairs_mut().append_pair(&name, &value);
    }
    redacted.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Secret;
    use crate::catalog::{self, Catalog};

    fn sample() -> Catalog {
        catalog::parse(catalog::SAMPLE).expect("the sample parses")
    }

    fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn prepare(
        operation: &str,
        params: &BTreeMap<String, Value>,
        body: Option<&Value>,
        auth: &ResolvedAuth,
    ) -> Result<PreparedRequest, String> {
        let catalog = sample();
        let endpoint = catalog.endpoint("example").expect("declared").clone();
        let operation = endpoint.operation(operation).expect("declared").clone();
        build(&endpoint, &operation, params, body, auth)
    }

    #[test]
    fn a_path_parameter_lands_in_the_path_under_the_base_path() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("abc"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect("a valid call");

        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url.as_str(),
            "https://api.example.com/v2/things/abc"
        );
        assert!(request.body.is_none());
    }

    #[test]
    fn query_parameters_keep_declaration_order_and_defaults_are_applied() {
        let request = prepare("list_things", &params(&[]), None, &ResolvedAuth::None)
            .expect("every parameter has a default");

        assert_eq!(
            request.url.as_str(),
            "https://api.example.com/v2/things?limit=20&state=open"
        );
    }

    #[test]
    fn an_operation_with_no_query_values_produces_no_question_mark() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("1"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect("a valid call");

        assert!(!request.url.as_str().contains('?'), "{}", request.url);
    }

    #[test]
    fn a_parameter_the_operator_did_not_declare_is_refused_with_the_list_of_ones_that_are() {
        let error = prepare(
            "list_things",
            &params(&[("secret", Value::from("x"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("undeclared parameters are refused");

        assert!(error.contains("not a parameter"), "{error}");
        assert!(error.contains("limit, state"), "{error}");
    }

    #[test]
    fn a_missing_required_parameter_is_refused_and_quotes_its_description() {
        let error = prepare("get_thing", &params(&[]), None, &ResolvedAuth::None)
            .expect_err("id is required");

        assert!(error.contains("`id` is required"), "{error}");
        assert!(error.contains("as returned by list_things"), "{error}");
    }

    #[test]
    fn a_value_of_the_wrong_type_names_the_type_it_should_have_been() {
        let error = prepare(
            "list_things",
            &params(&[("limit", Value::from("ten"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("a string is not a whole number");

        assert!(error.contains("whole number"), "{error}");
        assert!(error.contains("got a string"), "{error}");
    }

    #[test]
    fn declared_ranges_and_enums_are_enforced_against_the_supplied_value() {
        let error = prepare(
            "list_things",
            &params(&[("limit", Value::from(1000))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("over the maximum");
        assert!(error.contains("maximum is 100"), "{error}");

        let error = prepare(
            "list_things",
            &params(&[("state", Value::from("deleted"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("outside the enum");
        assert!(error.contains("operator did not allow"), "{error}");
        assert!(error.contains("open, closed, all"), "{error}");
    }

    #[test]
    fn a_path_parameter_cannot_reach_another_path_on_the_same_host() {
        // The dot-segment refusal fires first; even if it did not, the
        // allowlist check on the assembled URL would.
        let error = prepare(
            "get_thing",
            &params(&[("id", Value::from("../../admin/keys"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("traversal is refused");

        assert!(error.contains("path separator"), "{error}");
    }

    #[test]
    fn a_path_parameter_cannot_append_a_query_string() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("1?admin=true"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect("the value is legal once encoded");

        assert_eq!(
            request.url.as_str(),
            "https://api.example.com/v2/things/1%3Fadmin%3Dtrue"
        );
        assert_eq!(request.url.query(), None);
    }

    #[test]
    fn a_path_parameter_cannot_change_the_host() {
        for hostile in [
            "//evil.example.com/x",
            "..//evil.example.com",
            "https://evil.example.com",
        ] {
            let error = prepare(
                "get_thing",
                &params(&[("id", Value::from(hostile))]),
                None,
                &ResolvedAuth::None,
            )
            .expect_err(&format!("{hostile} must be refused"));
            assert!(error.contains("path separator"), "{hostile}: {error}");
        }
    }

    #[test]
    fn an_operation_outside_the_allowlist_is_refused_even_if_it_reached_the_builder() {
        // Simulates a catalog whose reachability check was bypassed: the
        // request-time check on the finished URL is the one that matters.
        let catalog = sample();
        let mut endpoint = catalog.endpoint("example").expect("declared").clone();
        endpoint.paths = vec!["/search".to_string()];
        let operation = endpoint.operation("get_thing").expect("declared").clone();

        let error = build(
            &endpoint,
            &operation,
            &params(&[("id", Value::from("1"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect_err("the assembled path is outside the allowlist");

        assert!(error.contains("not allowed on endpoint"), "{error}");
        assert!(error.contains("/search"), "{error}");
    }

    #[test]
    fn a_method_outside_the_endpoint_list_is_refused_at_request_time_too() {
        let catalog = sample();
        let mut endpoint = catalog.endpoint("example").expect("declared").clone();
        endpoint.methods = vec!["GET".to_string()];
        let operation = endpoint.operation("search").expect("declared").clone();

        let error = build(
            &endpoint,
            &operation,
            &params(&[]),
            Some(&serde_json::json!({"query": "x"})),
            &ResolvedAuth::None,
        )
        .expect_err("POST is not permitted");

        assert!(error.contains("method POST is not allowed"), "{error}");
    }

    #[test]
    fn a_bearer_credential_becomes_a_header_and_never_appears_in_the_url() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("1"))]),
            None,
            &ResolvedAuth::Bearer(Secret::new("token-value")),
        )
        .expect("a valid call");

        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer token-value"),
            "{:?}",
            request.headers
        );
        assert!(!request.display_url.contains("token-value"));
        assert!(request.headers.iter().any(|(name, _)| name == "Accept"));
    }

    #[test]
    fn a_query_credential_is_sent_but_redacted_in_the_url_that_is_reported_back() {
        let request = prepare(
            "list_things",
            &params(&[]),
            None,
            &ResolvedAuth::Query {
                param: "api_key".into(),
                value: Secret::new("key-value"),
            },
        )
        .expect("a valid call");

        assert!(request.url.as_str().contains("api_key=key-value"));
        assert!(
            !request.display_url.contains("key-value"),
            "{}",
            request.display_url
        );
        assert!(
            request.display_url.contains("api_key=%3Credacted%3E"),
            "{}",
            request.display_url
        );
        assert!(
            request.display_url.contains("limit=20"),
            "{}",
            request.display_url
        );
    }

    #[test]
    fn the_debug_form_prints_header_names_but_never_header_values() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("1"))]),
            None,
            &ResolvedAuth::Bearer(Secret::new("token-value")),
        )
        .expect("a valid call");

        let rendered = format!("{request:?}");

        assert!(!rendered.contains("token-value"), "{rendered}");
        assert!(rendered.contains("Authorization"), "{rendered}");
    }

    #[test]
    fn a_declared_body_is_serialized_and_a_body_on_an_operation_without_one_is_refused() {
        let request = prepare(
            "search",
            &params(&[]),
            Some(&serde_json::json!({"query": "rust"})),
            &ResolvedAuth::None,
        )
        .expect("search declares a body");

        let (content_type, bytes) = request.body.expect("a body was built");
        assert_eq!(content_type, "application/json");
        assert_eq!(String::from_utf8(bytes).unwrap(), r#"{"query":"rust"}"#);

        let error = prepare(
            "get_thing",
            &params(&[("id", Value::from("1"))]),
            Some(&serde_json::json!({"x": 1})),
            &ResolvedAuth::None,
        )
        .expect_err("get_thing declares no body");
        assert!(error.contains("does not accept a body"), "{error}");
    }

    #[test]
    fn a_required_body_that_is_missing_is_refused_and_quotes_its_description() {
        let error = prepare("search", &params(&[]), None, &ResolvedAuth::None)
            .expect_err("search requires a body");

        assert!(error.contains("requires a body"), "{error}");
        assert!(error.contains("`query` string"), "{error}");
    }

    #[test]
    fn an_over_large_body_is_refused_and_names_the_setting_that_raises_the_limit() {
        let catalog = sample();
        let mut endpoint = catalog.endpoint("example").expect("declared").clone();
        endpoint.limits.max_request_bytes = 64;
        let operation = endpoint.operation("search").expect("declared").clone();

        let error = build(
            &endpoint,
            &operation,
            &params(&[]),
            Some(&serde_json::json!({ "query": "x".repeat(200) })),
            &ResolvedAuth::None,
        )
        .expect_err("over the request cap");

        assert!(error.contains("max_request_bytes"), "{error}");
    }

    #[test]
    fn a_base_url_with_a_path_prefix_is_kept_and_the_allowlist_is_relative_to_it() {
        let request = prepare(
            "get_thing",
            &params(&[("id", Value::from("7"))]),
            None,
            &ResolvedAuth::None,
        )
        .expect("a valid call");

        // `/v2` came from base_url; `/things/7` is what the allowlist saw.
        assert_eq!(request.url.path(), "/v2/things/7");
    }

    #[test]
    fn an_explicit_null_is_treated_as_absent_rather_than_as_a_value() {
        let request = prepare(
            "list_things",
            &params(&[("state", Value::Null)]),
            None,
            &ResolvedAuth::None,
        )
        .expect("null falls back to the default");

        assert!(
            request.url.as_str().contains("state=open"),
            "{}",
            request.url
        );
    }
}
