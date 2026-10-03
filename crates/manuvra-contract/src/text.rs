use crate::{AssertionScope, TextAbsent, TextVisible, VerdictResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TextAssertion {
    Visible(TextVisible),
    Absent(TextAbsent),
}

impl TextAssertion {
    pub fn text(&self) -> &str {
        match self {
            Self::Visible(wanted) => &wanted.text_visible,
            Self::Absent(wanted) => &wanted.text_absent,
        }
    }

    pub fn scope(&self) -> Option<&AssertionScope> {
        match self {
            Self::Visible(wanted) => wanted.scope.as_ref(),
            Self::Absent(wanted) => wanted.scope.as_ref(),
        }
    }

    pub fn include_aria_hidden(&self) -> bool {
        match self {
            Self::Visible(wanted) => wanted.include_aria_hidden,
            Self::Absent(wanted) => wanted.include_aria_hidden,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextCheckEvidence {
    pub assertion: TextAssertion,
    pub result: VerdictResult,
    pub searched_channels: Vec<TextChannel>,
    pub matched_channel: Option<TextChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<TextFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextChannel {
    Accessible,
    PaintedAriaHidden,
    DialogText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextFailure {
    AmbiguousOrMissingScope,
    IncompleteCoverage,
}
