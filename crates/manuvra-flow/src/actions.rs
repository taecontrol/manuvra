use crate::judgment::Operation;
use crate::policy::Permit;
use crate::values::Values;
use manuvra_chrome::{
    InputCancellation, Observation, PerformError, PerformFact, PreparedInput, PreparedOperation,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Observed,
    NotPerformed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionFact {
    pub candidate_id: String,
    pub operation: Operation,
    pub target_name: Option<String>,
    pub value_name: Option<String>,
    pub outcome: Outcome,
    pub readback_matches: Option<bool>,
    pub suboperations: Vec<String>,
    pub replay_key: String,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStop {
    EvidenceUnavailable,
    Uncertain,
    ReadbackMismatch,
    IncompleteEvidence,
    InvalidPermit,
    Reobserve(String),
}

pub trait Performer {
    fn dispatch(
        &self,
        input: PreparedInput,
        cancellation: &InputCancellation,
    ) -> Result<PerformFact, PerformError>;
}

impl Performer for manuvra_chrome::OwnedBrowser {
    fn dispatch(
        &self,
        input: PreparedInput,
        cancellation: &InputCancellation,
    ) -> Result<PerformFact, PerformError> {
        self.perform(input, cancellation)
    }
}

pub trait ActionJournal {
    fn append(&mut self, value: &Value) -> Result<(), String>;
    fn entries(&self) -> &[Value];
}

pub struct DurableJournal {
    path: PathBuf,
    file: File,
    entries: Vec<Value>,
    redactor: crate::evidence::Redactor,
}

impl DurableJournal {
    pub fn open(
        root: &Path,
        run_id: &str,
        redactor: &crate::evidence::Redactor,
    ) -> Result<Self, String> {
        fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let path = root.join(format!(".{run_id}.action-journal.jsonl"));
        let file = private_file(&path)?;
        Ok(Self {
            path,
            file,
            entries: Vec::new(),
            redactor: redactor.clone(),
        })
    }

    pub fn entries(&self) -> &[Value] {
        &self.entries
    }

    pub fn remove(self) -> Result<(), String> {
        let parent = self.path.parent().map(Path::to_path_buf);
        drop(self.file);
        fs::remove_file(self.path).map_err(|error| error.to_string())?;
        sync_parent(parent.as_deref())
    }

    pub fn clear(&mut self) -> Result<(), String> {
        self.file.sync_all().map_err(|error| error.to_string())?;
        fs::remove_file(&self.path).map_err(|error| error.to_string())?;
        sync_parent(self.path.parent())
    }
}

fn sync_parent(parent: Option<&Path>) -> Result<(), String> {
    parent
        .ok_or_else(|| "action journal path has no parent".to_owned())
        .and_then(|path| File::open(path).map_err(|error| error.to_string()))?
        .sync_all()
        .map_err(|error| error.to_string())
}

impl ActionJournal for DurableJournal {
    fn append(&mut self, value: &Value) -> Result<(), String> {
        let serialized = serde_json::to_string(value).map_err(|error| error.to_string())?;
        let redacted = self.redactor.redact_export_text(&serialized);
        let persisted: Value =
            serde_json::from_str(&redacted).map_err(|error| error.to_string())?;
        let mut bytes = serde_json::to_vec(&persisted).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        self.file.sync_data().map_err(|error| error.to_string())?;
        self.entries.push(persisted);
        Ok(())
    }

    fn entries(&self) -> &[Value] {
        &self.entries
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())
}

pub fn perform(
    permit: Permit,
    browser: &(impl Performer + ?Sized),
    observation: &Observation,
    values: &Values<'_>,
    journal: &mut (impl ActionJournal + ?Sized),
    cancellation: &InputCancellation,
) -> Result<ActionFact, ActionStop> {
    perform_with_basis(
        permit,
        browser,
        observation,
        values,
        journal,
        cancellation,
        "autonomous",
    )
}

pub fn perform_with_basis(
    permit: Permit,
    browser: &(impl Performer + ?Sized),
    observation: &Observation,
    values: &Values<'_>,
    journal: &mut (impl ActionJournal + ?Sized),
    cancellation: &InputCancellation,
    basis: &str,
) -> Result<ActionFact, ActionStop> {
    let prepared = prepare(permit, observation, values, basis)?;
    journal
        .append(&prepared.evidence)
        .map_err(|_| ActionStop::EvidenceUnavailable)?;
    let dispatched = browser.dispatch(prepared.input.clone(), cancellation);
    let (fact, stop) = action_fact(prepared, dispatched);
    journal
        .append(&json!({"event":"action_fact","fact":fact}))
        .map_err(|_| ActionStop::IncompleteEvidence)?;
    stop.map_or(Ok(fact), Err)
}

struct PreparedAction {
    input: PreparedInput,
    candidate: crate::policy::Candidate,
    replay_key: String,
    expected_text: Option<String>,
    evidence: Value,
    basis: String,
}

fn prepare(
    permit: Permit,
    observation: &Observation,
    values: &Values<'_>,
    basis: &str,
) -> Result<PreparedAction, ActionStop> {
    let (candidate, document_id, replay_key, action_sequence) = permit.consume();
    let target = prepared_target(&candidate, observation)?;
    let text = resolved_text(&candidate, values)?;
    let option_node_id = selected_option_identity(&candidate, target, text)?;
    let evidence = json!({"event":"action_prepared","action_sequence":action_sequence,"candidate_id":candidate.id,"operation":candidate.operation,"target":target.map(|target|json!({"role":target.role,"name":target.name,"dialog":target.in_dialog})),"value_name":candidate.value_name,"replay_key":replay_key,"outcome":"not_performed","basis":basis});
    let operation = prepared_operation(candidate.operation, target)?;
    let input = PreparedInput {
        document_id,
        node_id: target.map_or(0, |target| target.node_id),
        operation,
        text: text.map(str::to_owned),
        previous_text: target.map(|target| target.value.clone()),
        option_node_id,
        combobox: target.is_some_and(|target| {
            target.role == "combobox" && candidate.operation == Operation::TypeText
        }),
        action_sequence,
    };
    Ok(PreparedAction {
        input,
        candidate,
        replay_key,
        expected_text: text.map(str::to_owned),
        evidence,
        basis: basis.to_owned(),
    })
}

fn prepared_target<'a>(
    candidate: &crate::policy::Candidate,
    observation: &'a Observation,
) -> Result<Option<&'a manuvra_chrome::Element>, ActionStop> {
    let target = candidate.target_index.and_then(|index| {
        observation
            .elements
            .iter()
            .find(|element| element.index == index)
    });
    let target_optional = matches!(
        candidate.operation,
        Operation::ScrollUp | Operation::ScrollDown
    );
    (target.is_some() || target_optional)
        .then_some(target)
        .ok_or(ActionStop::InvalidPermit)
}

