//! The endpoint declaration: what an operator writes, and what it is allowed
//! to say.
//!
//! This is the whole security model in one file. A model never supplies a URL;
//! it names an endpoint and an operation that the operator wrote down here, and
//! passes values for parameters the operator declared. An unrestricted HTTP
//! tool is a server-side request forgery primitive handed to a language model,
//! and the difference between that and this plugin is entirely the fact that
//! this document exists.
//!
//! Three decisions worth reading before changing anything:
//!
//! * **Unknown keys are errors.** Every table sets `deny_unknown_fields`. A
//!   silently ignored `method = "POST"` is a permission the operator does not
//!   know they granted, or a restriction they believe they applied.
//! * **Validation is all-or-nothing.** A document with one bad endpoint
//!   produces no catalog at all. Loading the rest would leave a machine serving
//!   a subset of the operator's intent without saying which subset.
//! * **No credential may appear in this file.** Auth carries the *name* of an
//!   environment variable, never a value, and a field that looks like a key is
//!   rejected at load time. This file is the kind of thing people paste into an
//!   issue.
//!
//! Nothing in this module touches the network, the filesystem, or the clock:
//! `&str` in, [`Catalog`] or a list of complaints out.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::pathmatch;

/// The only document version this build understands.
pub const CATALOG_VERSION: u32 = 1;

/// Bounds on the document itself. Every one of these exists because the parsed
/// catalog is held in memory for the life of the process and rendered into the
/// `call` tool's description, which a model pays context for.
pub const MAX_ENDPOINTS: usize = 32;
pub const MAX_OPERATIONS: usize = 64;
pub const MAX_PARAMETERS: usize = 32;
pub const MAX_PATH_PATTERNS: usize = 64;
pub const MAX_STATIC_HEADERS: usize = 16;
pub const MAX_ENUM_VALUES: usize = 64;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_DESCRIPTION_LEN: usize = 500;
pub const MAX_ENV_NAME_LEN: usize = 128;

pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
pub const MIN_TIMEOUT_SECS: u64 = 1;
pub const MAX_TIMEOUT_SECS: u64 = 120;

pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MIN_MAX_RESPONSE_BYTES: usize = 1_024;
pub const CEILING_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MIN_MAX_REQUEST_BYTES: usize = 64;
pub const CEILING_MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub const DEFAULT_MAX_CALLS_PER_MINUTE: u32 = 60;
pub const MIN_MAX_CALLS_PER_MINUTE: u32 = 1;
pub const CEILING_MAX_CALLS_PER_MINUTE: u32 = 6_000;

/// Methods an operator may allow. `CONNECT`, `TRACE`, and anything else are not
/// on this list and cannot be added through configuration.
pub const SUPPORTED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Methods that may carry a request body.
const BODY_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

/// Headers an operator may not set from the document, because this plugin owns
/// them: three are how credentials travel, and the rest are framing that the
/// HTTP client computes.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "upgrade",
];

// ---------------------------------------------------------------------------
// Validated types
// ---------------------------------------------------------------------------

/// A scalar an operator may write as an `enum` entry or a `default`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Literal {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

impl Literal {
    /// The form that goes into a URL. This is also the form used for equality
    /// against an `enum`, so an operator's `1` and a caller's `1` agree.
    pub fn render(&self) -> String {
        match self {
            Self::Bool(value) => value.to_string(),
            Self::Integer(value) => value.to_string(),
            Self::Float(value) => value.to_string(),
            Self::Text(value) => value.clone(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::Integer(value) => serde_json::Value::from(*value),
            Self::Float(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Self::Text(value) => serde_json::Value::String(value.clone()),
        }
    }

    fn type_of(&self) -> ParameterType {
        match self {
            Self::Bool(_) => ParameterType::Boolean,
            Self::Integer(_) => ParameterType::Integer,
            Self::Float(_) => ParameterType::Number,
            Self::Text(_) => ParameterType::String,
        }
    }
}

/// Where a parameter goes in the request.
///
/// There is no `header` location, and that is deliberate: a caller-controlled
/// header is how request smuggling and credential overwriting start, and no
/// declared API needs one badly enough to be worth it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterIn {
    Path,
    Query,
}

impl ParameterIn {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "path" => Ok(Self::Path),
            "query" => Ok(Self::Query),
            other => Err(format!(
                "unknown parameter location {other:?}; expected \"path\" or \"query\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Query => "query",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterType {
    String,
    Integer,
    Number,
    Boolean,
}

impl ParameterType {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "string" => Ok(Self::String),
            "integer" | "int" => Ok(Self::Integer),
            "number" | "float" => Ok(Self::Number),
            "boolean" | "bool" => Ok(Self::Boolean),
            other => Err(format!(
                "unknown parameter type {other:?}; expected \"string\", \"integer\", \"number\", \
                 or \"boolean\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }

    /// Whether a literal of `other` is acceptable where `self` is declared.
    /// An integer is accepted where a number is declared; nothing else widens.
    fn accepts(self, other: ParameterType) -> bool {
        self == other || (self == Self::Number && other == Self::Integer)
    }
}

/// One declared parameter — the OpenAPI-ish part of the document.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub location: ParameterIn,
    pub parameter_type: ParameterType,
    pub required: bool,
    /// Becomes the `description` a model reads in the generated schema, so the
    /// document refuses to declare a parameter without one.
    pub description: String,
    pub allowed: Vec<Literal>,
    pub default: Option<Literal>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

/// A declared request body. The plugin never invents one, and never accepts one
/// for an operation that did not declare it.
#[derive(Debug, Clone, PartialEq)]
pub struct BodySpec {
    pub required: bool,
    pub description: String,
    pub content_type: String,
}

/// One thing a model may ask this node to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub name: String,
    pub description: String,
    pub method: String,
    /// A path template such as `/repos/{owner}/{repo}/issues`, relative to the
    /// endpoint's base URL path.
    pub path: String,
    pub parameters: Vec<Parameter>,
    pub body: Option<BodySpec>,
}

impl Operation {
    pub fn parameter(&self, name: &str) -> Option<&Parameter> {
        self.parameters
            .iter()
            .find(|parameter| parameter.name == name)
    }
}

/// How this plugin authenticates to an endpoint.
///
/// Every variant holds the *name* of an environment variable, never a value.
/// Resolution happens once at startup, in `resolve.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecl {
    None,
    Bearer {
        token_env: String,
    },
    Basic {
        username: String,
        password_env: String,
    },
    Header {
        header: String,
        value_env: String,
    },
    Query {
        param: String,
        value_env: String,
    },
}

impl AuthDecl {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer { .. } => "bearer",
            Self::Basic { .. } => "basic",
            Self::Header { .. } => "header",
            Self::Query { .. } => "query",
        }
    }

