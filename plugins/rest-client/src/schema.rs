//! Turning the operator's declaration into something a model can read.
//!
//! The SDK builds a tool's `inputSchema` from a Rust type at compile time, so
//! `call`'s schema cannot itself change shape per endpoint — see "Why the
//! schema is in the description" in README.md. What *can* change is the tool's
//! description, which is a runtime `String`, and the payloads of `endpoints`
//! and `describe`. So the declared parameters reach a model three ways:
//!
//! 1. [`render_catalog`] writes every operation's signature into the `call`
//!    tool description, which a model reads before it calls anything.
//! 2. [`operation_schema`] emits a real JSON Schema object per operation,
//!    returned by `describe`.
//! 3. `request.rs` validates against the same declaration and names the
//!    offending parameter, so a wrong call is corrected rather than guessed at.
//!
//! Everything here is pure formatting.

use serde_json::{Value, json};

use crate::catalog::{Catalog, Endpoint, Operation, Parameter, ParameterIn};

/// Ceiling on the generated `call` description. A model pays context for this
/// on every request, so a node with 32 endpoints does not get to spend it all.
pub const MAX_DESCRIPTION_CHARS: usize = 6_000;

/// A JSON Schema object describing one operation's `params`.
///
/// `additionalProperties: false` matches what `request.rs` enforces: a
/// parameter the operator did not declare is an error, not something quietly
/// appended to the query string.
pub fn operation_schema(operation: &Operation) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    for parameter in &operation.parameters {
        properties.insert(parameter.name.clone(), parameter_schema(parameter));
        if parameter.required {
            required.push(Value::String(parameter.name.clone()));
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    Value::Object(schema)
}

fn parameter_schema(parameter: &Parameter) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!(parameter.parameter_type.as_str()));
    schema.insert("description".into(), json!(parameter.description));
    if !parameter.allowed.is_empty() {
        schema.insert(
            "enum".into(),
            Value::Array(
                parameter
                    .allowed
                    .iter()
                    .map(|value| value.to_json())
                    .collect(),
            ),
        );
    }
    if let Some(default) = &parameter.default {
        schema.insert("default".into(), default.to_json());
    }
    if let Some(value) = parameter.min_length {
        schema.insert("minLength".into(), json!(value));
    }
    if let Some(value) = parameter.max_length {
        schema.insert("maxLength".into(), json!(value));
    }
    if let Some(value) = parameter.minimum {
        schema.insert("minimum".into(), json!(value));
    }
    if let Some(value) = parameter.maximum {
        schema.insert("maximum".into(), json!(value));
    }
    // Not part of JSON Schema, but the one thing a caller most needs to know
    // that a schema cannot say: whether this value lands in the path or the
    // query. It changes which values are legal.
    schema.insert("x-in".into(), json!(parameter.location.as_str()));
    Value::Object(schema)
}

/// `endpoint.operation` — the name a caller passes to `call`, as one string.
pub fn qualified_name(endpoint: &Endpoint, operation: &Operation) -> String {
    format!("{}.{}", endpoint.name, operation.name)
}

/// One line: `example.get_thing — GET /things/{id}`.
pub fn signature(endpoint: &Endpoint, operation: &Operation) -> String {
    format!(
        "{} — {} {}",
        qualified_name(endpoint, operation),
        operation.method,
        operation.path
    )
}

