use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use thiserror::Error;

pub const WIRE_PROTOCOL: &str = "1.0";
pub const CONTROL_PROTOCOL: u16 = 1;
pub const REGISTRY_VERSION: &str = "1.0.0";
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_STDOUT_BYTES: usize = 4_096;

const REGISTRY_JSON: &str = include_str!("../assets/registry.json");
const ERROR_CATALOG_JSON: &str = include_str!("../assets/error-catalog.json");
const COMMAND_INPUTS_SCHEMA_JSON: &str =
    include_str!("../assets/schemas/command-inputs.schema.json");
const COMMAND_RESULTS_SCHEMA_JSON: &str =
    include_str!("../assets/schemas/command-results.schema.json");
const ACTION_RESULT_SCHEMA_JSON: &str = include_str!("../assets/schemas/action-result.schema.json");
const COMPLETE_TREE_RESULT_SCHEMA_JSON: &str =
    include_str!("../assets/schemas/complete-tree-result.schema.json");
const EXPORTED_ARTIFACT_MANIFEST_SCHEMA_JSON: &str =
    include_str!("../assets/schemas/exported-artifact-manifest.schema.json");
const ERROR_SCHEMA_JSON: &str = include_str!("../assets/schemas/error.schema.json");
pub const AGENT_HELP: &str = include_str!("../assets/agent-help.md");

include!(concat!(env!("OUT_DIR"), "/command_ids.rs"));
include!(concat!(env!("OUT_DIR"), "/release_manifest.rs"));

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Status,
    Drain,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub control_protocol: u16,
    pub request_id: String,
    pub action: ControlAction,
}

impl ControlRequest {
    pub fn new(request_id: String, action: ControlAction) -> Self {
        Self {
            control_protocol: CONTROL_PROTOCOL,
            request_id,
            action,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub control_protocol: u16,
    pub request_id: String,
    pub ok: bool,
    pub daemon: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationalError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub major: u16,
    pub minimum_minor: u16,
    pub maximum_minor: u16,
}

impl Default for ProtocolRange {
    fn default() -> Self {
        Self {
            major: 1,
            minimum_minor: 0,
            maximum_minor: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub protocol: ProtocolRange,
    pub registry_version: String,
    pub build_digest: String,
    pub request_id: String,
    pub deadline_ms: u64,
    pub command: String,
    pub input: Value,
}

impl Invocation {
    pub fn new(
        command: impl Into<String>,
        input: Value,
        request_id: String,
        deadline_ms: u64,
    ) -> Self {
        Self {
            protocol: ProtocolRange::default(),
            registry_version: REGISTRY_VERSION.to_owned(),
            build_digest: build_digest(),
            request_id,
            deadline_ms,
            command: command.into(),
            input,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: Invocation,
}

impl RpcRequest {
    pub fn invocation(invocation: Invocation) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: invocation.request_id.clone(),
            method: "manuvra.invoke".to_owned(),
            params: invocation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcTransportError>,
    #[serde(skip)]
    pub exit_code: i32,
}

impl RpcResponse {
    pub fn result(id: String, result: Value, exit_code: i32) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
            exit_code,
        }
    }

    pub fn transport_error(id: String, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(RpcTransportError {
                code,
                message: message.into(),
            }),
            exit_code: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcTransportError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorMeta {
    pub code: String,
    pub category: String,
    pub phase: String,
    pub effects: String,
    pub retry: String,
    pub exit: i32,
    pub meaning: String,
    pub recovery: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WarningMeta {
    pub code: String,
    pub meaning: String,
    pub recovery: String,
}

#[derive(Debug, Deserialize)]
struct ErrorCatalog {
    errors: Vec<ErrorMeta>,
    warnings: Vec<WarningMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationalError {
    pub code: String,
    pub category: String,
    pub phase: String,
    pub effects: String,
    pub retry: String,
    pub message: String,
    pub recovery_command: String,
    pub help_command: String,
    pub details_path: Option<String>,
}

pub fn operational_error(code: &str, message: Option<&str>) -> (OperationalError, i32) {
    let meta = error_meta(code).unwrap_or_else(|| error_meta("internal_error").expect("catalog"));
    let value = OperationalError {
        code: meta.code.clone(),
        category: meta.category.clone(),
        phase: meta.phase.clone(),
        effects: meta.effects.clone(),
        retry: meta.retry.clone(),
        message: message.unwrap_or(&meta.meaning).chars().take(256).collect(),
        recovery_command: meta.recovery.clone(),
        help_command: format!("manuvra commands errors {}", meta.code),
        details_path: None,
    };
    (value, meta.exit)
}

pub fn error_meta(code: &str) -> Option<&'static ErrorMeta> {
    let code = ErrorCode::parse(code)?;
    error_catalog()
        .errors
        .iter()
        .find(|entry| entry.code == code.as_str())
}

pub fn all_errors() -> &'static [ErrorMeta] {
    &error_catalog().errors
}

pub fn all_warnings() -> &'static [WarningMeta] {
    &error_catalog().warnings
}

fn error_catalog() -> &'static ErrorCatalog {
    static CATALOG: OnceLock<ErrorCatalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| serde_json::from_str(ERROR_CATALOG_JSON).expect("accepted error catalog"))
}

pub fn registry() -> &'static Value {
    static REGISTRY: OnceLock<Value> = OnceLock::new();
    REGISTRY.get_or_init(|| serde_json::from_str(REGISTRY_JSON).expect("accepted registry"))
}

pub fn command_descriptor(id: &str) -> Option<&'static Value> {
    registry()["commands"]
        .as_array()?
        .iter()
        .find(|command| command["id"] == id)
}

pub fn command_default_timeout_ms(id: &str) -> Option<u64> {
    command_descriptor(id)?["default_timeout_ms"].as_u64()
}

pub fn command_authority(id: &str) -> Option<&'static str> {
    command_descriptor(id)?["authority"].as_str()
}

pub fn command_input_fields(id: &str) -> Option<Vec<&'static str>> {
    input_schema_for(id)?["properties"]
        .as_object()
        .map(|properties| properties.keys().map(String::as_str).collect())
}

pub fn command_required_fields(id: &str) -> Option<Vec<&'static str>> {
    Some(
        input_schema_for(id)?["required"]
            .as_array()
            .map(|required| required.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
    )
}