    /// The environment variable this endpoint reads its credential from, for
    /// diagnostics. A variable *name* is not a secret; its value is.
    pub fn env_name(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Bearer { token_env } => Some(token_env),
            Self::Basic { password_env, .. } => Some(password_env),
            Self::Header { value_env, .. } => Some(value_env),
            Self::Query { value_env, .. } => Some(value_env),
        }
    }
}

/// Bounds every call to one endpoint runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointLimits {
    pub timeout_secs: u64,
    pub max_response_bytes: usize,
    pub max_request_bytes: usize,
    pub max_calls_per_minute: u32,
}

impl Default for EndpointLimits {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_calls_per_minute: DEFAULT_MAX_CALLS_PER_MINUTE,
        }
    }
}

/// One API the operator has decided this node may talk to.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub name: String,
    pub description: String,
    /// Scheme, host, port, and a path prefix. Never carries a query, a
    /// fragment, or userinfo — all three are rejected at load time.
    pub base_url: String,
    /// The base URL's path with any trailing `/` removed, so `""` for a root
    /// base and `/v2` for `https://host/v2/`.
    pub base_path: String,
    pub methods: Vec<String>,
    /// Patterns matched against the request path *relative to `base_path`* —
    /// the same string space the operation templates are written in.
    pub paths: Vec<String>,
    pub headers: Vec<(String, String)>,
    pub auth: AuthDecl,
    pub limits: EndpointLimits,
    /// Set only when the operator means a service on their own network.
    pub allow_private_base: bool,
    /// Set only when the operator accepts sending a credential in cleartext.
    pub allow_insecure_auth: bool,
    pub operations: Vec<Operation>,
}

impl Endpoint {
    pub fn operation(&self, name: &str) -> Option<&Operation> {
        self.operations
            .iter()
            .find(|operation| operation.name == name)
    }
}

/// The whole document, validated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub endpoints: Vec<Endpoint>,
}

