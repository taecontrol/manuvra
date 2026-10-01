//! Candidates: the operation and target a permit would authorize, built from the provider's
//! judgments on an observation, and the redacted form a caller is offered.

use super::targeting::{dispatched_operation, selected_reveal, selected_target, selected_value};
use super::{Policy, PolicyStop};
use crate::judgment::{Judgments, Operation};
use manuvra_chrome::{FocusAnchor, HoverRegion, Key, Observation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: String,
    pub(crate) operation: Operation,
    pub(crate) target_index: Option<u64>,
    pub(crate) target_name: Option<String>,
    pub(super) target_identity: TargetIdentity,
    pub(crate) target_role: Option<String>,
    pub(crate) target_dialog: Option<String>,
    pub(crate) target_container: Option<String>,
    pub(crate) target_input_type: Option<String>,
    pub(crate) value_name: Option<String>,
    pub(crate) key: Option<Key>,
    pub(crate) focus_anchor: Option<FocusAnchor>,
    pub(crate) hover_target: Option<HoverTarget>,
}

/// The hover region a `HOVER` candidate targets, as recorded in evidence. Its dispatch identity,
/// the chosen hidden control, stays in the candidate's target identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverTarget {
    pub index: u64,
    pub name: String,
    pub reveals_on_hover: Vec<String>,
    pub reveal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TargetIdentity {
    pub(super) node_id: Option<u64>,
    pub(super) document_id: String,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Serialize)]
struct OfferedCandidate<'a> {
    id: &'a str,
    operation: Operation,
    target_name: Option<&'a str>,
    target_role: Option<&'a str>,
    target_dialog: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_container: Option<&'a str>,
    target_input_type: Option<&'a str>,
    value_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<Key>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_anchor: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hover_target: Option<OfferedHoverTarget<'a>>,
}

/// The hover region a caller is offered: its name and the controls it reveals, never its index
/// or dispatch identity.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Serialize)]
struct OfferedHoverTarget<'a> {
    name: &'a str,
    reveals_on_hover: &'a [String],
    reveal: &'a str,
}

impl Candidate {
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn offered(&self) -> serde_json::Value {
        serde_json::to_value(OfferedCandidate {
            id: &self.id,
            operation: self.operation,
            target_name: self.target_name.as_deref(),
            target_role: self.target_role.as_deref(),
            target_dialog: self.target_dialog.as_deref(),
            target_container: self.target_container.as_deref(),
            target_input_type: self.target_input_type.as_deref(),
            value_name: self.value_name.as_deref(),
            key: self.key,
            focus_anchor: (self.operation == Operation::PressKey).then(|| {
                self.focus_anchor.as_ref().map_or(serde_json::Value::Null, |anchor| {
                    serde_json::json!({"role":anchor.role,"name":anchor.name,"dialog":anchor.in_dialog})
                })
            }),
            hover_target: self.hover_target.as_ref().map(|region| OfferedHoverTarget {
                name: &region.name,
                reveals_on_hover: &region.reveals_on_hover,
                reveal: &region.reveal,
            }),
        })
        .expect("offered candidate is serializable")
    }

    /// The observed hover region this candidate targets, when it is still the same region: same
    /// document, same hidden control, same name and reveals.
    pub(crate) fn hover_region<'a>(&self, observation: &'a Observation) -> Option<&'a HoverRegion> {
        let wanted = self.hover_target.as_ref()?;
        (observation.document_id == self.target_identity.document_id)
            .then_some(&observation.hover_regions)?
            .iter()
            .find(|region| {
                region.index == wanted.index
                    && region.reveal_node_ids.first() == Some(&region.node_id)
                    && region
                        .reveal_node_ids
                        .iter()
                        .zip(&region.reveals_on_hover)
                        .any(|(node, name)| {
                            Some(*node) == self.target_identity.node_id && *name == wanted.reveal
                        })
                    && region.name == wanted.name
                    && region.reveals_on_hover == wanted.reveals_on_hover
            })
    }
    /// The chosen hidden node, only while its complete region identity still matches.
    pub(crate) fn hover_node_id(&self, observation: &Observation) -> Option<u64> {
        self.hover_region(observation)?;
        self.target_identity.node_id
    }
}