fn resolved_text<'a>(
    candidate: &crate::policy::Candidate,
    values: &'a Values<'_>,
) -> Result<Option<&'a str>, ActionStop> {
    candidate
        .value_name
        .as_deref()
        .map(|name| values.resolve(name).ok_or(ActionStop::InvalidPermit))
        .transpose()
}

fn selected_option_identity(
    candidate: &crate::policy::Candidate,
    target: Option<&manuvra_chrome::Element>,
    text: Option<&str>,
) -> Result<Option<u64>, ActionStop> {
    if candidate.operation != Operation::Select {
        return Ok(None);
    }
    let expected = text.ok_or(ActionStop::InvalidPermit)?;
    target
        .and_then(|target| {
            target.select_options.iter().find(|option| {
                !option.disabled && (option.value == expected || option.label == expected)
            })
        })
        .map(|option| Some(option.node_id))
        .ok_or(ActionStop::InvalidPermit)
}

fn prepared_operation(
    operation: Operation,
    target: Option<&manuvra_chrome::Element>,
) -> Result<PreparedOperation, ActionStop> {
    match operation {
        Operation::Click => Ok(PreparedOperation::Click),
        Operation::TypeText => Ok(text_operation(target)),
        Operation::Select => Ok(PreparedOperation::Select),
        other => prepared_fallback(other),
    }
}

fn prepared_fallback(operation: Operation) -> Result<PreparedOperation, ActionStop> {
    match operation {
        Operation::ScrollUp => Ok(PreparedOperation::ScrollUp),
        Operation::ScrollDown => Ok(PreparedOperation::ScrollDown),
        Operation::Wait | Operation::Blocked => Err(ActionStop::InvalidPermit),
        Operation::Click | Operation::TypeText | Operation::Select => {
            unreachable!("input operation")
        }
    }
}