impl Catalog {
    pub fn endpoint(&self, name: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.name == name)
    }

    pub fn names(&self) -> Vec<String> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.name.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The document as written
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    version: u32,
    #[serde(default)]
    endpoint: Vec<RawEndpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEndpoint {
    name: String,
    description: String,
    base_url: String,
    methods: Vec<String>,
    paths: Vec<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    auth: Option<RawAuth>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_response_bytes: Option<usize>,
    #[serde(default)]
    max_request_bytes: Option<usize>,
    #[serde(default)]
    max_calls_per_minute: Option<u32>,
    #[serde(default)]
    allow_private_base: bool,
    #[serde(default)]
    allow_insecure_auth: bool,
    #[serde(default)]
    operation: Vec<RawOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    kind: String,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    param: Option<String>,
    #[serde(default)]
    value_env: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperation {
    name: String,
    description: String,
    method: String,
    path: String,
    #[serde(default)]
    parameter: Vec<RawParameter>,
    #[serde(default)]
    body: Option<RawBody>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBody {
    #[serde(default)]
    required: bool,
    description: String,
    #[serde(default)]
    content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParameter {
    name: String,
    #[serde(rename = "in")]
    location: String,
    #[serde(rename = "type")]
    parameter_type: String,
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(rename = "enum", default)]
    allowed: Vec<Literal>,
    #[serde(default)]
    default: Option<Literal>,
    #[serde(default)]
    min_length: Option<usize>,
    #[serde(default)]
    max_length: Option<usize>,
    #[serde(default)]
    minimum: Option<f64>,
    #[serde(default)]
    maximum: Option<f64>,
}

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/// Parse and validate a declaration document.
///
/// The error is every complaint found, joined, so an operator fixes the file
/// once rather than once per restart.
pub fn parse(text: &str) -> Result<Catalog, String> {
    let raw: RawCatalog = toml::from_str(text).map_err(|error| error.to_string())?;
    if raw.version != CATALOG_VERSION {
        return Err(format!(
            "version is {}; this build of rest-client understands version {CATALOG_VERSION} only",
            raw.version
        ));
    }
    if raw.endpoint.len() > MAX_ENDPOINTS {
        return Err(format!(
            "{} endpoints are declared; the limit is {MAX_ENDPOINTS}",
            raw.endpoint.len()
        ));
    }

    let mut problems: Vec<String> = Vec::new();
    let mut endpoints: Vec<Endpoint> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for raw_endpoint in raw.endpoint {
        let label = raw_endpoint.name.clone();
        match validate_endpoint(raw_endpoint) {
            Ok(endpoint) => {
                if !seen.insert(endpoint.name.clone()) {
                    problems.push(format!("endpoint {:?} is declared twice", endpoint.name));
                }
                endpoints.push(endpoint);
            }
            Err(errors) => problems.extend(
                errors
                    .into_iter()
                    .map(|error| format!("endpoint {label:?}: {error}")),
            ),
        }
    }

    if problems.is_empty() {
        Ok(Catalog { endpoints })
    } else {
        Err(problems.join("\n"))
    }
}

fn validate_endpoint(raw: RawEndpoint) -> Result<Endpoint, Vec<String>> {
    let mut problems: Vec<String> = Vec::new();

    if let Err(error) = validate_identifier("endpoint name", &raw.name) {
        problems.push(error);
    }
    if let Err(error) = validate_description("description", &raw.description) {
        problems.push(error);
    }

    let (base_url, base_path, is_https) = match validate_base_url(&raw.base_url) {
        Ok(parts) => parts,
        Err(error) => {
            problems.push(error);
            (String::new(), String::new(), false)
        }
    };

    let methods = match validate_methods(&raw.methods) {
        Ok(methods) => methods,
        Err(error) => {
            problems.push(error);
            Vec::new()
        }
    };

    let paths = match validate_paths(&raw.paths) {
        Ok(paths) => paths,
        Err(error) => {
            problems.push(error);
            Vec::new()
        }
    };

    let headers = match validate_headers(&raw.headers) {
        Ok(headers) => headers,
        Err(error) => {
            problems.push(error);
            Vec::new()
        }
    };

    let auth = match raw.auth.as_ref().map(validate_auth) {
        None => AuthDecl::None,
        Some(Ok(auth)) => auth,
        Some(Err(error)) => {
            problems.push(error);
            AuthDecl::None
        }
    };

    if auth != AuthDecl::None && !is_https && !raw.allow_insecure_auth && !base_url.is_empty() {
        problems.push(format!(
            "auth is configured but base_url is cleartext http ({base_url}). A credential sent \
             over http is readable by anything on the path. Use https, or set \
             `allow_insecure_auth = true` on this endpoint if it is a service on your own machine \
             and you mean it."
        ));
    }

    let limits = match validate_limits(&raw) {
        Ok(limits) => limits,
        Err(errors) => {
            problems.extend(errors);
            EndpointLimits::default()
        }
    };

    if raw.operation.len() > MAX_OPERATIONS {
        problems.push(format!(
            "{} operations are declared; the limit is {MAX_OPERATIONS}",
            raw.operation.len()
        ));
    }

    let mut operations: Vec<Operation> = Vec::new();
    let mut seen_operations: BTreeSet<String> = BTreeSet::new();
    for raw_operation in raw.operation {
        let label = raw_operation.name.clone();
        match validate_operation(raw_operation, &methods) {
            Ok(operation) => {
                if !seen_operations.insert(operation.name.clone()) {
                    problems.push(format!("operation {:?} is declared twice", operation.name));
                }
                if !paths.is_empty()
                    && let Err(error) = check_reachable(&operation, &paths)
                {
                    problems.push(error);
                }
                operations.push(operation);
            }
            Err(errors) => problems.extend(
                errors
                    .into_iter()
                    .map(|error| format!("operation {label:?}: {error}")),
            ),
        }
    }

    if operations.is_empty() && problems.is_empty() {
        problems.push(
            "no operations are declared, so nothing on this endpoint can be called. Declare at \
             least one [[endpoint.operation]]."
                .to_string(),
        );
    }

    if problems.is_empty() {
        Ok(Endpoint {
            name: raw.name,
            description: raw.description.trim().to_string(),
            base_url,
            base_path,
            methods,
            paths,
            headers,
            auth,
            limits,
            allow_private_base: raw.allow_private_base,
            allow_insecure_auth: raw.allow_insecure_auth,
            operations,
        })
    } else {
        Err(problems)
    }
}

/// Split a base URL into `(normalized url, path prefix, is_https)`.
///
/// Rejecting a query, a fragment, and userinfo here is what lets the request
/// builder treat the base as an inert prefix rather than something it has to
/// merge with.
fn validate_base_url(raw: &str) -> Result<(String, String, bool), String> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|error| format!("base_url {raw:?} is not a URL: {error}"))?;
    let scheme = url.scheme();
    if !matches!(scheme, "http" | "https") {
        return Err(format!(
            "base_url scheme is {scheme:?}; only http and https are supported"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "base_url carries embedded credentials. Put the credential in an environment variable \
             and declare [endpoint.auth] instead."
                .to_string(),
        );
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("base_url has no host".to_string());
    }
    if url.query().is_some() {
        return Err(
            "base_url carries a query string. Declare query parameters on the operation instead."
                .to_string(),
        );
    }
    if url.fragment().is_some() {
        return Err("base_url carries a fragment".to_string());
    }

    let path = url.path().trim_end_matches('/').to_string();
    if !path.is_empty() {
        pathmatch::check_assembled_path(&path)
            .map_err(|error| format!("base_url path: {error}"))?;
    }

    let mut normalized = url.clone();
    normalized.set_path(url.path());
    Ok((
        normalized.as_str().trim_end_matches('/').to_string(),
        path,
        scheme == "https",
    ))
}

fn validate_methods(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err(
            "methods is empty, so nothing can be called. List the methods this endpoint may be \
             used with, for example methods = [\"GET\"]."
                .to_string(),
        );
    }
    let mut methods: Vec<String> = Vec::new();
    for method in raw {
        let upper = method.trim().to_ascii_uppercase();
        if !SUPPORTED_METHODS.contains(&upper.as_str()) {
            return Err(format!(
                "method {method:?} is not supported; expected one of {}",
                SUPPORTED_METHODS.join(", ")
            ));
        }
        if !methods.contains(&upper) {
            methods.push(upper);
        }
    }
    Ok(methods)
}

fn validate_paths(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err(
            "paths is empty, so no request path is allowed. List the path patterns this endpoint \
             may be used with, for example paths = [\"/repos/**\"]."
                .to_string(),
        );
    }
    if raw.len() > MAX_PATH_PATTERNS {
        return Err(format!(
            "{} path patterns are declared; the limit is {MAX_PATH_PATTERNS}",
            raw.len()
        ));
    }
    let mut paths = Vec::new();
    for pattern in raw {
        let pattern = pattern.trim();
        if !pattern.starts_with('/') {
            return Err(format!(
                "path pattern {pattern:?} must start with `/` and is relative to the base URL path"
            ));
        }
        if pattern.contains("://") || pattern.contains('?') || pattern.contains('#') {
            return Err(format!(
                "path pattern {pattern:?} looks like a URL. Patterns are paths only; the host \
                 comes from base_url."
            ));
        }
        if pattern.split('/').any(|segment| segment == "..") {
            return Err(format!("path pattern {pattern:?} has a `..` segment"));
        }
        paths.push(pattern.to_string());
    }
    Ok(paths)
}

