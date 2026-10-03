use crate::{ValidationError, VerdictResult, validate_nonempty, validate_optional};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColorAssertion {
    pub color: ColorCheck,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ColorCheck {
    Equals(ExactColor),
    SameAs(SameColor),
    DifferentFrom(DifferentColor),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExactColor {
    pub target: ColorTarget,
    #[schemars(regex(pattern = "^#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?$"))]
    pub equals: String,
    #[serde(default)]
    pub tolerance: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SameColor {
    pub target: ColorTarget,
    pub same_as: ColorTarget,
    #[serde(default)]
    pub tolerance: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DifferentColor {
    pub target: ColorTarget,
    pub different_from: ColorTarget,
    #[serde(default)]
    pub tolerance: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ColorTarget {
    Text(TextColorTarget),
    Name(NamedColorTarget),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextColorTarget {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NamedColorTarget {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

impl TextColorTarget {
    /// Match the browser's ECMAScript `\s` collapse and trim for a single text segment.
    pub fn normalized_text(&self) -> String {
        self.text
            .split(color_text_space)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn color_text_space(character: char) -> bool {
    // ECMAScript WhiteSpace + LineTerminator; NEL is excluded and BOM is included.
    matches!(
        character,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

impl ColorCheck {
    pub fn target(&self) -> &ColorTarget {
        match self {
            Self::Equals(check) => &check.target,
            Self::SameAs(check) => &check.target,
            Self::DifferentFrom(check) => &check.target,
        }
    }

    pub fn reference(&self) -> Option<&ColorTarget> {
        match self {
            Self::Equals(_) => None,
            Self::SameAs(check) => Some(&check.same_as),
            Self::DifferentFrom(check) => Some(&check.different_from),
        }
    }

    pub fn tolerance(&self) -> u8 {
        match self {
            Self::Equals(check) => check.tolerance,
            Self::SameAs(check) => check.tolerance,
            Self::DifferentFrom(check) => check.tolerance,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        self.target().validate()?;
        validate_optional(&self.reference(), |target| target.validate())?;
        if let Self::Equals(check) = self {
            check
                .rgba()
                .ok_or_else(|| ValidationError::new("color.equals must be #RRGGBB or #RRGGBBAA"))?;
        }
        Ok(())
    }
}

impl ExactColor {
    pub fn rgba(&self) -> Option<[u8; 4]> {
        let hex = self.equals.strip_prefix('#')?;
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut rgba = [255; 4];
        for (channel, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            rgba[channel] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(rgba)
    }
}

impl ColorTarget {
    pub fn dialog(&self) -> Option<&str> {
        match self {
            Self::Text(target) => target.dialog.as_deref(),
            Self::Name(target) => target.dialog.as_deref(),
        }
    }

    pub fn container(&self) -> Option<&str> {
        match self {
            Self::Text(target) => target.container.as_deref(),
            Self::Name(target) => target.container.as_deref(),
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        self.validate_subject()?;
        validate_optional(&self.dialog(), |name| {
            validate_nonempty("color dialog", name)
        })?;
        validate_optional(&self.container(), |name| {
            validate_nonempty("color container", name)
        })?;
        if let Self::Name(target) = self {
            validate_optional(&target.role, |role| validate_nonempty("color role", role))?;
        }
        Ok(())
    }

    fn validate_subject(&self) -> Result<(), ValidationError> {
        match self {
            Self::Text(target) if target.normalized_text().is_empty() => {
                Err(ValidationError::new("color target must not be empty"))
            }
            Self::Text(_) => Ok(()),
            Self::Name(target) => validate_nonempty("color target", &target.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorCheckEvidence {
    pub target: ColorTargetEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<ColorTargetEvidence>,
    pub comparator: ColorComparator,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    pub tolerance: u8,
    pub result: VerdictResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ColorFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorTargetEvidence {
    pub selector: ColorTarget,
    pub channel: Option<ColorChannel>,
    pub raw: Option<String>,
    pub rgba: Option<[u8; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ColorFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorComparator {
    Equals,
    SameAs,
    DifferentFrom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorChannel {
    Accessible,
    PaintedAriaHidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorFailure {
    Missing,
    AmbiguousOwner,
    AmbiguousScope,
    IncompleteCoverage,
    UnsupportedColor,
    Transparent,
}