pub fn command_capabilities(id: &str) -> Option<Vec<&'static str>> {
    Some(
        command_descriptor(id)?["capabilities"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect(),
    )
}

pub fn command_modes(id: &str) -> Option<Vec<&'static str>> {
    Some(
        command_descriptor(id)?["modes"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect(),
    )
}

fn input_schema_for(id: &str) -> Option<&'static Value> {
    let reference = command_descriptor(id)?["input_schema"].as_str()?;
    let definition = reference.rsplit('/').next()?;
    command_inputs_schema()["$defs"].get(definition)
}

pub fn validate_command_input(command: &str, input: &Value) -> Result<(), String> {
    let descriptor = command_descriptor(command).ok_or_else(|| "unknown command".to_owned())?;
    let reference = descriptor["input_schema"]
        .as_str()
        .ok_or_else(|| "command has no input schema".to_owned())?;
    let definition = reference
        .rsplit('/')
        .next()
        .ok_or_else(|| "invalid input schema reference".to_owned())?;
    let root = command_inputs_schema();
    let schema = root["$defs"]
        .get(definition)
        .ok_or_else(|| "input schema definition is missing".to_owned())?;
    validate_schema(schema, input, root, "input")
}

pub fn validate_command_result(command: &str, result: &Value) -> Result<(), String> {
    let descriptor = command_descriptor(command).ok_or_else(|| "unknown command".to_owned())?;
    let reference = descriptor["result_schema"]
        .as_str()
        .ok_or_else(|| "command has no result schema".to_owned())?;
    let root = result_schema_document(reference)?;
    let schema = match reference.split_once('#') {
        Some((_, pointer)) => root
            .pointer(pointer)
            .ok_or_else(|| "result schema definition is missing".to_owned())?,
        None => root,
    };
    validate_schema(schema, result, root, "result")
}

fn command_inputs_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(COMMAND_INPUTS_SCHEMA_JSON).expect("accepted command input schema")
    })
}

static COMMAND_RESULTS_SCHEMA: OnceLock<Value> = OnceLock::new();
static ACTION_RESULT_SCHEMA: OnceLock<Value> = OnceLock::new();
static COMPLETE_TREE_RESULT_SCHEMA: OnceLock<Value> = OnceLock::new();
static EXPORTED_ARTIFACT_MANIFEST_SCHEMA: OnceLock<Value> = OnceLock::new();
static ERROR_SCHEMA: OnceLock<Value> = OnceLock::new();

pub fn validate_exported_artifact_manifest(manifest: &Value) -> Result<(), String> {
    let schema = parsed_schema(
        &EXPORTED_ARTIFACT_MANIFEST_SCHEMA,
        EXPORTED_ARTIFACT_MANIFEST_SCHEMA_JSON,
    );
    validate_schema(schema, manifest, schema, "exported_manifest")
}

pub fn validate_external_document(schema: &Value, value: &Value) -> Result<(), String> {
    validate_schema(schema, value, schema, "document")
}

fn result_schema_document(reference: &str) -> Result<&'static Value, String> {
    if reference.starts_with("./schemas/command-results.schema.json") {
        return Ok(parsed_schema(
            &COMMAND_RESULTS_SCHEMA,
            COMMAND_RESULTS_SCHEMA_JSON,
        ));
    }
    if reference == "./schemas/action-result.schema.json" {
        return Ok(parsed_schema(
            &ACTION_RESULT_SCHEMA,
            ACTION_RESULT_SCHEMA_JSON,
        ));
    }
    if reference == "./schemas/complete-tree-result.schema.json" {
        return Ok(parsed_schema(
            &COMPLETE_TREE_RESULT_SCHEMA,
            COMPLETE_TREE_RESULT_SCHEMA_JSON,
        ));
    }
    Err(format!("unsupported result schema {reference}"))
}

fn parsed_schema(cell: &'static OnceLock<Value>, source: &str) -> &'static Value {
    cell.get_or_init(|| serde_json::from_str(source).expect("accepted result schema"))
}

fn validate_schema(schema: &Value, value: &Value, root: &Value, path: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return validate_schema_reference(reference, value, root, path);
    }
    validate_inline_schema(schema, value, root, path)
}

fn validate_inline_schema(
    schema: &Value,
    value: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    validate_alternatives(schema, value, root, path)?;
    validate_condition(schema, value, root, path)?;
    validate_type(schema, value, path)?;
    validate_exact_values(schema, value, path)?;
    validate_value_kind(schema, value, root, path)
}

fn validate_value_kind(
    schema: &Value,
    value: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    match value {
        Value::Object(map) => validate_object(schema, map, root, path),
        Value::Array(items) => validate_array(schema, items, root, path),
        Value::String(text) => validate_string(schema, text, path),
        Value::Number(number) => validate_number(schema, number, path),
        _ => Ok(()),
    }
}

fn validate_schema_reference(
    reference: &str,
    value: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    if reference.starts_with('#') {
        return validate_schema(resolve_local_ref(root, reference)?, value, root, path);
    }
    let external = match reference {
        "https://manuvra.local/schema/action-result/1" => {
            parsed_schema(&ACTION_RESULT_SCHEMA, ACTION_RESULT_SCHEMA_JSON)
        }
        "https://manuvra.local/schema/error/1" => parsed_schema(&ERROR_SCHEMA, ERROR_SCHEMA_JSON),
        _ => return Err(format!("unsupported schema reference {reference}")),
    };
    validate_schema(external, value, external, path)
}

fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Result<&'a Value, String> {
    let pointer = reference
        .strip_prefix('#')
        .ok_or_else(|| "only local schema references are supported".to_owned())?;
    root.pointer(pointer)
        .ok_or_else(|| format!("unresolved schema reference {reference}"))
}

fn validate_alternatives(
    schema: &Value,
    value: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            validate_schema(branch, value, root, path)?;
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let matches = matching_branches(branches, value, root, path);
        if matches == 0 {
            return Err(format!("{path} does not match any allowed schema"));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = matching_branches(branches, value, root, path);
        if matches != 1 {
            return Err(format!("{path} must match exactly one allowed schema"));
        }
    }
    Ok(())
}

fn matching_branches(branches: &[Value], value: &Value, root: &Value, path: &str) -> usize {
    branches
        .iter()
        .filter(|branch| validate_schema(branch, value, root, path).is_ok())
        .count()
}