fn validate_headers(raw: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, String> {
    if raw.len() > MAX_STATIC_HEADERS {
        return Err(format!(
            "{} static headers are declared; the limit is {MAX_STATIC_HEADERS}",
            raw.len()
        ));
    }
    let mut headers = Vec::new();
    for (name, value) in raw {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
        {
            return Err(format!(
                "header name {name:?} is not a valid HTTP token (letters, digits, `-`, `_`)"
            ));
        }
        if RESERVED_HEADERS.contains(&lower.as_str()) {
            return Err(format!(
                "header {name:?} is set by rest-client itself and may not be declared here. \
                 Credentials go in [endpoint.auth]; a request content type goes in \
                 [endpoint.operation.body]."
            ));
        }
        if value.chars().any(|character| character.is_control()) {
            return Err(format!(
                "header {name:?} has a control character in its value"
            ));
        }
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

fn validate_auth(raw: &RawAuth) -> Result<AuthDecl, String> {
    let kind = raw.kind.trim().to_ascii_lowercase();
    let expect_env = |value: &Option<String>, field: &str| -> Result<String, String> {
        let name = value
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("auth kind {kind:?} needs `{field}`"))?;
        validate_env_name(field, name)?;
        Ok(name.to_string())
    };

    match kind.as_str() {
        "none" => Ok(AuthDecl::None),
        "bearer" => Ok(AuthDecl::Bearer {
            token_env: expect_env(&raw.token_env, "token_env")?,
        }),
        "basic" => {
            let username = raw
                .username
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "auth kind \"basic\" needs `username`".to_string())?;
            if username.chars().any(|character| character == ':') {
                return Err(
                    "basic auth `username` may not contain `:` — RFC 7617 uses it as the \
                     separator"
                        .to_string(),
                );
            }
            if username.chars().any(char::is_control) {
                return Err("basic auth `username` has a control character".to_string());
            }
            Ok(AuthDecl::Basic {
                username: username.to_string(),
                password_env: expect_env(&raw.password_env, "password_env")?,
            })
        }
        "header" => {
            let header = raw
                .header
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "auth kind \"header\" needs `header`".to_string())?;
            if !header
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
            {
                return Err(format!(
                    "auth header name {header:?} is not a valid HTTP token"
                ));
            }
            Ok(AuthDecl::Header {
                header: header.to_string(),
                value_env: expect_env(&raw.value_env, "value_env")?,
            })
        }
        "query" => {
            let param = raw
                .param
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "auth kind \"query\" needs `param`".to_string())?;
            if param.chars().any(char::is_control) {
                return Err("auth `param` has a control character".to_string());
            }
            Ok(AuthDecl::Query {
                param: param.to_string(),
                value_env: expect_env(&raw.value_env, "value_env")?,
            })
        }
        other => Err(format!(
            "unknown auth kind {other:?}; expected \"none\", \"bearer\", \"basic\", \"header\", \
             or \"query\""
        )),
    }
}

/// An auth field takes the *name* of an environment variable.
///
/// The character set is the one a shell will actually export, which already
/// rejects most credential shapes (`sk-…`, `xoxb-…`, a JWT's dots). The
/// prefix check catches the ones that would otherwise slip through, so pasting
/// a GitHub token here fails at load rather than being written to a file that
/// gets shared.
fn validate_env_name(field: &str, name: &str) -> Result<(), String> {
    if name.len() > MAX_ENV_NAME_LEN {
        return Err(format!(
            "`{field}` is {} characters; an environment variable name is at most \
             {MAX_ENV_NAME_LEN}",
            name.len()
        ));
    }
    let mut characters = name.chars();
    let valid = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_');
    if !valid {
        return Err(format!(
            "`{field}` is {name:?}, which is not a valid environment variable name (letters, \
             digits, and `_`, not starting with a digit). This field takes the NAME of a variable, \
             never a credential."
        ));
    }
    if looks_like_a_credential(name) {
        return Err(format!(
            "`{field}` looks like a credential rather than an environment variable name. This \
             field takes the NAME of a variable; export the value in the environment of the tdcc \
             process instead. Nothing in this file is treated as secret."
        ));
    }
    Ok(())
}

/// Shapes that are credentials often enough that accepting one is worse than
/// the occasional false positive on a variable name.
fn looks_like_a_credential(value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk_",
        "pk_",
        "rk_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb",
        "xoxp",
        "xoxa",
        "xapp",
        "glpat",
        "AKIA",
        "ASIA",
        "AIza",
        "ya29",
        "shpat_",
        "npm_",
        "dop_v1_",
        "hf_",
    ];
    PREFIXES
        .iter()
        .any(|prefix| value.len() > prefix.len() && value.starts_with(prefix))
}

fn validate_limits(raw: &RawEndpoint) -> Result<EndpointLimits, Vec<String>> {
    let mut problems = Vec::new();
    let defaults = EndpointLimits::default();

    let timeout_secs = bounded_u64(
        "timeout_secs",
        raw.timeout_secs.unwrap_or(defaults.timeout_secs),
        MIN_TIMEOUT_SECS,
        MAX_TIMEOUT_SECS,
        &mut problems,
        defaults.timeout_secs,
    );
    let max_response_bytes = bounded_usize(
        "max_response_bytes",
        raw.max_response_bytes
            .unwrap_or(defaults.max_response_bytes),
        MIN_MAX_RESPONSE_BYTES,
        CEILING_MAX_RESPONSE_BYTES,
        &mut problems,
        defaults.max_response_bytes,
    );
    let max_request_bytes = bounded_usize(
        "max_request_bytes",
        raw.max_request_bytes.unwrap_or(defaults.max_request_bytes),
        MIN_MAX_REQUEST_BYTES,
        CEILING_MAX_REQUEST_BYTES,
        &mut problems,
        defaults.max_request_bytes,
    );
    let max_calls_per_minute = bounded_u32(
        "max_calls_per_minute",
        raw.max_calls_per_minute
            .unwrap_or(defaults.max_calls_per_minute),
        MIN_MAX_CALLS_PER_MINUTE,
        CEILING_MAX_CALLS_PER_MINUTE,
        &mut problems,
        defaults.max_calls_per_minute,
    );

    if problems.is_empty() {
        Ok(EndpointLimits {
            timeout_secs,
            max_response_bytes,
            max_request_bytes,
            max_calls_per_minute,
        })
    } else {
        Err(problems)
    }
}

