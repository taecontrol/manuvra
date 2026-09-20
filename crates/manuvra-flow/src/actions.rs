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
    pub replay_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStop {
    EvidenceUnavailable,
    Uncertain,
    ReadbackMismatch,
    IncompleteEvidence,
    InvalidPermit,
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
    let prepared = prepare(permit, observation, values)?;
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
}

fn prepare(
    permit: Permit,
    observation: &Observation,
    values: &Values<'_>,
) -> Result<PreparedAction, ActionStop> {
    let (candidate, document_id, replay_key, action_sequence) = permit.consume();
    let target = candidate
        .target_index
        .and_then(|index| {
            observation
                .elements
                .iter()
                .find(|element| element.index == index)
        })
        .ok_or(ActionStop::InvalidPermit)?;
    let text = candidate
        .value_name
        .as_deref()
        .map(|name| values.resolve(name).ok_or(ActionStop::InvalidPermit))
        .transpose()?;
    let evidence = json!({"event":"action_prepared","action_sequence":action_sequence,"candidate_id":candidate.id,"operation":candidate.operation,"target":{"role":target.role,"name":target.name,"dialog":target.in_dialog},"value_name":candidate.value_name,"replay_key":replay_key,"outcome":"not_performed"});
    let input = PreparedInput {
        document_id,
        node_id: target.node_id,
        operation: match candidate.operation {
            Operation::Click => PreparedOperation::Click,
            Operation::TypeText => PreparedOperation::TypeText,
            _ => return Err(ActionStop::InvalidPermit),
        },
        text: text.map(str::to_owned),
        action_sequence,
    };
    Ok(PreparedAction {
        input,
        candidate,
        replay_key,
        expected_text: text.map(str::to_owned),
        evidence,
    })
}

fn action_fact(
    prepared: PreparedAction,
    dispatched: Result<PerformFact, PerformError>,
) -> (ActionFact, Option<ActionStop>) {
    let (outcome, readback_matches, stop) = match dispatched {
        Ok(fact) if prepared.candidate.operation == Operation::TypeText => {
            let matches = fact.readback == prepared.expected_text;
            (
                Outcome::Observed,
                Some(matches),
                (!matches).then_some(ActionStop::ReadbackMismatch),
            )
        }
        Ok(_) => (Outcome::Observed, None, None),
        Err(
            PerformError::NotPerformed(_) | PerformError::Rejected(_) | PerformError::Uncertain(_),
        ) => (Outcome::Uncertain, None, Some(ActionStop::Uncertain)),
    };
    let fact = ActionFact {
        candidate_id: prepared.candidate.id,
        operation: prepared.candidate.operation,
        target_name: prepared.candidate.target_name,
        value_name: prepared.candidate.value_name,
        outcome,
        readback_matches,
        replay_key: prepared.replay_key,
    };
    (fact, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::{ChoiceJudgment, Judgments};
    use crate::policy::{Next, Policy};
    use manuvra_chrome::{Coverage, Element, Rect, ViewportState};
    use manuvra_contract::{DoneCondition, Job, Step};
    use std::collections::BTreeMap;
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
                Ok(PerformFact { readback: None })
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
        let c = |s: &str| ChoiceJudgment {
            choice: s.into(),
            probabilities: BTreeMap::from([(s.into(), 1.)]),
            confidence: 1.,
        };
        let j = Judgments {
            operation: c("TYPE_TEXT"),
            click_target: c("NO_CLICK_TARGET"),
            type_target: c("1"),
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
            Next::Mutate(p) => p,
            _ => panic!(),
        }
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
        let mut after = FakeJournal {
            entries: vec![],
            fail_at: Some(1),
        };
        assert_eq!(
            perform(
                permit(&job, &obs),
                &FakePerformer(Ok(PerformFact {
                    readback: Some("Wanted".into())
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