fn validate_condition(
    schema: &Value,
    value: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    let Some(condition) = schema.get("if") else {
        return Ok(());
    };
    if validate_schema(condition, value, root, path).is_ok()
        && let Some(then_schema) = schema.get("then")
    {
        validate_schema(then_schema, value, root, path)?;
    }
    Ok(())
}

fn validate_type(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(expected) = schema.get("type") else {
        return Ok(());
    };
    let matches = match expected {
        Value::String(kind) => value_matches_type(value, kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| value_matches_type(value, kind)),
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(format!("{path} has the wrong JSON type"))
    }
}

fn value_matches_type(value: &Value, kind: &str) -> bool {
    match value {
        Value::Object(_) => kind == "object",
        Value::Array(_) => kind == "array",
        Value::String(_) => kind == "string",
        Value::Number(number) => number_matches_schema_type(number, kind),
        Value::Bool(_) => kind == "boolean",
        Value::Null => kind == "null",
    }
}

fn number_matches_schema_type(number: &serde_json::Number, kind: &str) -> bool {
    match kind {
        "number" => true,
        "integer" => number.as_i64().is_some() || number.as_u64().is_some(),
        _ => false,
    }
}

fn validate_exact_values(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return Err(format!("{path} differs from the required constant"));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not an allowed value"));
    }
    Ok(())
}

fn validate_object(
    schema: &Value,
    map: &Map<String, Value>,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    validate_required(schema, map, path)?;
    validate_max_properties(schema, map.len(), path)?;
    validate_object_properties(schema, map, root, path)
}

fn validate_max_properties(schema: &Value, length: usize, path: &str) -> Result<(), String> {
    if let Some(maximum) = schema.get("maxProperties").and_then(Value::as_u64)
        && length as u64 > maximum
    {
        return Err(format!("{path} has too many properties"));
    }
    Ok(())
}

fn validate_object_properties(
    schema: &Value,
    map: &Map<String, Value>,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    for (key, child) in map {
        let child_path = format!("{path}.{key}");
        if let Some(child_schema) = properties.and_then(|items| items.get(key)) {
            validate_schema(child_schema, child, root, &child_path)?;
        } else {
            validate_additional_property(schema, child, root, &child_path)?;
        }
    }
    Ok(())
}

fn validate_required(schema: &Value, map: &Map<String, Value>, path: &str) -> Result<(), String> {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    for key in required.iter().filter_map(Value::as_str) {
        if !map.contains_key(key) {
            return Err(format!("{path}.{key} is required"));
        }
    }
    Ok(())
}

fn validate_additional_property(
    schema: &Value,
    child: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => Err(format!("{path} is not allowed")),
        Some(child_schema @ Value::Object(_)) => validate_schema(child_schema, child, root, path),
        _ => Ok(()),
    }
}

fn validate_array(schema: &Value, items: &[Value], root: &Value, path: &str) -> Result<(), String> {
    validate_array_length(schema, items.len(), path)?;
    validate_unique_items(schema, items, path)?;
    validate_array_items(schema, items, root, path)
}

fn validate_unique_items(schema: &Value, items: &[Value], path: &str) -> Result<(), String> {
    if schema.get("uniqueItems") != Some(&Value::Bool(true)) {
        return Ok(());
    }
    for (index, item) in items.iter().enumerate() {
        if items[..index].contains(item) {
            return Err(format!("{path} contains duplicate items"));
        }
    }
    Ok(())
}

fn validate_array_items(
    schema: &Value,
    items: &[Value],
    root: &Value,
    path: &str,
) -> Result<(), String> {
    let Some(item_schema) = schema.get("items") else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        validate_schema(item_schema, item, root, &format!("{path}[{index}]"))?;
    }
    Ok(())
}

fn validate_array_length(schema: &Value, length: usize, path: &str) -> Result<(), String> {
    if schema
        .get("minItems")
        .and_then(Value::as_u64)
        .is_some_and(|minimum| (length as u64) < minimum)
    {
        return Err(format!("{path} has too few items"));
    }
    if schema
        .get("maxItems")
        .and_then(Value::as_u64)
        .is_some_and(|maximum| length as u64 > maximum)
    {
        return Err(format!("{path} has too many items"));
    }
    Ok(())
}