fn validate_operation(raw: RawOperation, methods: &[String]) -> Result<Operation, Vec<String>> {
    let mut problems = Vec::new();

    if let Err(error) = validate_identifier("operation name", &raw.name) {
        problems.push(error);
    }
    if let Err(error) = validate_description("description", &raw.description) {
        problems.push(error);
    }

    let method = raw.method.trim().to_ascii_uppercase();
    if !SUPPORTED_METHODS.contains(&method.as_str()) {
        problems.push(format!("method {:?} is not supported", raw.method));
    } else if !methods.is_empty() && !methods.contains(&method) {
        problems.push(format!(
            "method {method} is not in this endpoint's `methods` list ({}). The endpoint list is \
             the outer limit; an operation may only narrow it.",
            methods.join(", ")
        ));
    }

    let path = raw.path.trim().to_string();
    if !path.starts_with('/') {
        problems.push(format!(
            "path {path:?} must start with `/` and is relative to the base URL path"
        ));
    }
    if path.contains('?') || path.contains('#') || path.contains("://") {
        problems.push(format!(
            "path {path:?} may not carry a query, a fragment, or a scheme. Query parameters are \
             declared with `in = \"query\"`."
        ));
    }
    let placeholders = match pathmatch::placeholders(&path) {
        Ok(names) => names,
        Err(error) => {
            problems.push(error);
            Vec::new()
        }
    };

    if raw.parameter.len() > MAX_PARAMETERS {
        problems.push(format!(
            "{} parameters are declared; the limit is {MAX_PARAMETERS}",
            raw.parameter.len()
        ));
    }

    let mut parameters: Vec<Parameter> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for raw_parameter in raw.parameter {
        let label = raw_parameter.name.clone();
        match validate_parameter(raw_parameter) {
            Ok(parameter) => {
                if !seen.insert(parameter.name.clone()) {
                    problems.push(format!("parameter {:?} is declared twice", parameter.name));
                }
                parameters.push(parameter);
            }
            Err(error) => problems.push(format!("parameter {label:?}: {error}")),
        }
    }

    // Every hole in the template must have a parameter, and every path
    // parameter must have a hole. Either mismatch is a template that cannot be
    // expanded, which is better found now than on the first call.
    for placeholder in &placeholders {
        match parameters
            .iter()
            .find(|parameter| &parameter.name == placeholder)
        {
            None => problems.push(format!(
                "path template has `{{{placeholder}}}` but no parameter named {placeholder:?} is \
                 declared"
            )),
            Some(parameter) if parameter.location != ParameterIn::Path => problems.push(format!(
                "parameter {placeholder:?} fills `{{{placeholder}}}` in the path but is declared \
                 with `in = \"{}\"`",
                parameter.location.as_str()
            )),
            Some(parameter) if !parameter.required => problems.push(format!(
                "path parameter {placeholder:?} is optional, but a path template has no shape \
                 without it. Declare `required = true`."
            )),
            Some(_) => {}
        }
    }
    for parameter in &parameters {
        if parameter.location == ParameterIn::Path && !placeholders.contains(&parameter.name) {
            problems.push(format!(
                "parameter {:?} is declared `in = \"path\"` but the path template has no \
                 `{{{}}}`",
                parameter.name, parameter.name
            ));
        }
    }

    let body = match raw.body {
        None => None,
        Some(raw_body) => {
            if !BODY_METHODS.contains(&method.as_str()) {
                problems.push(format!(
                    "a body is declared but the method is {method}, which does not carry one"
                ));
            }
            match validate_body(raw_body) {
                Ok(body) => Some(body),
                Err(error) => {
                    problems.push(format!("body: {error}"));
                    None
                }
            }
        }
    };

    if problems.is_empty() {
        Ok(Operation {
            name: raw.name,
            description: raw.description.trim().to_string(),
            method,
            path,
            parameters,
            body,
        })
    } else {
        Err(problems)
    }
}

fn validate_body(raw: RawBody) -> Result<BodySpec, String> {
    validate_description("description", &raw.description)?;
    let content_type = raw
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/json")
        .to_string();
    if content_type.chars().any(|character| character.is_control()) {
        return Err("content_type has a control character".to_string());
    }
    if !content_type.contains('/') {
        return Err(format!("content_type {content_type:?} is not a media type"));
    }
    Ok(BodySpec {
        required: raw.required,
        description: raw.description.trim().to_string(),
        content_type,
    })
}

fn validate_parameter(raw: RawParameter) -> Result<Parameter, String> {
    validate_identifier("name", &raw.name)?;
    validate_description("description", &raw.description)?;
    let location = ParameterIn::parse(&raw.location)?;
    let parameter_type = ParameterType::parse(&raw.parameter_type)?;

    if raw.allowed.len() > MAX_ENUM_VALUES {
        return Err(format!(
            "enum has {} values; the limit is {MAX_ENUM_VALUES}",
            raw.allowed.len()
        ));
    }
    for value in &raw.allowed {
        if !parameter_type.accepts(value.type_of()) {
            return Err(format!(
                "enum value {} is a {}, but the parameter is declared as {}",
                value.render(),
                value.type_of().as_str(),
                parameter_type.as_str()
            ));
        }
    }

    if let Some(default) = &raw.default {
        if raw.required {
            return Err(
                "a required parameter cannot have a default; a default is what makes a parameter \
                 optional"
                    .to_string(),
            );
        }
        if !parameter_type.accepts(default.type_of()) {
            return Err(format!(
                "default {} is a {}, but the parameter is declared as {}",
                default.render(),
                default.type_of().as_str(),
                parameter_type.as_str()
            ));
        }
        if !raw.allowed.is_empty()
            && !raw
                .allowed
                .iter()
                .any(|value| value.render() == default.render())
        {
            return Err(format!(
                "default {} is not one of the enum values",
                default.render()
            ));
        }
    }

    if parameter_type != ParameterType::String
        && (raw.min_length.is_some() || raw.max_length.is_some())
    {
        return Err("min_length and max_length apply to string parameters only".to_string());
    }
    if !matches!(
        parameter_type,
        ParameterType::Integer | ParameterType::Number
    ) && (raw.minimum.is_some() || raw.maximum.is_some())
    {
        return Err("minimum and maximum apply to integer and number parameters only".to_string());
    }
    if let (Some(min), Some(max)) = (raw.min_length, raw.max_length)
        && min > max
    {
        return Err(format!("min_length {min} is greater than max_length {max}"));
    }
    if let (Some(min), Some(max)) = (raw.minimum, raw.maximum)
        && min > max
    {
        return Err(format!("minimum {min} is greater than maximum {max}"));
    }

    Ok(Parameter {
        name: raw.name.trim().to_string(),
        location,
        parameter_type,
        required: raw.required,
        description: raw.description.trim().to_string(),
        allowed: raw.allowed,
        default: raw.default,
        min_length: raw.min_length,
        max_length: raw.max_length,
        minimum: raw.minimum,
        maximum: raw.maximum,
    })
}