fn text_operation(target: Option<&manuvra_chrome::Element>) -> PreparedOperation {
    if target
        .and_then(|target| target.input_type.as_deref())
        .is_some_and(|kind| matches!(kind, "date" | "time" | "color" | "range"))
    {
        PreparedOperation::SetValue
    } else {
        PreparedOperation::TypeText
    }
}

fn action_fact(
    prepared: PreparedAction,
    dispatched: Result<PerformFact, PerformError>,
) -> (ActionFact, Option<ActionStop>) {
    let (outcome, readback_matches, stop, suboperations) = match dispatched {
        Ok(fact)
            if matches!(
                prepared.candidate.operation,
                Operation::TypeText | Operation::Select
            ) =>
        {
            let matches = fact
                .readback_matches
                .unwrap_or(fact.readback == prepared.expected_text);
            (
                Outcome::Observed,
                Some(matches),
                (!matches).then_some(ActionStop::ReadbackMismatch),
                fact.suboperations,
            )
        }
        Ok(fact) => (Outcome::Observed, None, None, fact.suboperations),
        Err(PerformError::Rejected(_)) => (
            Outcome::NotPerformed,
            None,
            Some(ActionStop::Reobserve(prepared.replay_key.clone())),
            Vec::new(),
        ),
        Err(PerformError::NotPerformed(_) | PerformError::Uncertain(_)) => (
            Outcome::Uncertain,
            None,
            Some(ActionStop::Uncertain),
            Vec::new(),
        ),
    };
    let fact = ActionFact {
        candidate_id: prepared.candidate.id,
        operation: prepared.candidate.operation,
        target_name: prepared.candidate.target_name,
        value_name: prepared.candidate.value_name,
        outcome,
        readback_matches,
        suboperations,
        replay_key: prepared.replay_key,
        basis: prepared.basis,
    };
    (fact, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::{ChoiceJudgment, Judgments};
    use crate::policy::{Next, Policy};
    use manuvra_chrome::{Coverage, Element, Rect, SelectOption, ViewportState};
    use manuvra_contract::{DoneCondition, Job, Step};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct FakePerformer(Result<PerformFact, PerformError>);
    impl Performer for FakePerformer {
        fn dispatch(
            &self,
            _: PreparedInput,
            _: &InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            self.0.clone()
        }
    }
    struct CapturingPerformer(Mutex<Option<PreparedInput>>);
    impl Performer for CapturingPerformer {
        fn dispatch(
            &self,
            input: PreparedInput,
            _: &InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            *self.0.lock().unwrap() = Some(input);
            Ok(PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: Some(true),
                suboperations: vec![],
            })
        }
    }
    struct CountingPerformer<'a>(&'a AtomicUsize);
    impl Performer for CountingPerformer<'_> {
        fn dispatch(
            &self,
            _: PreparedInput,
            _: &InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: None,
                suboperations: vec![],
            })
        }
    }
    struct CancellationBoundaryPerformer<'a>(&'a AtomicUsize);
    impl Performer for CancellationBoundaryPerformer<'_> {
        fn dispatch(
            &self,
            _: PreparedInput,
            cancellation: &InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            if cancellation.is_cancelled() {
                Err(PerformError::NotPerformed(
                    "cancelled before transport send".into(),
                ))
            } else {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(PerformFact {
                    readback: None,
                    readback_matches: None,
                    suboperations: vec![],
                })
            }
        }
    }
    #[derive(Default)]
    struct FakeJournal {
        entries: Vec<Value>,
        fail_at: Option<usize>,
    }
    impl ActionJournal for FakeJournal {
        fn append(&mut self, value: &Value) -> Result<(), String> {
            if self.fail_at == Some(self.entries.len()) {
                return Err("injected".into());
            }
            self.entries.push(value.clone());
            Ok(())
        }

        fn entries(&self) -> &[Value] {
            &self.entries
        }
    }
    fn job() -> Job {
        Job::parse(serde_json::to_vec(&json!({"schema_version":1,"target":{"kind":"browser","url":"http://example.test"},"context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},"values":{"name":{"value":"Wanted","description":"name"}},"steps":[{"id":"fill","goal":"fill","done_when":[{"field":"Name","equals_value":"name"}],"mutation_limit":2}]})).unwrap().as_slice()).unwrap()
    }
    fn observation() -> Observation {
        Observation {
            document_id: "d".into(),
            url: "http://example.test/".into(),
            route: "/".into(),
            title: "x".into(),
            dialogs: vec![],
            focused: None,
            visible_text: "".into(),
            covered_text: "".into(),
            dialog_texts: BTreeMap::new(),
            elements: vec![Element {
                index: 1,
                node_id: 1,
                context: "main".into(),
                role: "textbox".into(),
                name: "Name".into(),
                input_type: Some("text".into()),
                value: "".into(),
                checked: None,
                selected: None,
                expanded: None,
                disabled: false,
                in_dialog: None,
                operations: vec!["TYPE_TEXT".into()],
                select_options: vec![],
                rect: Rect {
                    x: 0.,
                    y: 0.,
                    width: 1.,
                    height: 1.,
                },
            }],
            viewport: ViewportState {
                width: 10,
                height: 10,
                scroll_x: 0.,
                scroll_y: 0.,
                document_height: 10.,
            },
            coverage: Coverage::default(),
        }
    }
    fn permit(job: &Job, obs: &Observation) -> Permit {
        permit_for(job, obs, "TYPE_TEXT")
    }
    fn permit_for(job: &Job, obs: &Observation, operation: &str) -> Permit {
        let c = |s: &str| ChoiceJudgment {
            choice: s.into(),
            probabilities: BTreeMap::from([(s.into(), 1.)]),
            confidence: 1.,
        };
        let j = Judgments {
            operation: c(operation),
            click_target: c("NO_CLICK_TARGET"),
            type_target: c("1"),
            select_target: c("1"),
            type_value: c("name"),
            step_done: 0.,
            usage: BTreeMap::new(),
            request_id: None,
            model: "jev".into(),
            request: Value::Null,
        };
        let mut p = Policy::new(&job.options, "http://example.test");
        match p.decide(
            &job.steps[0],
            obs,
            &j,
            crate::verification::DoneResult::NotSatisfied,
            false,
            false,
        ) {
            Next::Mutate(p) => *p,
            _ => panic!(),
        }
    }

    #[test]
    fn prepare_selects_native_strategies_from_observed_input_semantics() {
        let job = job();
        let mut select = observation();
        select.elements[0].role = "combobox".into();
        select.elements[0].input_type = None;
        select.elements[0].operations = vec!["SELECT".into()];
        select.elements[0].select_options = vec![SelectOption {
            node_id: 2,
            label: "Wanted".into(),
            value: "wanted-id".into(),
            disabled: false,
            selected: false,
        }];
        let capture = CapturingPerformer(Mutex::new(None));
        perform(
            permit_for(&job, &select, "SELECT"),
            &capture,
            &select,
            &Values::new(&job),
            &mut FakeJournal::default(),
            &InputCancellation::default(),
        )
        .unwrap();
        let prepared = capture.0.lock().unwrap().clone().unwrap();
        assert_eq!(prepared.operation, PreparedOperation::Select);
        assert_eq!(prepared.option_node_id, Some(2));

        for input_type in ["date", "time", "color", "range"] {
            let mut specialized = observation();
            specialized.elements[0].input_type = Some(input_type.into());
            let capture = CapturingPerformer(Mutex::new(None));
            perform(
                permit(&job, &specialized),
                &capture,
                &specialized,
                &Values::new(&job),
                &mut FakeJournal::default(),
                &InputCancellation::default(),
            )
            .unwrap();
            assert_eq!(
                capture.0.lock().unwrap().as_ref().unwrap().operation,
                PreparedOperation::SetValue,
                "{input_type}"
            );
        }
    }

    #[test]
    fn fallback_operation_mapping_is_closed() {
        assert_eq!(
            prepared_fallback(Operation::ScrollUp),
            Ok(PreparedOperation::ScrollUp)
        );
        assert_eq!(
            prepared_fallback(Operation::ScrollDown),
            Ok(PreparedOperation::ScrollDown)
        );
        assert_eq!(
            prepared_fallback(Operation::Wait),
            Err(ActionStop::InvalidPermit)
        );
        assert_eq!(
            prepared_fallback(Operation::Blocked),
            Err(ActionStop::InvalidPermit)
        );
    }

    #[test]
    #[should_panic(expected = "input operation")]
    fn fallback_operation_mapping_rejects_internal_misrouting() {
        let _ = prepared_fallback(Operation::Click);
    }

    #[test]
    fn flush_precedes_dispatch_and_readback_mismatch_is_failed() {
        let job = job();
        let obs = observation();
        let mut journal = FakeJournal::default();
        let result = perform(
            permit(&job, &obs),
            &FakePerformer(Ok(PerformFact {
                readback: Some("Wrong".into()),
                readback_matches: None,
                suboperations: vec![],
            })),
            &obs,
            &Values::new(&job),
            &mut journal,
            &InputCancellation::default(),
        );
        assert_eq!(result, Err(ActionStop::ReadbackMismatch));
        assert_eq!(journal.entries[0]["event"], "action_prepared");
        assert_eq!(journal.entries[1]["event"], "action_fact");
    }

    #[test]
    fn fault_boundaries_preserve_no_dispatch_and_truthful_uncertainty() {
        let job = job();
        let obs = observation();
        let mut before = FakeJournal {
            entries: vec![],
            fail_at: Some(0),
        };
        let dispatches = AtomicUsize::new(0);
        assert_eq!(
            perform(
                permit(&job, &obs),
                &CountingPerformer(&dispatches),
                &obs,
                &Values::new(&job),
                &mut before,
                &InputCancellation::default(),
            ),
            Err(ActionStop::EvidenceUnavailable)
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        let cancellation = InputCancellation::default();
        cancellation.cancel();
        let mut after_flush_before_send = FakeJournal::default();
        assert_eq!(
            perform(
                permit(&job, &obs),
                &CancellationBoundaryPerformer(&dispatches),
                &obs,
                &Values::new(&job),
                &mut after_flush_before_send,
                &cancellation,
            ),
            Err(ActionStop::Uncertain)
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert_eq!(
            after_flush_before_send.entries[0]["event"],
            "action_prepared"
        );
        for failure in [
            PerformError::NotPerformed("before send".into()),
            PerformError::Uncertain("after send".into()),
            PerformError::Uncertain("cancelled between suboperations".into()),
            PerformError::Uncertain("readback unavailable".into()),
        ] {
            let mut journal = FakeJournal::default();
            assert_eq!(
                perform(
                    permit(&job, &obs),
                    &FakePerformer(Err(failure)),
                    &obs,
                    &Values::new(&job),
                    &mut journal,
                    &InputCancellation::default(),
                ),
                Err(ActionStop::Uncertain)
            );
            assert_eq!(journal.entries[0]["event"], "action_prepared");
        }
        let mut remounted = FakeJournal::default();
        let result = perform(
            permit(&job, &obs),
            &FakePerformer(Err(PerformError::Rejected("target_missing".into()))),
            &obs,
            &Values::new(&job),
            &mut remounted,
            &InputCancellation::default(),
        );
        assert!(matches!(result, Err(ActionStop::Reobserve(_))));
        assert_eq!(remounted.entries[1]["fact"]["outcome"], "not_performed");
        let mut after = FakeJournal {
            entries: vec![],
            fail_at: Some(1),
        };
        assert_eq!(
            perform(
                permit(&job, &obs),
                &FakePerformer(Ok(PerformFact {
                    readback: Some("Wanted".into()),
                    readback_matches: Some(true),
                    suboperations: vec!["insert_text".into()],
                })),
                &obs,
                &Values::new(&job),
                &mut after,
                &InputCancellation::default(),
            ),
            Err(ActionStop::IncompleteEvidence)
        );
    }

    #[test]
    fn permit_type_is_not_constructible_outside_policy_module() {
        let _ = Step {
            id: "x".into(),
            goal: "x".into(),
            done_when: DoneCondition::Structured(vec![]),
            requires_values: vec![],
            mutation_limit: 1,
        };
    }

    #[test]
    fn durable_journal_flushes_redacted_json_and_removes_itself() {
        let mut job = job();
        job.values.get_mut("name").unwrap().secret = true;
        let root = TempDir::new().unwrap();
        let path = root.path().join(".journal-test.action-journal.jsonl");
        let redactor = crate::evidence::Redactor::for_job(&job).unwrap();
        let mut journal = DurableJournal::open(root.path(), "journal-test", &redactor).unwrap();
        journal
            .append(&json!({"event":"action_prepared","value":"Wanted"}))
            .unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("action_prepared"));
        assert!(!persisted.contains("Wanted"));
        assert_eq!(journal.entries().len(), 1);
        journal.remove().unwrap();
        assert!(!path.exists());

        let clear_path = root.path().join(".clear-test.action-journal.jsonl");
        let mut journal = DurableJournal::open(root.path(), "clear-test", &redactor).unwrap();
        journal.append(&json!({"event":"clear_test"})).unwrap();
        journal.clear().unwrap();
        assert!(!clear_path.exists());
    }
}