fn validate_string(schema: &Value, text: &str, path: &str) -> Result<(), String> {
    let length = text.chars().count() as u64;
    if schema
        .get("minLength")
        .and_then(Value::as_u64)
        .is_some_and(|minimum| length < minimum)
    {
        return Err(format!("{path} is too short"));
    }
    if schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .is_some_and(|maximum| length > maximum)
    {
        return Err(format!("{path} is too long"));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
        && !matches_accepted_pattern(pattern, text)
    {
        return Err(format!("{path} has an invalid format"));
    }
    if schema.get("format") == Some(&Value::String("uri".to_owned())) && !looks_like_uri(text) {
        return Err(format!("{path} must be a URI"));
    }
    Ok(())
}

fn validate_number(schema: &Value, number: &serde_json::Number, path: &str) -> Result<(), String> {
    let value = number
        .as_f64()
        .ok_or_else(|| format!("{path} is not a finite number"))?;
    if schema
        .get("minimum")
        .and_then(Value::as_f64)
        .is_some_and(|minimum| value < minimum)
    {
        return Err(format!("{path} is below its minimum"));
    }
    if schema
        .get("maximum")
        .and_then(Value::as_f64)
        .is_some_and(|maximum| value > maximum)
    {
        return Err(format!("{path} is above its maximum"));
    }
    Ok(())
}

fn matches_accepted_pattern(pattern: &str, text: &str) -> bool {
    PATTERN_MATCHERS
        .iter()
        .find(|(accepted, _)| *accepted == pattern)
        .is_some_and(|(_, matcher)| matcher(text))
}

type PatternMatcher = fn(&str) -> bool;

const PATTERN_MATCHERS: &[(&str, PatternMatcher)] = &[
    ("^s_[A-Za-z0-9]+$", session_token),
    ("^a_[A-Za-z0-9]+$", artifact_token),
    ("^e_[A-Za-z0-9_-]+$", element_token),
    ("^f_[A-Za-z0-9_-]+$", frame_token),
    ("^r_[A-Za-z0-9]+$", request_token),
    ("^[a-z][a-z0-9_]*$", lower_identifier),
    (
        "^[A-Za-z][A-Za-z0-9_]*\\.[A-Za-z][A-Za-z0-9_]*$",
        cdp_method,
    ),
    ("^/", absolute_path_pattern),
    ("^[^/]", relative_path_pattern),
    ("^[a-f0-9]{64}$", sha256_pattern),
    ("^[a-f0-9]{40}$", commit_pattern),
    ("^[0-9]+\\.[0-9]+\\.[0-9]+$", semver_triplet),
    (
        "^[a-z][a-z0-9]*(\\.[a-z][a-z0-9_]*)+$",
        dotted_lower_identifier,
    ),
    (
        "^manuvra commands errors [a-z][a-z0-9_]*$",
        error_help_pattern,
    ),
];

fn session_token(text: &str) -> bool {
    prefixed_token(text, "s_", false)
}
fn artifact_token(text: &str) -> bool {
    prefixed_token(text, "a_", false)
}
fn element_token(text: &str) -> bool {
    prefixed_token(text, "e_", true)
}
fn frame_token(text: &str) -> bool {
    prefixed_token(text, "f_", true)
}
fn request_token(text: &str) -> bool {
    prefixed_token(text, "r_", false)
}
fn absolute_path_pattern(text: &str) -> bool {
    text.starts_with('/')
}
fn relative_path_pattern(text: &str) -> bool {
    !text.is_empty() && !text.starts_with('/')
}
fn sha256_pattern(text: &str) -> bool {
    lowercase_hex_of_length(text, 64)
}
fn commit_pattern(text: &str) -> bool {
    lowercase_hex_of_length(text, 40)
}

fn lowercase_hex_of_length(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn error_help_pattern(text: &str) -> bool {
    text.strip_prefix("manuvra commands errors ")
        .is_some_and(lower_identifier)
}

fn semver_triplet(text: &str) -> bool {
    let parts = text.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
}

fn prefixed_token(text: &str, prefix: &str, punctuation: bool) -> bool {
    text.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.chars().all(|character| {
                character.is_ascii_alphanumeric() || punctuation && matches!(character, '_' | '-')
            })
    })
}

fn lower_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

fn cdp_method(text: &str) -> bool {
    let Some((domain, method)) = text.split_once('.') else {
        return false;
    };
    identifier_part(domain) && identifier_part(method) && !method.contains('.')
}

fn dotted_lower_identifier(text: &str) -> bool {
    let mut parts = text.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    lower_word(first) && parts.clone().next().is_some() && parts.all(lower_identifier)
}

fn lower_word(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
}

fn identifier_part(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn looks_like_uri(text: &str) -> bool {
    text.split_once(':')
        .is_some_and(|(scheme, rest)| identifier_part(scheme) && !rest.is_empty())
}

pub fn command_id_for_route(route: &[&str]) -> Option<&'static str> {
    registry()["commands"]
        .as_array()?
        .iter()
        .find_map(|command| {
            let cli = command["cli"].as_array()?;
            let matches = cli.len() == route.len()
                && cli
                    .iter()
                    .zip(route)
                    .all(|(part, expected)| part.as_str() == Some(expected));
            matches.then(|| command["id"].as_str()).flatten()
        })
}

pub fn registry_page(cursor: usize, limit: usize) -> Value {
    let commands = registry()["commands"].as_array().expect("commands");
    let end = cursor.saturating_add(limit).min(commands.len());
    let page = commands[cursor.min(commands.len())..end]
        .iter()
        .map(compact_descriptor)
        .collect::<Vec<_>>();
    let next_cursor = (end < commands.len()).then(|| end.to_string());
    json!({
        "registry_version": REGISTRY_VERSION,
        "commands": page,
        "next_cursor": next_cursor,
    })
}

fn compact_descriptor(command: &Value) -> Value {
    json!({
        "id": command["id"],
        "cli": command["cli"],
        "summary": command["summary"],
        "effect": command["effect"],
        "authority": command["authority"],
        "modes": command["modes"],
        "default_timeout_ms": command["default_timeout_ms"],
        "capabilities": command["capabilities"],
        "since": command["since"],
    })
}

pub fn command_help(id: &str) -> Option<Value> {
    let command = command_descriptor(id)?;
    let examples = command["examples"]
        .as_array()?
        .iter()
        .map(|example| example["cli"].as_str())
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Some(json!({
        "command": command["id"],
        "summary": command["summary"],
        "when_to_use": command["summary"],
        "effects": command["effect"],
        "authority": command["authority"],
        "modes": command["modes"],
        "defaults": {
            "timeout_ms": command["default_timeout_ms"],
            "maximum_timeout_ms": command["maximum_timeout_ms"],
        },
        "input_schema": schema_pointer(command["input_schema"].as_str()?).ok()?,
        "result_schema": schema_pointer(command["result_schema"].as_str()?).ok()?,
        "errors": relevant_error_codes(command),
        "examples": examples,
    }))
}

fn relevant_error_codes(command: &Value) -> Vec<&'static str> {
    let mut codes = vec![
        "invalid_request",
        "daemon_version_mismatch",
        "internal_error",
    ];
    if command["authority"] == "session"
        || command["authority"] == "observer"
        || command["authority"] == "actor"
    {
        codes.extend(["session_not_found", "target_stale"]);
    }
    if command["authority"] == "actor" {
        codes.extend(["actor_lease_required", "actor_lease_expired"]);
    }
    if command["effect"] == "mutate" || command["effect"] == "declared" {
        codes.extend(["timed_out", "cancelled", "artifact_io_failed"]);
    }
    if command["id"] == "system.chrome.launch" {
        codes.extend(["chrome_unavailable", "chrome_endpoint_busy", "timed_out"]);
    }
    codes
}

pub fn schema_pointer(reference: &str) -> Result<Value, ProtocolError> {
    schema_pointer_at(&resource_root()?, reference)
}

pub fn schema_pointer_at(root: &Path, reference: &str) -> Result<Value, ProtocolError> {
    let (relative, pointer) = reference.split_once('#').unwrap_or((reference, ""));
    let path = fs::canonicalize(root.join(relative.trim_start_matches("./")))?;
    let bytes = fs::read(&path)?;
    Ok(json!({
        "absolute_path": path,
        "sha256": sha256_hex(&bytes),
        "json_pointer": (!pointer.is_empty()).then_some(pointer),
    }))
}