/// Would this operation ever be allowed by the endpoint's `paths` list?
///
/// The probe substitutes each path parameter's first `enum` value when it has
/// one and a neutral `_` when it does not, so an operator whose allowlist pins
/// a literal segment can express that as an `enum` on the parameter. This is
/// only a load-time sanity check; the authoritative check runs in `request.rs`
/// against the concrete path of the URL that is about to be sent.
fn check_reachable(operation: &Operation, paths: &[String]) -> Result<(), String> {
    let values: BTreeMap<String, String> = operation
        .parameters
        .iter()
        .filter(|parameter| parameter.location == ParameterIn::Path)
        .map(|parameter| {
            let probe = parameter
                .allowed
                .first()
                .map(Literal::render)
                .unwrap_or_else(|| "_".to_string());
            (parameter.name.clone(), probe)
        })
        .collect();

    let probe = pathmatch::expand(&operation.path, &values)
        .map_err(|error| format!("operation {:?}: {error}", operation.name))?;
    if pathmatch::matches_any(paths, &probe) {
        return Ok(());
    }
    Err(format!(
        "operation {:?} resolves to {probe} which no entry in `paths` allows ({}). Either widen \
         `paths` or narrow the operation.",
        operation.name,
        paths.join(", ")
    ))
}

fn validate_identifier(field: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(format!(
            "{field} {value:?} is longer than {MAX_NAME_LEN} characters"
        ));
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(format!(
            "{field} {value:?} may only contain ASCII letters, digits, `_`, and `-`"
        ));
    }
    if !value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric() || first == '_')
    {
        return Err(format!(
            "{field} {value:?} must start with a letter, a digit, or `_`"
        ));
    }
    Ok(())
}

/// A description is not decoration. It becomes the text a model reads when it
/// decides whether to call something, so an empty one is a declaration nobody
/// can use correctly.
fn validate_description(field: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "{field} must not be empty — it is what a model reads when it decides how to call this"
        ));
    }
    if value.len() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "{field} is {} characters; the limit is {MAX_DESCRIPTION_LEN}",
            value.len()
        ));
    }
    Ok(())
}

fn bounded_u64(
    field: &str,
    value: u64,
    min: u64,
    max: u64,
    problems: &mut Vec<String>,
    fallback: u64,
) -> u64 {
    if value < min || value > max {
        problems.push(format!(
            "{field} must be between {min} and {max}, got {value}"
        ));
        return fallback;
    }
    value
}

fn bounded_u32(
    field: &str,
    value: u32,
    min: u32,
    max: u32,
    problems: &mut Vec<String>,
    fallback: u32,
) -> u32 {
    if value < min || value > max {
        problems.push(format!(
            "{field} must be between {min} and {max}, got {value}"
        ));
        return fallback;
    }
    value
}

fn bounded_usize(
    field: &str,
    value: usize,
    min: usize,
    max: usize,
    problems: &mut Vec<String>,
    fallback: usize,
) -> usize {
    if value < min || value > max {
        problems.push(format!(
            "{field} must be between {min} and {max}, got {value}"
        ));
        return fallback;
    }
    value
}

#[cfg(test)]
pub(crate) const SAMPLE: &str = r#"
version = 1

[[endpoint]]
name = "example"
description = "A read-only example API."
base_url = "https://api.example.com/v2"
methods = ["GET", "POST"]
paths = ["/things", "/things/*", "/search"]

[endpoint.auth]
kind = "bearer"
token_env = "TDCC_REST_CLIENT_EXAMPLE_TOKEN"

[endpoint.headers]
Accept = "application/json"

[[endpoint.operation]]
name = "get_thing"
description = "Fetch one thing by its identifier."
method = "GET"
path = "/things/{id}"

[[endpoint.operation.parameter]]
name = "id"
in = "path"
type = "string"
required = true
description = "Identifier of the thing, as returned by list_things."

[[endpoint.operation]]
name = "list_things"
description = "List things, newest first."
method = "GET"
path = "/things"

[[endpoint.operation.parameter]]
name = "limit"
in = "query"
type = "integer"
description = "How many things to return."
minimum = 1
maximum = 100
default = 20

[[endpoint.operation.parameter]]
name = "state"
in = "query"
type = "string"
description = "Which things to include."
enum = ["open", "closed", "all"]
default = "open"

[[endpoint.operation]]
name = "search"
description = "Search things by free text."
method = "POST"
path = "/search"