impl Policy {
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn caller_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let operation = dispatched_operation(observation, judgments)?;
        match operation {
            Operation::Click
            | Operation::TypeText
            | Operation::Select
            | Operation::Hover
            | Operation::PressKey => self.candidate(observation, judgments, operation),
            _ => Err(PolicyStop::Blocked("provider_invalid_response")),
        }
    }

    pub(super) fn candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        match operation {
            Operation::ScrollUp | Operation::ScrollDown => {
                self.scroll_candidate(observation, operation)
            }
            Operation::Hover => self.hover_candidate(observation, judgments),
            Operation::PressKey => self.key_candidate(observation, judgments),
            _ => self.element_candidate(observation, judgments, operation),
        }
    }

    fn element_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        let target = selected_target(observation, judgments, operation)?;
        let value_name = selected_value(judgments, operation)?;
        Ok(Candidate {
            id: format!("c_{}", self.actions + 1),
            operation,
            target_index: Some(target.index),
            target_name: Some(target.name.clone()),
            target_identity: TargetIdentity {
                node_id: Some(target.node_id),
                document_id: observation.document_id.clone(),
            },
            target_role: Some(target.role.clone()),
            target_dialog: target.in_dialog.clone(),
            target_container: target.container.clone(),
            target_input_type: target.input_type.clone(),
            value_name,
            key: None,
            focus_anchor: None,
            hover_target: None,
        })
    }

    fn key_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let key = Key::from_choice(&judgments.key.choice)
            .ok_or(PolicyStop::Blocked("provider_invalid_response"))?;
        let anchor = observation.focus_anchor.clone();
        Ok(Candidate {
            id: format!("c_{}", self.actions + 1),
            operation: Operation::PressKey,
            target_index: None,
            target_name: anchor.as_ref().map(|anchor| anchor.name.clone()),
            target_identity: TargetIdentity {
                node_id: anchor.as_ref().map(|anchor| anchor.node_id),
                document_id: observation.document_id.clone(),
            },
            target_role: anchor.as_ref().map(|anchor| anchor.role.clone()),
            target_dialog: anchor.as_ref().and_then(|anchor| anchor.in_dialog.clone()),
            target_container: anchor.as_ref().and_then(|anchor| anchor.container.clone()),
            target_input_type: None,
            value_name: None,
            key: Some(key),
            focus_anchor: anchor,
            hover_target: None,
        })
    }

    fn scroll_candidate(
        &self,
        observation: &Observation,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        let can_scroll = match operation {
            Operation::ScrollUp => observation.viewport.scroll_y > 0.0,
            Operation::ScrollDown => {
                observation.viewport.scroll_y + f64::from(observation.viewport.height)
                    < observation.viewport.document_height
            }
            _ => false,
        };
        can_scroll
            .then(|| self.untargeted_candidate(observation, operation))
            .ok_or(PolicyStop::Blocked("operation_blocked"))
    }

    fn hover_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let (region, offset) = selected_reveal(observation, judgments)
            .ok_or(PolicyStop::Blocked("provider_invalid_response"))?;
        let mut candidate = self.untargeted_candidate(observation, Operation::Hover);
        candidate.target_identity.node_id = Some(region.reveal_node_ids[offset]);
        candidate.hover_target = Some(HoverTarget {
            index: region.index,
            name: region.name.clone(),
            reveals_on_hover: region.reveals_on_hover.clone(),
            reveal: region.reveals_on_hover[offset].clone(),
        });
        Ok(candidate)
    }

    fn untargeted_candidate(&self, observation: &Observation, operation: Operation) -> Candidate {
        Candidate {
            id: format!("c_{}", self.actions + 1),
            operation,
            target_index: None,
            target_name: None,
            target_identity: TargetIdentity {
                node_id: None,
                document_id: observation.document_id.clone(),
            },
            target_role: None,
            target_dialog: None,
            target_container: None,
            target_input_type: None,
            value_name: None,
            key: None,
            focus_anchor: None,
            hover_target: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::Operation;
    use crate::policy::tests::support::*;
    use crate::test_support::hover_region as region;
    use manuvra_contract::JobOptions;

    #[test]
    fn hover_on_a_listed_region_mints_a_fallback_permit_naming_the_region() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let permit = minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R2_1")),
            false,
        ));
        assert_eq!(permit.operation(), Operation::Hover);
        let (candidate, document_id, _, sequence) = permit.consume();
        assert_eq!(document_id, "d");
        assert_eq!(sequence, 1);
        assert_eq!(candidate.target_index, None);
        assert_eq!(candidate.target_name, None);
        assert_eq!(
            candidate.hover_target,
            Some(HoverTarget {
                index: 2,
                name: "Rent".into(),
                reveals_on_hover: vec!["Actions for Rent".into()],
                reveal: "Actions for Rent".into(),
            })
        );
        assert_eq!(
            candidate.hover_region(&hover_page()),
            Some(&region(2, "Rent", 42))
        );
        assert_eq!(policy.actions, 1);
        assert_eq!(policy.fallbacks, 1);
        assert_eq!(policy.step_mutations(), 0);
    }

    #[test]
    fn a_hover_below_the_gate_is_offered_by_region_name_and_reveals_only() {
        let policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy
            .caller_candidate(&hover_page(), &hover(Some("R2_1")))
            .unwrap();
        assert_eq!(candidate.operation, Operation::Hover);
        assert_eq!(
            candidate.hover_region(&hover_page()),
            Some(&region(2, "Rent", 42))
        );
        assert_eq!(
            candidate.offered(),
            serde_json::json!({
                "id":"c_1","operation":"HOVER","target_name":null,"target_role":null,
                "target_dialog":null,"target_input_type":null,"value_name":null,
                "hover_target":{"name":"Rent","reveals_on_hover":["Actions for Rent"],"reveal":"Actions for Rent"}
            })
        );
        for unlisted in [Some("NOT_LISTED"), Some("R9")] {
            assert_eq!(
                policy.caller_candidate(&hover_page(), &hover(unlisted)),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        for fallback in ["SCROLL_DOWN", "WAIT", "BLOCKED"] {
            assert_eq!(
                policy.caller_candidate(&hover_page(), &judgments(fallback)),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        let element = policy
            .caller_candidate(&hover_page(), &judgments("CLICK"))
            .unwrap()
            .offered();
        assert!(element.get("hover_target").is_none());
    }
}