pub fn resource_root() -> Result<PathBuf, ProtocolError> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    if let Some(root) = installed_resource_root(&executable) {
        return Ok(root);
    }
    source_tree_resource_root()
}

fn source_tree_resource_root() -> Result<PathBuf, ProtocolError> {
    if cfg!(debug_assertions) {
        let override_root = std::env::var_os("MANUVRA_RESOURCE_ROOT");
        return source_assets_root(override_root.as_ref().map(Path::new));
    }
    Err(ProtocolError::InvalidInstallation(
        "executable is outside Manuvra.app/Contents/MacOS".to_owned(),
    ))
}

fn source_assets_root(override_root: Option<&Path>) -> Result<PathBuf, ProtocolError> {
    match override_root {
        Some(root) => Ok(fs::canonicalize(root)?),
        None => Ok(fs::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
        )?),
    }
}

fn installed_resource_root(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    is_manuvra_app_layout(macos, contents, bundle).then(|| contents.join("Resources"))
}

fn is_manuvra_app_layout(macos: &Path, contents: &Path, bundle: &Path) -> bool {
    path_named(macos, "MacOS")
        && path_named(contents, "Contents")
        && path_named(bundle, "Manuvra.app")
}

fn path_named(path: &Path, name: &str) -> bool {
    path.file_name().and_then(|value| value.to_str()) == Some(name)
}

pub fn release_manifest() -> Value {
    serde_json::from_str(RELEASE_MANIFEST_JSON).expect("embedded release manifest")
}

pub fn embedded_resource(relative: &str) -> Option<&'static [u8]> {
    EMBEDDED_RESOURCES
        .iter()
        .find(|(path, _)| *path == relative)
        .map(|(_, bytes)| *bytes)
}

static EMBEDDED_RESOURCES: &[(&str, &[u8])] = &[
    ("registry.json", REGISTRY_JSON.as_bytes()),
    ("error-catalog.json", ERROR_CATALOG_JSON.as_bytes()),
    ("agent-help.md", AGENT_HELP.as_bytes()),
    (
        "schemas/action-result.schema.json",
        include_bytes!("../assets/schemas/action-result.schema.json"),
    ),
    (
        "schemas/artifact-manifest.schema.json",
        include_bytes!("../assets/schemas/artifact-manifest.schema.json"),
    ),
    (
        "schemas/command-inputs.schema.json",
        COMMAND_INPUTS_SCHEMA_JSON.as_bytes(),
    ),
    (
        "schemas/command-registry.schema.json",
        include_bytes!("../assets/schemas/command-registry.schema.json"),
    ),
    (
        "schemas/command-results.schema.json",
        COMMAND_RESULTS_SCHEMA_JSON.as_bytes(),
    ),
    (
        "schemas/complete-tree-result.schema.json",
        COMPLETE_TREE_RESULT_SCHEMA_JSON.as_bytes(),
    ),
    (
        "schemas/config.schema.json",
        include_bytes!("../assets/schemas/config.schema.json"),
    ),
    (
        "schemas/error-catalog.schema.json",
        include_bytes!("../assets/schemas/error-catalog.schema.json"),
    ),
    ("schemas/error.schema.json", ERROR_SCHEMA_JSON.as_bytes()),
    (
        "schemas/exported-artifact-manifest.schema.json",
        EXPORTED_ARTIFACT_MANIFEST_SCHEMA_JSON.as_bytes(),
    ),
    (
        "schemas/protocol.schema.json",
        include_bytes!("../assets/schemas/protocol.schema.json"),
    ),
    (
        "schemas/usage.schema.json",
        include_bytes!("../assets/schemas/usage.schema.json"),
    ),
];

pub fn build_digest() -> String {
    env!("MANUVRA_BUILD_DIGEST").to_owned()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        Value::Object(map) => {
            let sorted = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        value => value.clone(),
    }
}

pub fn canonical_invocation_digest(invocation: &Invocation) -> String {
    #[derive(Serialize)]
    struct RequestIdentity<'a> {
        protocol: &'a ProtocolRange,
        registry_version: &'a str,
        build_digest: &'a str,
        request_id: &'a str,
        command: &'a str,
        input: &'a Value,
    }

    let identity = RequestIdentity {
        protocol: &invocation.protocol,
        registry_version: &invocation.registry_version,
        build_digest: &invocation.build_digest,
        request_id: &invocation.request_id,
        command: &invocation.command,
        input: &invocation.input,
    };
    let value = serde_json::to_value(identity).expect("serializable invocation identity");
    let bytes = serde_json::to_vec(&canonical_json(&value)).expect("canonical json");
    sha256_hex(&bytes)
}

pub fn encode_operational_line(value: &Value) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(ProtocolError::OperationalResultTooLarge(bytes.len()));
    }
    Ok(bytes)
}