/// The generated part of the `call` tool description.
///
/// Written for a model deciding what to call: every operation, its method and
/// path, and every parameter with its type, whether it is required, where it
/// goes, and the operator's own words about it. Truncated at
/// [`MAX_DESCRIPTION_CHARS`] with a pointer at the tools that page through the
/// rest, because silently dropping half a catalog would be worse.
pub fn render_catalog(catalog: &Catalog) -> String {
    if catalog.endpoints.is_empty() {
        return "No endpoints are declared, so there is nothing to call. The node's operator \
                declares them in rest-client.toml; `rest-client.status` reports where that file \
                is expected and why it is empty."
            .to_string();
    }

    let mut out = String::new();
    let mut rendered = 0usize;
    let mut total = 0usize;
    let mut truncated = false;

    for endpoint in &catalog.endpoints {
        total += endpoint.operations.len();
    }

    'outer: for endpoint in &catalog.endpoints {
        let heading = format!("\n{}: {}\n", endpoint.name, endpoint.description);
        if out.len() + heading.len() > MAX_DESCRIPTION_CHARS {
            truncated = true;
            break;
        }
        out.push_str(&heading);

        for operation in &endpoint.operations {
            let block = render_operation(endpoint, operation);
            if out.len() + block.len() > MAX_DESCRIPTION_CHARS {
                truncated = true;
                break 'outer;
            }
            out.push_str(&block);
            rendered += 1;
        }
    }

    if truncated {
        out.push_str(&format!(
            "\n… {} of {total} operations are listed above. Call `rest-client.endpoints` for the \
             rest and `rest-client.describe` for one operation's full parameter schema.\n",
            rendered
        ));
    }
    out
}

fn render_operation(endpoint: &Endpoint, operation: &Operation) -> String {
    // The signature gets a line of its own because it is what a model scans;
    // the description sits underneath, where it is read once the scan stops.
    let mut block = format!(
        "  {}\n      {}\n",
        signature(endpoint, operation),
        operation.description
    );
    for parameter in &operation.parameters {
        block.push_str(&format!("      {}\n", render_parameter(parameter)));
    }
    // Only said when true. "Takes no body" on every GET in a catalog of sixty
    // is context spent to say nothing.
    if let Some(body) = &operation.body {
        block.push_str(&format!(
            "      body ({}, {}) {}\n",
            body.content_type,
            if body.required {
                "required"
            } else {
                "optional"
            },
            body.description
        ));
    }
    block
}

