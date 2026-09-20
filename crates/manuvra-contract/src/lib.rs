use std::collections::{BTreeMap, BTreeSet};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema, schema_for};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use url::Url;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchemaVersion;

impl Serialize for SchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(SCHEMA_VERSION)
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u32::deserialize(deserializer)?;
        if version == SCHEMA_VERSION {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "schema_version must be {SCHEMA_VERSION}"
            )))
        }
    }
}

impl JsonSchema for SchemaVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SchemaVersion".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "const": SCHEMA_VERSION})
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequiredTrue;

impl Serialize for RequiredTrue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for RequiredTrue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("nonempty must be true"))
        }
    }
}

impl JsonSchema for RequiredTrue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RequiredTrue".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "boolean", "const": true})
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub schema_version: SchemaVersion,
    pub target: Target,
    pub context: JobContext,
    #[serde(default)]
    pub values: BTreeMap<String, JobValue>,
    #[schemars(length(min = 1))]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expectations: Vec<Expectation>,
    #[serde(default)]
    pub options: JobOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Browser { url: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobContext {
    pub journey: String,
    pub revision: String,
    pub environment: String,
    pub actor: String,
    pub authority: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobValue {
    pub value: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats: Option<ValueFormats>,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValueFormats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub goal: String,
    pub done_when: DoneCondition,
    #[serde(default)]
    pub requires_values: Vec<String>,
    #[serde(default = "default_mutation_limit")]
    #[schemars(range(min = 1, max = 8))]
    pub mutation_limit: u8,
}

const fn default_mutation_limit() -> u8 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DoneCondition {
    Structured(#[schemars(length(min = 1))] Vec<Assertion>),
    NaturalLanguage(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Assertion {
    TextVisible(TextVisible),
    TextAbsent(TextAbsent),
    FieldNonempty(FieldNonempty),
    FieldEqualsValue(FieldEqualsValue),
    DialogOpen(DialogOpen),
    DialogClosed(DialogClosed),
    UrlContains(UrlContains),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextVisible {
    pub text_visible: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<AssertionScope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextAbsent {
    pub text_absent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<AssertionScope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AssertionScope {
    Viewport(ViewportScope),
    Dialog(DialogScope),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewportScope {
    Viewport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DialogScope {
    pub dialog: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldNonempty {
    pub field: String,
    pub nonempty: RequiredTrue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldEqualsValue {
    pub field: String,
    pub equals_value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DialogOpen {
    pub dialog_open: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DialogClosed {
    pub dialog_closed: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UrlContains {
    pub url_contains: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub id: String,
    pub claim: String,
    #[serde(default)]
    pub exact_literals: Vec<ExactLiteral>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExactLiteral {
    pub literal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct JobOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3_600_000))]
    pub active_timeout_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3_600_000))]
    pub pause_timeout_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3_600_000))]
    pub lifetime_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 500))]
    pub max_actions: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_model_calls: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact_values: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<DebugOptions>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    #[schemars(range(min = 320, max = 7680))]
    pub width: u16,
    #[schemars(range(min = 240, max = 4320))]
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DebugOptions {
    pub force_stop_at_step: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunResult {
    pub schema_version: SchemaVersion,
    pub request_id: String,
    pub run_id: String,
    pub state: RunState,
    pub terminal: bool,
    pub reason: Option<Reason>,
    pub verdict: Verdict,
    pub evidence: EvidenceRef,
    pub escalation: Option<Escalation>,
    pub cleanup: Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Uncertain,
    Passed,
    Failed,
    Blocked,
    Aborted,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Reason {
    pub code: String,
    #[serde(flatten)]
    pub details: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verdict {
    pub overall: VerdictResult,
    pub steps: Vec<StepVerdict>,
    pub expectations: Vec<ExpectationVerdict>,
    pub caller_assisted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerdictResult {
    Satisfied,
    NotSatisfied,
    Unresolved,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepVerdict {
    pub id: String,
    pub result: VerdictResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExpectationVerdict {
    pub id: String,
    pub result: VerdictResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
    #[serde(default)]
    pub numeric_checks: Vec<NumericCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NumericCheck {
    pub literal: String,
    pub present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceRef {
    pub manifest: String,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Escalation {
    pub id: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub expires_at: String,
    pub payload: String,
    pub dispositions: Vec<DispositionKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Cleanup {
    pub browser: String,
    pub profile: String,
    pub application_state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispositionRequest {
    pub schema_version: SchemaVersion,
    pub escalation_id: String,
    pub disposition: Disposition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Disposition {
    Advance(AdvanceDisposition),
    Execute(ExecuteDisposition),
    RetryObservation(RetryObservationDisposition),
    Abort(AbortDisposition),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdvanceDisposition {
    pub kind: AdvanceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdvanceKind {
    Advance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteDisposition {
    pub kind: ExecuteKind,
    pub candidate_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteKind {
    Execute,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryObservationDisposition {
    pub kind: RetryObservationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetryObservationKind {
    RetryObservation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbortDisposition {
    pub kind: AbortKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AbortKind {
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispositionKind {
    Advance,
    Execute,
    RetryObservation,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Manifest {
    pub schema_version: SchemaVersion,
    pub run_id: String,
    pub complete: bool,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    pub role: String,
    pub path: String,
    pub digest: String,
    pub complete: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct ValidationError {
    pub message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Job {
    pub fn parse(bytes: &[u8]) -> Result<Self, ValidationError> {
        let job: Self = serde_json::from_slice(bytes)
            .map_err(|error| ValidationError::new(format!("invalid job JSON: {error}")))?;
        job.validate()?;
        Ok(job)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        self.validate_envelope()?;
        self.validate_members()
    }

    fn validate_envelope(&self) -> Result<(), ValidationError> {
        self.context.validate()?;
        self.target.validate()?;
        validate_nonempty_steps(&self.steps)?;
        validate_unique_ids("step", self.steps.iter().map(|step| step.id.as_str()))?;
        validate_unique_ids("expectation", self.expectation_ids())
    }

    fn validate_members(&self) -> Result<(), ValidationError> {
        validate_all(self.values.iter().map(|(name, value)| value.validate(name)))?;
        validate_all(self.steps.iter().map(Step::validate))?;
        validate_all(self.expectations.iter().map(Expectation::validate))?;
        self.options.validate(self)
    }

    pub fn first_missing_value(&self) -> Option<MissingValue> {
        self.steps
            .iter()
            .find_map(|step| step.first_missing_value(&self.values))
    }

    pub fn first_unsupported_feature(&self) -> Option<&'static str> {
        if self
            .steps
            .iter()
            .any(|step| matches!(step.done_when, DoneCondition::NaturalLanguage(_)))
        {
            return Some("natural_language_done_condition");
        }
        if !self.expectations.is_empty() {
            return Some("expectations");
        }
        self.options.first_unsupported_feature()
    }

    fn expectation_ids(&self) -> impl Iterator<Item = &str> {
        self.expectations
            .iter()
            .map(|expectation| expectation.id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingValue {
    pub value_name: String,
    pub step_id: String,
}

impl JobOptions {
    fn validate(&self, job: &Job) -> Result<(), ValidationError> {
        let lifetime = self.lifetime_ms.unwrap_or(900_000);
        validate_range("options.lifetime_ms", lifetime, 1, 3_600_000)?;
        self.validate_timeouts(lifetime)?;
        self.validate_budgets()?;
        validate_optional(&self.viewport, Viewport::validate)?;
        validate_optional(&self.allowed_origins, |origins| validate_origins(origins))?;
        validate_optional(&self.redact_values, |names| validate_redactions(names, job))?;
        validate_optional(&self.debug, |debug| debug.validate(job))?;
        Ok(())
    }

    fn validate_timeouts(&self, lifetime: u32) -> Result<(), ValidationError> {
        validate_range(
            "options.active_timeout_ms",
            self.active_timeout_ms.unwrap_or(120_000),
            1,
            lifetime,
        )?;
        validate_range(
            "options.pause_timeout_ms",
            self.pause_timeout_ms.unwrap_or(300_000),
            1,
            lifetime,
        )
    }

    fn validate_budgets(&self) -> Result<(), ValidationError> {
        validate_range(
            "options.max_actions",
            u32::from(self.max_actions.unwrap_or(80)),
            1,
            500,
        )?;
        validate_range(
            "options.max_model_calls",
            u32::from(self.max_model_calls.unwrap_or(120)),
            1,
            1000,
        )
    }

    fn first_unsupported_feature(&self) -> Option<&'static str> {
        [
            (self.pause_timeout_ms.is_some(), "options.pause_timeout_ms"),
            (self.lifetime_ms.is_some(), "options.lifetime_ms"),
            (self.debug.is_some(), "options.debug"),
        ]
        .into_iter()
        .find_map(|(present, name)| present.then_some(name))
    }
}

impl JobContext {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("context.journey", &self.journey)?;
        validate_nonempty("context.revision", &self.revision)?;
        validate_nonempty("context.environment", &self.environment)?;
        validate_nonempty("context.actor", &self.actor)?;
        validate_nonempty("context.authority", &self.authority)
    }
}

impl Target {
    fn validate(&self) -> Result<(), ValidationError> {
        let Self::Browser { url } = self;
        validate_http_url("target.url", url)
    }
}

impl JobValue {
    fn validate(&self, name: &str) -> Result<(), ValidationError> {
        validate_nonempty("value name", name)?;
        validate_nonempty(&format!("values.{name}.description"), &self.description)?;
        validate_optional(&self.formats, |formats| formats.validate(name))
    }
}

impl ValueFormats {
    fn validate(&self, name: &str) -> Result<(), ValidationError> {
        if self.iso.is_some() || self.display.is_some() {
            Ok(())
        } else {
            Err(ValidationError::new(format!(
                "values.{name}.formats must contain iso or display"
            )))
        }
    }
}

impl Step {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("step.id", &self.id)?;
        validate_nonempty(&format!("step {} goal", self.id), &self.goal)?;
        validate_range(
            &format!("step {} mutation_limit", self.id),
            u32::from(self.mutation_limit),
            1,
            8,
        )?;
        self.done_when.validate(&self.id)?;
        reject_duplicates(
            &format!("step {} requires_values", self.id),
            self.requires_values.iter().map(String::as_str),
        )
    }

    fn first_missing_value(&self, values: &BTreeMap<String, JobValue>) -> Option<MissingValue> {
        let name = self
            .requires_values
            .iter()
            .find(|name| !values.contains_key(*name))
            .or_else(|| self.done_when.first_missing_value(values))?;
        Some(MissingValue {
            value_name: name.clone(),
            step_id: self.id.clone(),
        })
    }
}

impl DoneCondition {
    fn validate(&self, step_id: &str) -> Result<(), ValidationError> {
        match self {
            Self::NaturalLanguage(text) => {
                validate_nonempty(&format!("step {step_id} done_when"), text)
            }
            Self::Structured(assertions) => {
                validate_nonempty_assertions(step_id, assertions)?;
                validate_all(assertions.iter().map(Assertion::validate))
            }
        }
    }

    fn first_missing_value<'a>(
        &'a self,
        values: &BTreeMap<String, JobValue>,
    ) -> Option<&'a String> {
        let Self::Structured(assertions) = self else {
            return None;
        };
        assertions.iter().find_map(|assertion| match assertion {
            Assertion::FieldEqualsValue(field) if !values.contains_key(&field.equals_value) => {
                Some(&field.equals_value)
            }
            _ => None,
        })
    }
}

impl Assertion {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::TextVisible(value) => validate_nonempty("text_visible", &value.text_visible),
            Self::TextAbsent(value) => validate_nonempty("text_absent", &value.text_absent),
            Self::FieldNonempty(value) => value.validate(),
            Self::FieldEqualsValue(value) => value.validate(),
            Self::DialogOpen(value) => validate_nonempty("dialog_open", &value.dialog_open),
            Self::DialogClosed(value) => validate_nonempty("dialog_closed", &value.dialog_closed),
            Self::UrlContains(value) => validate_nonempty("url_contains", &value.url_contains),
        }
    }
}

impl FieldNonempty {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("field", &self.field)
    }
}

impl FieldEqualsValue {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("field", &self.field)?;
        validate_nonempty("equals_value", &self.equals_value)
    }
}

impl Expectation {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("expectation.id", &self.id)?;
        validate_nonempty(&format!("expectation {} claim", self.id), &self.claim)?;
        validate_all(self.exact_literals.iter().map(ExactLiteral::validate))
    }
}

impl ExactLiteral {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_nonempty("exact_literals.literal", &self.literal)?;
        validate_optional(&self.within_text, |text| {
            validate_nonempty("exact_literals.within_text", text)
        })
    }
}

impl Viewport {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_range("options.viewport.width", u32::from(self.width), 320, 7680)?;
        validate_range("options.viewport.height", u32::from(self.height), 240, 4320)
    }
}

impl DebugOptions {
    fn validate(&self, job: &Job) -> Result<(), ValidationError> {
        if job
            .steps
            .iter()
            .any(|step| step.id == self.force_stop_at_step)
        {
            Ok(())
        } else {
            Err(ValidationError::new(format!(
                "options.debug.force_stop_at_step names unknown step {}",
                self.force_stop_at_step
            )))
        }
    }
}

fn validate_nonempty_steps(steps: &[Step]) -> Result<(), ValidationError> {
    if steps.is_empty() {
        Err(ValidationError::new("steps must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_nonempty_assertions(
    step_id: &str,
    assertions: &[Assertion],
) -> Result<(), ValidationError> {
    if assertions.is_empty() {
        Err(ValidationError::new(format!(
            "step {step_id} done_when must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn validate_origins(origins: &[String]) -> Result<(), ValidationError> {
    if origins.is_empty() {
        return Err(ValidationError::new(
            "options.allowed_origins must not be empty",
        ));
    }
    reject_duplicates(
        "options.allowed_origins",
        origins.iter().map(String::as_str),
    )?;
    validate_all(origins.iter().map(|origin| validate_origin(origin)))
}

fn validate_redactions(names: &[String], job: &Job) -> Result<(), ValidationError> {
    reject_duplicates("options.redact_values", names.iter().map(String::as_str))?;
    validate_all(names.iter().map(|name| {
        if job.values.contains_key(name) {
            Ok(())
        } else {
            Err(ValidationError::new(format!(
                "options.redact_values names unknown value {name}"
            )))
        }
    }))
}

fn validate_all(
    results: impl IntoIterator<Item = Result<(), ValidationError>>,
) -> Result<(), ValidationError> {
    results.into_iter().collect()
}

fn validate_optional<T>(
    value: &Option<T>,
    validate: impl FnOnce(&T) -> Result<(), ValidationError>,
) -> Result<(), ValidationError> {
    value.as_ref().map_or(Ok(()), validate)
}

fn validate_unique_ids<'a>(
    kind: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), ValidationError> {
    let ids: Vec<_> = ids.into_iter().collect();
    for id in &ids {
        validate_nonempty(&format!("{kind}.id"), id)?;
    }
    reject_duplicates(&format!("{kind} ids"), ids)
}

fn reject_duplicates<'a>(
    field: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), ValidationError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(ValidationError::new(format!(
                "{field} contains duplicate {value}"
            )));
        }
    }
    Ok(())
}

fn validate_nonempty(field: &str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::new(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn validate_range(
    field: &str,
    value: u32,
    minimum: u32,
    maximum: u32,
) -> Result<(), ValidationError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::new(format!(
            "{field} must be between {minimum} and {maximum}"
        )))
    }
}

fn validate_http_url(field: &str, value: &str) -> Result<(), ValidationError> {
    let url = Url::parse(value)
        .map_err(|error| ValidationError::new(format!("{field} is invalid: {error}")))?;
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        Ok(())
    } else {
        Err(ValidationError::new(format!(
            "{field} must be an HTTP(S) URL"
        )))
    }
}

fn validate_origin(value: &str) -> Result<(), ValidationError> {
    let url = Url::parse(value).map_err(|error| {
        ValidationError::new(format!(
            "options.allowed_origins has invalid origin: {error}"
        ))
    })?;
    if !is_bare_http_origin(&url) {
        return Err(ValidationError::new(format!(
            "options.allowed_origins entry is not scheme://host[:port]: {value}"
        )));
    }
    Ok(())
}

fn is_bare_http_origin(url: &Url) -> bool {
    is_http_with_host(url) && has_no_origin_suffix(url) && has_no_credentials(url)
}

fn is_http_with_host(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

fn has_no_origin_suffix(url: &Url) -> bool {
    url.path() == "/" && url.query().is_none() && url.fragment().is_none()
}

fn has_no_credentials(url: &Url) -> bool {
    url.username().is_empty() && url.password().is_none()
}

pub enum SchemaKind {
    Job,
    Result,
    Disposition,
    Manifest,
}

pub fn schema(kind: SchemaKind) -> serde_json::Value {
    let schema = match kind {
        SchemaKind::Job => schema_for!(Job),
        SchemaKind::Result => schema_for!(RunResult),
        SchemaKind::Disposition => schema_for!(DispositionRequest),
        SchemaKind::Manifest => schema_for!(Manifest),
    };
    serde_json::to_value(schema).expect("generated schemas serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn valid_job() -> Value {
        json!({
            "schema_version": 1,
            "target": {"kind": "browser", "url": "http://127.0.0.1:4351/"},
            "context": {
                "journey": "Create account", "revision": "abc", "environment": "fixture",
                "actor": "synthetic owner", "authority": "fixture only"
            },
            "values": {"account_name": {"value": "Wallet", "description": "Account name"}},
            "steps": [{
                "id": "name", "goal": "Fill name", "requires_values": ["account_name"],
                "done_when": [{"field": "Account name", "equals_value": "account_name"}]
            }],
            "expectations": [],
            "options": {"allowed_origins": ["http://127.0.0.1:4351"]}
        })
    }

    fn parse(value: &Value) -> Result<Job, ValidationError> {
        Job::parse(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn accepts_each_assertion_shape() {
        let assertions = json!([
            {"text_visible": "Saved", "scope": "viewport"},
            {"text_absent": "Error", "scope": {"dialog": "Create"}},
            {"field": "Name", "nonempty": true, "dialog": "Create", "role": "textbox"},
            {"field": "Name", "equals_value": "account_name"},
            {"dialog_open": "Create"}, {"dialog_closed": "Other"}, {"url_contains": "/accounts"}
        ]);
        let mut value = valid_job();
        value["steps"][0]["done_when"] = assertions;
        assert!(parse(&value).is_ok());
    }

    #[test]
    fn accepts_value_formats_and_expectation_literals() {
        let mut value = valid_job();
        value["values"]["account_name"]["formats"] = json!({"iso": "Wallet", "display": "Wallet"});
        value["expectations"] = json!([{
            "id": "account", "claim": "The final page shows 12.34",
            "exact_literals": [{"literal": "12.34", "within_text": "Wallet"}]
        }]);
        assert!(parse(&value).is_ok());
    }

    #[test]
    fn accepts_the_design_brief_job_fixture() {
        let job = Job::parse(include_bytes!("../tests/fixtures/create-account.json")).unwrap();
        assert_eq!(job.steps.len(), 9);
        assert_eq!(job.expectations.len(), 2);
        assert_eq!(job.first_missing_value(), None);
    }

    #[test]
    fn rejects_malformed_contract_shapes() {
        type Mutation = Box<dyn Fn(&mut Value)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|job| job["unknown"] = json!(true)),
            Box::new(|job| job["target"]["kind"] = json!("native")),
            Box::new(|job| job["steps"] = json!([])),
            Box::new(|job| {
                let duplicate = job["steps"][0].clone();
                job["steps"].as_array_mut().unwrap().push(duplicate);
            }),
            Box::new(|job| job["steps"][0]["mutation_limit"] = json!(9)),
            Box::new(|job| {
                job["steps"][0]["done_when"] = json!([{"text_visible": "x", "text_absent": "y"}])
            }),
            Box::new(|job| {
                job["steps"][0]["done_when"] = json!([{"field": "Name", "nonempty": false}])
            }),
            Box::new(|job| job["options"]["active_timeout_ms"] = json!(0)),
            Box::new(|job| job["options"]["max_model_calls"] = json!(1001)),
            Box::new(|job| job["options"]["viewport"] = json!({"width": 100, "height": 800})),
            Box::new(|job| job["options"]["redact_values"] = json!(["unknown"])),
            Box::new(|job| job["options"]["operation_threshold"] = json!(0.5)),
            Box::new(|job| {
                job["expectations"] = json!([
                    {"id": "duplicate", "claim": "one"},
                    {"id": "duplicate", "claim": "two"}
                ])
            }),
        ];
        for mutate in mutations {
            let mut job = valid_job();
            mutate(&mut job);
            assert!(parse(&job).is_err(), "unexpectedly accepted {job}");
        }
    }

    #[test]
    fn resolves_missing_references_in_plan_order() {
        let mut value = valid_job();
        value["values"] = json!({});
        let job = parse(&value).unwrap();
        assert_eq!(
            job.first_missing_value(),
            Some(MissingValue {
                value_name: "account_name".into(),
                step_id: "name".into()
            })
        );

        let mut equality_only = valid_job();
        equality_only["values"] = json!({});
        equality_only["steps"][0]["requires_values"] = json!([]);
        assert_eq!(
            parse(&equality_only).unwrap().first_missing_value(),
            Some(MissingValue {
                value_name: "account_name".into(),
                step_id: "name".into()
            })
        );
    }

    #[test]
    fn disposition_input_is_closed_and_result_output_is_additive() {
        for disposition in [
            json!({"kind": "advance"}),
            json!({"kind": "execute", "candidate_id": "c_1"}),
            json!({"kind": "retry_observation"}),
            json!({"kind": "abort"}),
        ] {
            let request = json!({
                "schema_version": 1,
                "escalation_id": "e_1",
                "disposition": disposition
            });
            assert!(serde_json::from_value::<DispositionRequest>(request).is_ok());
        }
        let invalid = json!({
            "schema_version": 1,
            "escalation_id": "e_1",
            "disposition": {"kind": "advance", "candidate_id": "not_allowed"}
        });
        assert!(serde_json::from_value::<DispositionRequest>(invalid).is_err());

        let output = json!({
            "schema_version": 1, "request_id": "q", "run_id": "r", "state": "blocked",
            "terminal": true, "reason": null,
            "verdict": {"overall": "unresolved", "steps": [], "expectations": [], "caller_assisted": false},
            "evidence": {"manifest": "/tmp/manifest.json", "complete": true},
            "escalation": null,
            "cleanup": {"browser": "not_started", "profile": "not_created", "application_state": "caller_owned"},
            "future_addition": true
        });
        assert!(serde_json::from_value::<RunResult>(output).is_ok());
    }

    #[test]
    fn names_the_first_feature_not_supported_by_current_build() {
        let mut natural = valid_job();
        natural["steps"][0]["done_when"] = json!("The account exists");
        assert_eq!(
            parse(&natural).unwrap().first_unsupported_feature(),
            Some("natural_language_done_condition")
        );

        let mut expectation = valid_job();
        expectation["expectations"] = json!([{"id": "account", "claim": "Account exists"}]);
        assert_eq!(
            parse(&expectation).unwrap().first_unsupported_feature(),
            Some("expectations")
        );

        let mut option = valid_job();
        option["options"]["pause_timeout_ms"] = json!(100_000);
        assert_eq!(
            parse(&option).unwrap().first_unsupported_feature(),
            Some("options.pause_timeout_ms")
        );
    }

    #[test]
    fn schemas_expose_version_and_closed_input_objects() {
        let job = schema(SchemaKind::Job);
        assert_eq!(job["$defs"]["SchemaVersion"]["const"], 1);
        assert_eq!(job["$defs"]["RequiredTrue"]["const"], true);
        assert_eq!(job["additionalProperties"], false);
        for kind in [
            SchemaKind::Result,
            SchemaKind::Disposition,
            SchemaKind::Manifest,
        ] {
            assert_eq!(schema(kind)["$defs"]["SchemaVersion"]["const"], 1);
        }
    }
}