pub fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), ProtocolError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(bytes.len()));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut impl Read,
) -> Result<T, ProtocolError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn schema_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/schemas")
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("frame contains {0} bytes, above the one MiB limit")]
    FrameTooLarge(usize),
    #[error("operational result contains {0} bytes, above the 4096-byte limit")]
    OperationalResultTooLarge(usize),
    #[error("I/O failure: {0}")]
    Io(#[from] io::Error),
    #[error("JSON failure: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid Manuvra installation: {0}")]
    InvalidInstallation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_and_error_catalog_have_accepted_cardinality() {
        assert_eq!(registry()["commands"].as_array().unwrap().len(), 31);
        assert_eq!(all_errors().len(), 46);
        assert_eq!(all_warnings().len(), 1);
        for command in registry()["commands"].as_array().unwrap() {
            let id = command["id"].as_str().unwrap();
            assert_eq!(CommandId::parse(id).unwrap().as_str(), id);
        }
        for error in all_errors() {
            assert_eq!(ErrorCode::parse(&error.code).unwrap().as_str(), error.code);
        }
        for warning in all_warnings() {
            assert_eq!(
                WarningCode::parse(&warning.code).unwrap().as_str(),
                warning.code
            );
        }
    }

    #[test]
    fn every_catalog_error_serializes_to_the_accepted_schema_and_byte_bound() {
        let schema = parsed_schema(&ERROR_SCHEMA, ERROR_SCHEMA_JSON);
        for metadata in all_errors() {
            let (error, exit_code) = operational_error(&metadata.code, None);
            let value = serde_json::to_value(error).unwrap();
            validate_schema(schema, &value, schema, "error")
                .unwrap_or_else(|message| panic!("{}: {message}", metadata.code));
            assert_eq!(exit_code, metadata.exit);
            encode_operational_line(&json!({"error": value})).unwrap();
        }
    }

    #[test]
    fn canonical_digest_ignores_object_key_order() {
        let left = json!({"b": 2, "a": {"z": 1, "x": 0}});
        let right = json!({"a": {"x": 0, "z": 1}, "b": 2});
        assert_eq!(canonical_json(&left), canonical_json(&right));
    }

    #[test]
    fn invocation_digest_treats_deadline_as_budget_not_request_content() {
        let original = Invocation::new("system.setup", json!({}), "same-request".to_owned(), 1_000);
        let mut retry = original.clone();
        retry.deadline_ms = 750;
        assert_eq!(
            canonical_invocation_digest(&original),
            canonical_invocation_digest(&retry)
        );

        retry.command = "system.doctor".to_owned();
        assert_ne!(
            canonical_invocation_digest(&original),
            canonical_invocation_digest(&retry)
        );
    }

    #[test]
    fn framing_round_trips_and_rejects_large_frame() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &json!({"ok": true})).unwrap();
        assert_eq!(
            read_frame::<Value>(&mut bytes.as_slice()).unwrap(),
            json!({"ok": true})
        );

        let mut too_large = Vec::from(((MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        too_large.extend(std::iter::repeat_n(0, MAX_FRAME_BYTES + 1));
        assert!(matches!(
            read_frame::<Value>(&mut too_large.as_slice()),
            Err(ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn operational_line_enforces_actual_utf8_bytes() {
        let exactish = json!({"value": "é".repeat(2030)});
        assert!(encode_operational_line(&exactish).is_ok());
        let large = json!({"value": "é".repeat(4096)});
        assert!(matches!(
            encode_operational_line(&large),
            Err(ProtocolError::OperationalResultTooLarge(_))
        ));
    }

    #[test]
    fn relative_path_pattern_excludes_host_absolute_paths() {
        assert!(matches_accepted_pattern(
            "^[^/]",
            "crates/manuvra/src/lib.rs"
        ));
        assert!(!matches_accepted_pattern("^[^/]", "/private/example.rs"));
    }

    #[test]
    fn every_schema_pointer_resolves() {
        for command in registry()["commands"].as_array().unwrap() {
            schema_pointer(command["input_schema"].as_str().unwrap()).unwrap();
            schema_pointer(command["result_schema"].as_str().unwrap()).unwrap();
        }
        schema_pointer("./schemas/exported-artifact-manifest.schema.json").unwrap();
    }

    #[test]
    fn registry_help_has_examples_and_authoritative_timeouts() {
        for command in registry()["commands"].as_array().unwrap() {
            let id = command["id"].as_str().unwrap();
            assert_eq!(
                command_default_timeout_ms(id),
                command["default_timeout_ms"].as_u64()
            );
            let help = command_help(id).unwrap();
            let examples = command["examples"].as_array().unwrap();
            assert!(!examples.is_empty());
            for example in examples {
                let cli = example["cli"].as_str().unwrap();
                assert!(cli.starts_with("manuvra "));
                assert!(!cli.ends_with(" --help"));
                validate_command_input(id, &example["input"]).unwrap();
            }
            assert_eq!(
                help["examples"],
                Value::Array(
                    examples
                        .iter()
                        .map(|example| example["cli"].clone())
                        .collect()
                )
            );
            validate_command_result("system.commands.get", &help).unwrap();
        }
    }

    #[test]
    fn exported_artifact_manifest_schema_is_closed_and_caller_owned() {
        let valid = json!({
            "schema": "manuvra/exported-artifact-manifest@1",
            "session_id": "s_example",
            "target_id": "chrome_example",
            "generation": 1,
            "lifetime": "caller_owned",
            "session_directory": "/tmp/exported",
            "artifacts": [{
                "artifact_id": "a_example",
                "kind": "events",
                "absolute_path": "/tmp/exported/events.jsonl",
                "media_type": "application/jsonl",
                "bytes": 2,
                "sha256": "a".repeat(64),
                "complete": true,
                "request_id": null,
                "action_sequence": null,
                "created_at": "2026-08-19T00:00:00Z",
                "lifetime": "caller_owned"
            }]
        });
        validate_exported_artifact_manifest(&valid).unwrap();

        let mut invalid = valid;
        invalid["lifetime"] = json!("until_session_close");
        assert!(validate_exported_artifact_manifest(&invalid).is_err());
        invalid["lifetime"] = json!("caller_owned");
        invalid["unexpected"] = json!(true);
        assert!(validate_exported_artifact_manifest(&invalid).is_err());
    }

    #[test]
    fn accepted_input_schema_rejects_invalid_nested_values() {
        let invalid = [
            ("system.commands.list", json!({"limit": 11})),
            ("session.close", json!({"session_id": "wrong"})),
            (
                "action.click",
                json!({"session_id": "s_1", "locator": {"kind": "semantic"}}),
            ),
            (
                "action.click",
                json!({"session_id": "s_1", "locator": {"kind": "semantic", "name": "Save", "extra": true}}),
            ),
            (
                "action.click",
                json!({"session_id": "s_1", "locator": {"kind": "semantic", "name": "Save"}, "count": 4}),
            ),
            (
                "action.type",
                json!({"session_id": "s_1", "locator": {"kind": "ref", "ref": "e_1"}, "text": 3}),
            ),
            (
                "action.scroll",
                json!({"session_id": "s_1", "delta_x": 0, "delta_y": 100001}),
            ),
            (
                "action.navigate",
                json!({"session_id": "s_1", "url": "not a uri"}),
            ),
            (
                "observe.query",
                json!({"session_id": "s_1", "semantic": {"kind": "semantic", "name": "Save"}, "limit": 6}),
            ),
            (
                "raw.cdp",
                json!({"session_id": "s_1", "intent": "query", "method": "bad", "params": []}),
            ),
            (
                "raw.ax.set",
                json!({"session_id": "s_1", "ref": "e_1", "attribute": "AXValue", "value": {"type": "string", "value": "x", "extra": 1}}),
            ),
            ("system.commands.usage", json!({"action": "export"})),
            (
                "artifact.export",
                json!({"session_id": "s_1", "destination": "/tmp/export", "all": false}),
            ),
        ];
        for (command, input) in invalid {
            assert!(
                validate_command_input(command, &input).is_err(),
                "accepted invalid input for {command}: {input}"
            );
        }
    }

    #[test]
    fn semantic_locator_accepts_optional_ancestor_scope() {
        validate_command_input(
            "action.click",
            &json!({
                "session_id": "s_1",
                "locator": {
                    "kind": "semantic",
                    "role": "button",
                    "name": "Save",
                    "within_role": "region",
                    "within_name": "Primary"
                }
            }),
        )
        .unwrap();
        validate_command_input(
            "observe.query",
            &json!({
                "session_id": "s_1",
                "semantic": {
                    "kind": "semantic",
                    "role": "button",
                    "within_role": "region"
                },
                "limit": 5
            }),
        )
        .unwrap();
    }

    #[test]
    fn accepted_input_schema_supports_recursive_tagged_ax_values() {
        let input = json!({
            "session_id": "s_1",
            "ref": "e_1",
            "attribute": "AXValue",
            "value": {
                "type": "dictionary",
                "value": {
                    "items": {"type": "array", "value": [
                        {"type": "string", "value": "hello"},
                        {"type": "range", "location": 0, "length": 5}
                    ]}
                }
            }
        });
        validate_command_input("raw.ax.set", &input).unwrap();
    }

    #[test]
    fn build_digest_is_a_full_sha256_identity() {
        let digest = build_digest();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn target_page_requires_presentation_owner_and_title() {
        let old_page = json!({
            "targets": [{
                "target_id": "chrome_example",
                "generation": 1,
                "kind": "chrome",
                "capabilities": ["observation.screenshot"],
                "actor_lease": "available"
            }],
            "next_cursor": null
        });
        let error = validate_command_result("target.list", &old_page)
            .expect_err("pages without owner/title must fail required fields");
        assert!(
            error.contains("owner is required"),
            "old page must fail for missing owner: {error}"
        );

        let mut page = json!({
            "targets": [
                {
                    "target_id": "chrome_example",
                    "generation": 1,
                    "kind": "chrome",
                    "owner": "Chrome",
                    "title": "Inbox",
                    "capabilities": ["observation.screenshot"],
                    "actor_lease": "available"
                },
                {
                    "target_id": "macos_example",
                    "generation": 2,
                    "kind": "macos",
                    "owner": "TextEdit",
                    "title": null,
                    "capabilities": ["observation.screenshot"],
                    "actor_lease": "held"
                }
            ],
            "next_cursor": null
        });
        validate_command_result("target.list", &page).unwrap();

        page["targets"][0]["owner"] = json!("");
        let empty_owner = validate_command_result("target.list", &page)
            .expect_err("empty owner must fail minLength");
        assert!(
            empty_owner.contains("too short"),
            "empty owner must be rejected: {empty_owner}"
        );

        page["targets"][0]["owner"] = json!("Chrome");
        page["targets"][0]["title"] = json!("");
        let empty_title = validate_command_result("target.list", &page)
            .expect_err("empty title must fail minLength");
        assert!(
            empty_title.contains("too short"),
            "empty title must be rejected: {empty_title}"
        );

        page["targets"][0]["title"] = json!("Inbox");
        page["targets"][0]["parsed_id"] = json!("no");
        let extra =
            validate_command_result("target.list", &page).expect_err("target items are closed");
        assert!(
            extra.contains("not allowed"),
            "extra target keys must be rejected: {extra}"
        );
    }

    #[test]
    fn external_schema_accepts_matching_json_types_and_rejects_mismatches() {
        let cases = [
            (json!({"type": "object"}), json!({"ok": true}), json!([])),
            (json!({"type": "array"}), json!([1]), json!({})),
            (json!({"type": "string"}), json!("ok"), json!(1)),
            (json!({"type": "boolean"}), json!(true), json!("true")),
            (json!({"type": "null"}), json!(null), json!(false)),
            (json!({"type": "number"}), json!(1.5), json!("1.5")),
            (json!({"type": "integer"}), json!(1), json!(1.5)),
        ];
        for (schema, accepted, rejected) in cases {
            validate_external_document(&schema, &accepted).unwrap_or_else(|error| {
                panic!("accepted value rejected: {schema} {accepted} {error}")
            });
            let error = validate_external_document(&schema, &rejected)
                .expect_err("mismatched JSON type must fail");
            assert!(
                error.contains("wrong JSON type"),
                "type mismatch must name the JSON type: {error}"
            );
        }

        validate_external_document(&json!({"type": "integer"}), &json!(u64::MAX)).unwrap();
        validate_external_document(&json!({"type": "number"}), &json!(1)).unwrap();
        let union = json!({"type": ["string", "null"]});
        validate_external_document(&union, &json!("ok")).unwrap();
        validate_external_document(&union, &json!(null)).unwrap();
        assert!(validate_external_document(&union, &json!(1)).is_err());
        assert!(
            validate_external_document(&json!({"type": "date"}), &json!("2026-08-21")).is_err()
        );
    }

    #[test]
    fn external_schema_enforces_object_and_array_constraints() {
        let object = json!({
            "type": "object",
            "required": ["id"],
            "maxProperties": 1,
            "additionalProperties": false,
            "properties": { "id": { "type": "string", "const": "ok" } }
        });
        validate_external_document(&object, &json!({"id": "ok"})).unwrap();
        let missing = validate_external_document(&object, &json!({}))
            .expect_err("required property must fail");
        assert!(
            missing.contains("id is required"),
            "missing required key must be named: {missing}"
        );
        let extra = validate_external_document(&object, &json!({"id": "ok", "n": 1}))
            .expect_err("maxProperties must fail before extra keys are accepted");
        assert!(
            extra.contains("too many properties"),
            "two properties must exceed maxProperties 1: {extra}"
        );
        let closed = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "id": { "type": "string" } }
        });
        let unknown = validate_external_document(&closed, &json!({"n": 1}))
            .expect_err("unknown keys must fail additionalProperties");
        assert!(
            unknown.contains("not allowed"),
            "unknown key must be rejected: {unknown}"
        );
        let typed_extra = json!({
            "type": "object",
            "additionalProperties": { "type": "integer" }
        });
        validate_external_document(&typed_extra, &json!({"n": 1})).unwrap();
        assert!(validate_external_document(&typed_extra, &json!({"n": "1"})).is_err());

        let array = json!({
            "type": "array",
            "minItems": 1,
            "maxItems": 2,
            "uniqueItems": true,
            "items": { "type": "integer" }
        });
        validate_external_document(&array, &json!([1, 2])).unwrap();
        let empty = validate_external_document(&array, &json!([])).expect_err("minItems must fail");
        assert!(
            empty.contains("too few items"),
            "empty array must fail minItems: {empty}"
        );
        let long =
            validate_external_document(&array, &json!([1, 2, 3])).expect_err("maxItems must fail");
        assert!(
            long.contains("too many items"),
            "three items must fail maxItems 2: {long}"
        );
        let duplicates =
            validate_external_document(&array, &json!([1, 1])).expect_err("uniqueItems must fail");
        assert!(
            duplicates.contains("duplicate items"),
            "duplicate integers must fail uniqueItems: {duplicates}"
        );
        let wrong_item = validate_external_document(&array, &json!([1, "2"]))
            .expect_err("item schema must fail");
        assert!(
            wrong_item.contains("wrong JSON type"),
            "string item must fail integer items: {wrong_item}"
        );
    }

    #[test]
    fn artifact_export_input_rejects_duplicate_or_empty_artifact_ids() {
        let unique = json!({
            "session_id": "s_1",
            "destination": "/tmp/export",
            "artifact_ids": ["a_1", "a_2"]
        });
        validate_command_input("artifact.export", &unique).unwrap();

        let duplicates = json!({
            "session_id": "s_1",
            "destination": "/tmp/export",
            "artifact_ids": ["a_1", "a_1"]
        });
        let error = validate_command_input("artifact.export", &duplicates)
            .expect_err("duplicate artifact ids must fail uniqueItems");
        assert!(
            error.contains("duplicate items"),
            "duplicate artifact_ids must be rejected: {error}"
        );

        let empty = json!({
            "session_id": "s_1",
            "destination": "/tmp/export",
            "artifact_ids": []
        });
        let error = validate_command_input("artifact.export", &empty)
            .expect_err("empty artifact ids must fail minItems");
        assert!(
            error.contains("too few items"),
            "empty artifact_ids must be rejected: {error}"
        );
    }

    #[test]
    fn external_schema_follows_local_refs_and_rejects_unknown_refs() {
        let schema = json!({
            "$defs": { "token": { "type": "string", "const": "ok" } },
            "$ref": "#/$defs/token"
        });
        validate_external_document(&schema, &json!("ok")).unwrap();
        let wrong = validate_external_document(&schema, &json!("no"))
            .expect_err("const through $ref must fail");
        assert!(
            wrong.contains("required constant"),
            "local $ref must keep const rejection: {wrong}"
        );
        let unresolved =
            validate_external_document(&json!({"$ref": "#/$defs/missing"}), &json!("ok"))
                .expect_err("missing local $ref must fail");
        assert!(
            unresolved.contains("unresolved schema reference"),
            "missing pointer must be unresolved: {unresolved}"
        );
        let unsupported = validate_external_document(
            &json!({"$ref": "https://example.invalid/schema"}),
            &json!({}),
        )
        .expect_err("unknown external $ref must fail");
        assert!(
            unsupported.contains("unsupported schema reference"),
            "unknown $ref must stay unsupported: {unsupported}"
        );
    }

    #[test]
    fn resource_root_and_schema_pointer_resolve_packaged_assets() {
        let expected = match std::env::var_os("MANUVRA_RESOURCE_ROOT") {
            Some(root) => fs::canonicalize(root).unwrap(),
            None => fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")).unwrap(),
        };
        let root = resource_root().unwrap();
        assert_eq!(root, expected);
        assert!(root.join("registry.json").is_file());
        let pointer = schema_pointer_at(&root, "./schemas/error.schema.json").unwrap();
        assert!(
            pointer["absolute_path"]
                .as_str()
                .unwrap()
                .ends_with("error.schema.json")
        );
        assert!(schema_pointer_at(&root, "./schemas/missing.schema.json").is_err());
    }

    #[test]
    fn source_assets_root_prefers_override_then_crate_assets() {
        let crate_assets =
            fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")).unwrap();
        assert_eq!(source_assets_root(None).unwrap(), crate_assets);

        let override_root = unique_temp_dir("source-assets");
        fs::create_dir_all(&override_root).unwrap();
        assert_eq!(
            source_assets_root(Some(&override_root)).unwrap(),
            fs::canonicalize(&override_root).unwrap()
        );
        fs::remove_dir_all(&override_root).unwrap();
        assert!(source_assets_root(Some(&override_root)).is_err());
    }

    #[test]
    fn installed_resource_root_requires_manuvra_app_layout() {
        let home = unique_temp_dir("installed-root");
        let macos = home.join("Manuvra.app/Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let executable = macos.join("manuvra");
        fs::write(&executable, []).unwrap();
        assert_eq!(
            installed_resource_root(&executable),
            Some(home.join("Manuvra.app/Contents/Resources"))
        );

        let wrong_macos = home.join("Manuvra.app/Contents/bin/manuvra");
        fs::create_dir_all(wrong_macos.parent().unwrap()).unwrap();
        fs::write(&wrong_macos, []).unwrap();
        assert_eq!(installed_resource_root(&wrong_macos), None);

        let wrong_contents = home.join("Manuvra.app/Resources/MacOS/manuvra");
        fs::create_dir_all(wrong_contents.parent().unwrap()).unwrap();
        fs::write(&wrong_contents, []).unwrap();
        assert_eq!(installed_resource_root(&wrong_contents), None);

        let wrong_bundle = home.join("Other.app/Contents/MacOS/manuvra");
        fs::create_dir_all(wrong_bundle.parent().unwrap()).unwrap();
        fs::write(&wrong_bundle, []).unwrap();
        assert_eq!(installed_resource_root(&wrong_bundle), None);

        assert_eq!(installed_resource_root(Path::new("manuvra")), None);
        fs::remove_dir_all(&home).unwrap();
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "manuvra-protocol-s1-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