fn render_parameter(parameter: &Parameter) -> String {
    let mut facets = vec![
        parameter.location.as_str().to_string(),
        parameter.parameter_type.as_str().to_string(),
    ];
    facets.push(
        if parameter.required {
            "required"
        } else {
            "optional"
        }
        .to_string(),
    );
    if let Some(default) = &parameter.default {
        facets.push(format!("default {}", default.render()));
    }
    if !parameter.allowed.is_empty() {
        facets.push(format!(
            "one of: {}",
            parameter
                .allowed
                .iter()
                .map(|value| value.render())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let (Some(min), Some(max)) = (parameter.minimum, parameter.maximum) {
        facets.push(format!("{min}–{max}"));
    }
    format!(
        "- {} ({}) {}",
        parameter.name,
        facets.join(", "),
        parameter.description
    )
}

/// The compact per-endpoint payload returned by the `endpoints` tool.
///
/// It reports the auth *kind* and the environment variable name, never a value:
/// a variable name is a diagnostic, its contents are a credential.
pub fn endpoint_summary(endpoint: &Endpoint, auth_ready: bool) -> Value {
    json!({
        "name": endpoint.name,
        "description": endpoint.description,
        "base_url": endpoint.base_url,
        "methods": endpoint.methods,
        "allowed_paths": endpoint.paths,
        "auth": {
            "kind": endpoint.auth.kind(),
            "env": endpoint.auth.env_name(),
            "ready": auth_ready,
        },
        "limits": {
            "timeout_secs": endpoint.limits.timeout_secs,
            "max_response_bytes": endpoint.limits.max_response_bytes,
            "max_request_bytes": endpoint.limits.max_request_bytes,
            "max_calls_per_minute": endpoint.limits.max_calls_per_minute,
        },
        "allow_private_base": endpoint.allow_private_base,
        "allow_insecure_auth": endpoint.allow_insecure_auth,
        "operations": endpoint
            .operations
            .iter()
            .map(|operation| json!({
                "name": operation.name,
                "call_as": qualified_name(endpoint, operation),
                "method": operation.method,
                "path": operation.path,
                "description": operation.description,
                "takes_body": operation.body.is_some(),
                "parameters": operation
                    .parameters
                    .iter()
                    .map(|parameter| json!({
                        "name": parameter.name,
                        "in": parameter.location.as_str(),
                        "type": parameter.parameter_type.as_str(),
                        "required": parameter.required,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The full per-operation payload returned by the `describe` tool.
pub fn operation_detail(endpoint: &Endpoint, operation: &Operation) -> Value {
    json!({
        "endpoint": endpoint.name,
        "operation": operation.name,
        "call_as": qualified_name(endpoint, operation),
        "method": operation.method,
        "path": operation.path,
        "description": operation.description,
        "params_schema": operation_schema(operation),
        "body": operation.body.as_ref().map(|body| json!({
            "required": body.required,
            "description": body.description,
            "content_type": body.content_type,
        })),
        "path_parameters": operation
            .parameters
            .iter()
            .filter(|parameter| parameter.location == ParameterIn::Path)
            .map(|parameter| parameter.name.clone())
            .collect::<Vec<_>>(),
        "query_parameters": operation
            .parameters
            .iter()
            .filter(|parameter| parameter.location == ParameterIn::Query)
            .map(|parameter| parameter.name.clone())
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    fn sample() -> Catalog {
        catalog::parse(catalog::SAMPLE).expect("the sample parses")
    }

    #[test]
    fn an_operation_schema_is_a_json_schema_object_with_the_declared_facets() {
        let catalog = sample();
        let endpoint = catalog.endpoint("example").expect("declared");
        let operation = endpoint.operation("list_things").expect("declared");

        let schema = operation_schema(operation);

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["limit"]["minimum"], 1.0);
        assert_eq!(schema["properties"]["limit"]["maximum"], 100.0);
        assert_eq!(schema["properties"]["limit"]["default"], 20);
        assert_eq!(schema["properties"]["state"]["enum"][0], "open");
        assert_eq!(schema["properties"]["state"]["x-in"], "query");
        assert_eq!(
            schema["properties"]["limit"]["description"],
            "How many things to return."
        );
        // No parameter of list_things is required, so no `required` key at all
        // rather than an empty array.
        assert!(schema.get("required").is_none(), "{schema:#}");
    }

    #[test]
    fn a_required_path_parameter_lands_in_the_schemas_required_list() {
        let catalog = sample();
        let endpoint = catalog.endpoint("example").expect("declared");
        let operation = endpoint.operation("get_thing").expect("declared");

        let schema = operation_schema(operation);

        assert_eq!(schema["required"], json!(["id"]));
        assert_eq!(schema["properties"]["id"]["x-in"], "path");
    }

    #[test]
    fn the_rendered_catalog_carries_every_signature_and_the_operator_descriptions() {
        let rendered = render_catalog(&sample());

        assert!(
            rendered.contains("example.get_thing — GET /things/{id}"),
            "{rendered}"
        );
        assert!(
            rendered.contains("example.list_things — GET /things"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Fetch one thing by its identifier."),
            "{rendered}"
        );
        assert!(rendered.contains("one of: open, closed, all"), "{rendered}");
        assert!(rendered.contains("default 20"), "{rendered}");
        assert!(
            rendered
                .contains("body (application/json, required) A JSON object with a `query` string."),
            "{rendered}"
        );
        // The common case says nothing rather than "takes no body" sixty times.
        assert!(!rendered.contains("no body"), "{rendered}");
        assert!(
            rendered.len() <= MAX_DESCRIPTION_CHARS,
            "{}",
            rendered.len()
        );
    }

    /// The declaration shown under "A worked example" in README.md, verbatim.
    /// The test below renders it and asserts the exact output the README
    /// prints, so the two cannot drift apart.
    const README_EXAMPLE: &str = include_str!("../README.md");

    #[test]
    fn the_readme_example_renders_exactly_what_the_readme_shows() {
        let document = fenced_block(README_EXAMPLE, "<!-- example:declaration -->");
        let expected = fenced_block(README_EXAMPLE, "<!-- example:rendered -->");
        let catalog = catalog::parse(&document)
            .unwrap_or_else(|error| panic!("the README declaration must parse:\n{error}"));

        let rendered = render_catalog(&catalog);

        assert_eq!(rendered.trim_end(), expected.trim_end(), "\n{rendered}");
    }

    /// Pull the fenced code block that follows a marker comment in the README.
    fn fenced_block(readme: &str, marker: &str) -> String {
        let after = readme
            .split_once(marker)
            .unwrap_or_else(|| panic!("README.md has no {marker} marker"))
            .1;
        let body = after
            .split_once("```")
            .expect("a fenced block follows the marker")
            .1;
        let body = body
            .split_once('\n')
            .expect("the fence has a language tag")
            .1;
        body.split_once("```")
            .expect("the fenced block is closed")
            .0
            .to_string()
    }

    #[test]
    fn an_empty_catalog_says_so_rather_than_rendering_nothing() {
        let rendered = render_catalog(&Catalog::default());

        assert!(rendered.contains("No endpoints are declared"), "{rendered}");
        assert!(rendered.contains("rest-client.toml"), "{rendered}");
    }

    #[test]
    fn a_catalog_too_large_to_render_is_truncated_with_a_pointer_at_the_other_tools() {
        // 32 endpoints × 64 operations is the configured ceiling, and well over
        // the description budget.
        let mut document = String::from("version = 1\n");
        for endpoint in 0..catalog::MAX_ENDPOINTS {
            document.push_str(&format!(
                r#"
[[endpoint]]
name = "e{endpoint}"
description = "Endpoint number {endpoint} with a description long enough to take up real space."
base_url = "https://api.example.com"
methods = ["GET"]
paths = ["/**"]
"#
            ));
            for operation in 0..8 {
                document.push_str(&format!(
                    r#"
[[endpoint.operation]]
name = "op{operation}"
description = "Operation {operation}, described at the sort of length an operator actually writes."
method = "GET"
path = "/e{endpoint}/op{operation}"
"#
                ));
            }
        }
        let catalog = catalog::parse(&document).expect("a large but valid document");

        let rendered = render_catalog(&catalog);

        assert!(
            rendered.len() <= MAX_DESCRIPTION_CHARS + 200,
            "{}",
            rendered.len()
        );
        assert!(rendered.contains("rest-client.endpoints"), "{rendered}");
        assert!(rendered.contains("of 256 operations"), "{rendered}");
    }

    #[test]
    fn an_endpoint_summary_reports_the_auth_variable_name_and_never_a_value() {
        let catalog = sample();
        let endpoint = catalog.endpoint("example").expect("declared");

        let summary = endpoint_summary(endpoint, false);

        assert_eq!(summary["auth"]["kind"], "bearer");
        assert_eq!(summary["auth"]["env"], "TDCC_REST_CLIENT_EXAMPLE_TOKEN");
        assert_eq!(summary["auth"]["ready"], false);
        assert_eq!(summary["operations"][0]["call_as"], "example.get_thing");
        let rendered = summary.to_string();
        assert!(!rendered.contains("token-value"), "{rendered}");
    }

    #[test]
    fn an_operation_detail_separates_path_and_query_parameters() {
        let catalog = sample();
        let endpoint = catalog.endpoint("example").expect("declared");

        let detail = operation_detail(endpoint, endpoint.operation("get_thing").unwrap());
        assert_eq!(detail["path_parameters"], json!(["id"]));
        assert_eq!(detail["query_parameters"], json!([]));
        assert_eq!(detail["body"], Value::Null);

        let detail = operation_detail(endpoint, endpoint.operation("list_things").unwrap());
        assert_eq!(detail["query_parameters"], json!(["limit", "state"]));

        let detail = operation_detail(endpoint, endpoint.operation("search").unwrap());
        assert_eq!(detail["body"]["required"], true);
        assert_eq!(detail["body"]["content_type"], "application/json");
    }
}