[endpoint.operation.body]
required = true
description = "A JSON object with a `query` string."
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Catalog {
        parse(text).unwrap_or_else(|error| panic!("expected a valid document:\n{error}"))
    }

    #[test]
    fn the_sample_document_parses_into_one_endpoint_with_three_operations() {
        let catalog = parse_ok(SAMPLE);

        assert_eq!(catalog.names(), vec!["example".to_string()]);
        let endpoint = catalog.endpoint("example").expect("declared");
        assert_eq!(endpoint.base_url, "https://api.example.com/v2");
        assert_eq!(endpoint.base_path, "/v2");
        assert_eq!(endpoint.methods, vec!["GET", "POST"]);
        assert_eq!(
            endpoint.headers,
            vec![("Accept".into(), "application/json".into())]
        );
        assert_eq!(
            endpoint.auth,
            AuthDecl::Bearer {
                token_env: "TDCC_REST_CLIENT_EXAMPLE_TOKEN".into()
            }
        );
        assert_eq!(endpoint.limits, EndpointLimits::default());
        assert_eq!(endpoint.operations.len(), 3);

        let list = endpoint.operation("list_things").expect("declared");
        let state = list.parameter("state").expect("declared");
        assert_eq!(state.default, Some(Literal::Text("open".into())));
        assert_eq!(state.allowed.len(), 3);
    }

    #[test]
    fn a_version_this_build_does_not_understand_is_refused() {
        let error = parse("version = 2\n").expect_err("unknown version");
        assert!(error.contains("version"), "{error}");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_setting_that_does_nothing() {
        let document = SAMPLE.replace("allow_private_base", "allow_privte_base");
        let document = document.replace(
            "paths = [\"/things\", \"/things/*\", \"/search\"]",
            "paths = [\"/things\", \"/things/*\", \"/search\"]\nallow_everythng = true",
        );
        let error = parse(&document).expect_err("unknown keys are refused");
        assert!(error.contains("allow_everythng"), "{error}");
    }

    #[test]
    fn a_base_url_with_credentials_a_query_or_a_bad_scheme_is_refused() {
        for (bad, expected) in [
            ("https://user:pass@api.example.com", "credentials"),
            ("https://api.example.com/?token=x", "query"),
            ("ftp://api.example.com", "scheme"),
            ("file:///etc/passwd", "scheme"),
            ("https://api.example.com/#frag", "fragment"),
        ] {
            let document = SAMPLE.replace("https://api.example.com/v2", bad);
            let error = parse(&document).expect_err(&format!("{bad} must be refused"));
            assert!(error.contains(expected), "{bad}: {error}");
        }
    }

    #[test]
    fn cleartext_http_with_auth_needs_an_explicit_opt_in() {
        let document = SAMPLE.replace("https://api.example.com/v2", "http://api.example.com/v2");
        let error = parse(&document).expect_err("http plus a credential is refused");
        assert!(error.contains("allow_insecure_auth"), "{error}");

        let allowed = document.replace(
            "paths = [\"/things\", \"/things/*\", \"/search\"]",
            "paths = [\"/things\", \"/things/*\", \"/search\"]\nallow_insecure_auth = true",
        );
        parse(&allowed).expect("the operator opted in explicitly");
    }

    #[test]
    fn cleartext_http_without_auth_is_fine() {
        let document = SAMPLE
            .replace("https://api.example.com/v2", "http://api.example.com/v2")
            .replace("kind = \"bearer\"", "kind = \"none\"")
            .replace("token_env = \"TDCC_REST_CLIENT_EXAMPLE_TOKEN\"", "");
        parse(&document).expect("no credential, nothing to leak");
    }

    #[test]
    fn an_auth_field_holding_a_credential_instead_of_a_variable_name_is_refused() {
        for pasted in [
            "ghp_16CharsOfNoiseHere",
            "sk_live_abc123def456",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxbFakeSlackToken",
        ] {
            let document = SAMPLE.replace("TDCC_REST_CLIENT_EXAMPLE_TOKEN", pasted);
            let error = parse(&document).expect_err(&format!("{pasted} must be refused"));
            assert!(error.contains("credential"), "{pasted}: {error}");
        }
    }

    #[test]
    fn an_auth_field_that_is_not_a_shell_variable_name_is_refused() {
        for pasted in ["my-token", "eyJhbGciOi.eyJzdWIi.sig", "1TOKEN", "a b"] {
            let document = SAMPLE.replace("TDCC_REST_CLIENT_EXAMPLE_TOKEN", pasted);
            let error = parse(&document).expect_err(&format!("{pasted} must be refused"));
            assert!(error.contains("NAME of a variable"), "{pasted}: {error}");
        }
    }

    #[test]
    fn a_reserved_header_may_not_be_set_from_the_document() {
        for header in ["Authorization", "authorization", "Cookie", "Content-Type"] {
            let document = SAMPLE.replace(
                "Accept = \"application/json\"",
                &format!("{header} = \"x\""),
            );
            let error = parse(&document).expect_err(&format!("{header} must be refused"));
            assert!(error.contains("rest-client itself"), "{header}: {error}");
        }
    }

    #[test]
    fn a_header_value_with_a_newline_is_refused() {
        let document = SAMPLE.replace(
            "Accept = \"application/json\"",
            "Accept = \"application/json\\r\\nX-Injected: 1\"",
        );
        let error = parse(&document).expect_err("header injection is refused");
        assert!(error.contains("control character"), "{error}");
    }

    #[test]
    fn an_operation_outside_the_path_allowlist_is_refused_at_load() {
        let document = SAMPLE.replace("path = \"/things\"\n", "path = \"/admin/things\"\n");
        let error = parse(&document).expect_err("unreachable operation");
        assert!(error.contains("no entry in `paths` allows"), "{error}");
    }

    #[test]
    fn an_enum_lets_an_allowlist_pin_a_literal_segment() {
        let document = r#"
version = 1

[[endpoint]]
name = "pinned"
description = "Only one repository is reachable."
base_url = "https://api.github.com"
methods = ["GET"]
paths = ["/repos/rust-lang/*/issues"]

[[endpoint.operation]]
name = "list_issues"
description = "List issues in an allowed repository."
method = "GET"
path = "/repos/{owner}/{repo}/issues"

[[endpoint.operation.parameter]]
name = "owner"
in = "path"
type = "string"
required = true
description = "Repository owner. Only rust-lang is reachable."
enum = ["rust-lang"]

[[endpoint.operation.parameter]]
name = "repo"
in = "path"
type = "string"
required = true
description = "Repository name."
"#;
        parse_ok(document);
    }

    #[test]
    fn an_operation_method_outside_the_endpoint_methods_is_refused() {
        let document = SAMPLE.replace("methods = [\"GET\", \"POST\"]", "methods = [\"GET\"]");
        let error = parse(&document).expect_err("POST is not allowed on this endpoint");
        assert!(
            error.contains("is not in this endpoint's `methods`"),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_method_cannot_be_configured() {
        let document = SAMPLE.replace("methods = [\"GET\", \"POST\"]", "methods = [\"CONNECT\"]");
        let error = parse(&document).expect_err("CONNECT is not offered");
        assert!(error.contains("not supported"), "{error}");
    }

    #[test]
    fn a_path_placeholder_needs_a_matching_required_path_parameter() {
        let missing = SAMPLE.replace(
            "name = \"id\"\nin = \"path\"",
            "name = \"other\"\nin = \"path\"",
        );
        let error = parse(&missing).expect_err("the placeholder has no parameter");
        assert!(error.contains("no parameter named"), "{error}");

        let wrong_place = SAMPLE.replace(
            "name = \"id\"\nin = \"path\"",
            "name = \"id\"\nin = \"query\"",
        );
        let error = parse(&wrong_place).expect_err("a query parameter cannot fill a path hole");
        assert!(error.contains("in = \"query\""), "{error}");

        let optional = SAMPLE.replace(
            "name = \"id\"\nin = \"path\"\ntype = \"string\"\nrequired = true",
            "name = \"id\"\nin = \"path\"\ntype = \"string\"",
        );
        let error = parse(&optional).expect_err("an optional path parameter has no shape");
        assert!(error.contains("required = true"), "{error}");
    }

    #[test]
    fn a_default_must_be_optional_typed_and_inside_its_enum() {
        let required_default = SAMPLE.replace(
            "description = \"Which things to include.\"\nenum = [\"open\", \"closed\", \"all\"]",
            "description = \"Which things to include.\"\nrequired = true\nenum = [\"open\", \"closed\", \"all\"]",
        );
        let error = parse(&required_default).expect_err("a required parameter has no default");
        assert!(error.contains("cannot have a default"), "{error}");

        let wrong_type = SAMPLE.replace("default = 20", "default = \"twenty\"");
        let error = parse(&wrong_type).expect_err("a string default on an integer");
        assert!(error.contains("declared as integer"), "{error}");

        let outside = SAMPLE.replace("default = \"open\"", "default = \"pending\"");
        let error = parse(&outside).expect_err("a default outside the enum");
        assert!(error.contains("not one of the enum"), "{error}");
    }

    #[test]
    fn constraints_are_refused_on_types_they_do_not_apply_to() {
        let document = SAMPLE.replace(
            "description = \"Identifier of the thing, as returned by list_things.\"",
            "description = \"Identifier of the thing.\"\nminimum = 1",
        );
        let error = parse(&document).expect_err("minimum on a string");
        assert!(error.contains("integer and number"), "{error}");

        let document = SAMPLE.replace(
            "description = \"How many things to return.\"",
            "description = \"How many things to return.\"\nmax_length = 4",
        );
        let error = parse(&document).expect_err("max_length on an integer");
        assert!(error.contains("string parameters only"), "{error}");
    }

    #[test]
    fn a_body_on_a_method_that_carries_none_is_refused() {
        let document = SAMPLE.replace(
            "name = \"search\"\ndescription = \"Search things by free text.\"\nmethod = \"POST\"",
            "name = \"search\"\ndescription = \"Search things by free text.\"\nmethod = \"GET\"",
        );
        let error = parse(&document).expect_err("a GET body is refused");
        assert!(error.contains("does not carry one"), "{error}");
    }

    #[test]
    fn an_empty_description_is_refused_everywhere_it_appears() {
        let endpoint = SAMPLE.replace(
            "description = \"A read-only example API.\"",
            "description = \"\"",
        );
        assert!(parse(&endpoint).is_err());

        let operation = SAMPLE.replace(
            "description = \"Fetch one thing by its identifier.\"",
            "description = \"\"",
        );
        assert!(parse(&operation).is_err());

        let parameter = SAMPLE.replace(
            "description = \"Identifier of the thing, as returned by list_things.\"",
            "description = \"  \"",
        );
        assert!(parse(&parameter).is_err());
    }

    #[test]
    fn an_endpoint_with_no_operations_is_refused_rather_than_silently_inert() {
        let document = r#"
version = 1

[[endpoint]]
name = "empty"
description = "Declares nothing callable."
base_url = "https://api.example.com"
methods = ["GET"]
paths = ["/**"]
"#;
        let error = parse(document).expect_err("an endpoint with no operations");
        assert!(error.contains("no operations"), "{error}");
    }

    #[test]
    fn duplicate_endpoint_and_operation_names_are_refused() {
        let document = format!(
            "{SAMPLE}\n{}",
            SAMPLE.trim_start().trim_start_matches("version = 1")
        );
        let error = parse(&document).expect_err("a duplicate endpoint name");
        assert!(error.contains("declared twice"), "{error}");
    }

    #[test]
    fn limits_outside_their_ceilings_are_refused_with_the_bounds_named() {
        for (line, needle) in [
            ("timeout_secs = 0", "timeout_secs"),
            ("timeout_secs = 1000", "timeout_secs"),
            ("max_response_bytes = 16", "max_response_bytes"),
            ("max_response_bytes = 99999999", "max_response_bytes"),
            ("max_calls_per_minute = 0", "max_calls_per_minute"),
            ("max_request_bytes = 99999999", "max_request_bytes"),
        ] {
            let document = SAMPLE.replace(
                "paths = [\"/things\", \"/things/*\", \"/search\"]",
                &format!("paths = [\"/things\", \"/things/*\", \"/search\"]\n{line}"),
            );
            let error = parse(&document).expect_err(&format!("{line} must be refused"));
            assert!(error.contains(needle), "{line}: {error}");
        }
    }

    #[test]
    fn an_empty_document_is_a_valid_catalog_with_nothing_in_it() {
        let catalog = parse_ok("version = 1\n");
        assert!(catalog.endpoints.is_empty());
    }

    #[test]
    fn every_complaint_is_reported_rather_than_only_the_first() {
        let document = r#"
version = 1

[[endpoint]]
name = "bad name"
description = ""
base_url = "not a url"
methods = ["FLY"]
paths = ["relative"]
"#;
        let error = parse(document).expect_err("several problems");
        assert!(error.lines().count() >= 4, "{error}");
    }
}
