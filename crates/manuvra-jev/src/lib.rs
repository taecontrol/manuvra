//! The narrow TypeSafe System One transport owned by Manuvra.

use rand::Rng;
use reqwest::StatusCode;
use reqwest::blocking::{Client as HttpClient, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const REQUEST_MODEL: &str = "jev-latest";
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const LOGICAL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: BTreeMap<String, u64>,
    pub request_id: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum JevError {
    #[error("provider unavailable")]
    Unavailable,
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("provider model changed within the run")]
    ModelChanged,
    #[error("model-call deadline elapsed")]
    Deadline,
}

pub trait Evaluator {
    fn evaluate(&self, request: &Value, deadline: Instant) -> Result<Evaluation, JevError>;
}

pub struct Client {
    http: HttpClient,
    key: String,
    endpoint: String,
    pinned_model: Mutex<Option<String>>,
}

enum Attempt<T> {
    Success(T),
    Retry(Option<Duration>),
    Fail(JevError),
}

impl Client {
    pub fn from_environment() -> Result<Self, JevError> {
        let key = std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or(JevError::Unavailable)?;
        Self::new(key, ENDPOINT)
    }

    pub fn from_key(key: String) -> Result<Self, JevError> {
        Self::new(key, ENDPOINT)
    }

    pub fn new(key: String, endpoint: impl Into<String>) -> Result<Self, JevError> {
        let http = HttpClient::builder()
            .connect_timeout(ATTEMPT_TIMEOUT)
            .build()
            .map_err(|_| JevError::Unavailable)?;
        Ok(Self {
            http,
            key,
            endpoint: endpoint.into(),
            pinned_model: Mutex::new(None),
        })
    }

    fn send(
        &self,
        request: &Value,
        questions: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Evaluation, JevError> {
        let logical = deadline.min(Instant::now() + LOGICAL_TIMEOUT);
        let mut attempt = 0_u32;
        loop {
            let remaining = logical
                .checked_duration_since(Instant::now())
                .ok_or(JevError::Deadline)?;
            match self.evaluation_attempt(request, questions, remaining, attempt < 2) {
                Attempt::Success(evaluation) => return Ok(evaluation),
                Attempt::Retry(retry_after) => {
                    wait_before_retry(attempt, retry_after, logical)?;
                    attempt += 1;
                }
                Attempt::Fail(error) => return Err(error),
            }
        }
    }

    fn evaluation_attempt(
        &self,
        request: &Value,
        questions: &Map<String, Value>,
        remaining: Duration,
        can_retry: bool,
    ) -> Attempt<Evaluation> {
        match self.attempt(request, remaining, can_retry) {
            Attempt::Success(response) => {
                classify_evaluation(self.parse_response(response, questions), can_retry)
            }
            Attempt::Retry(delay) => Attempt::Retry(delay),
            Attempt::Fail(error) => Attempt::Fail(error),
        }
    }

    fn attempt(&self, request: &Value, remaining: Duration, can_retry: bool) -> Attempt<Response> {
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .timeout(remaining.min(ATTEMPT_TIMEOUT))
            .json(request)
            .send();
        match response {
            Ok(response) => classify_response(response, can_retry),
            Err(error) => classify_error(&error, can_retry),
        }
    }

    fn parse_response(
        &self,
        response: Response,
        questions: &Map<String, Value>,
    ) -> Result<Evaluation, JevError> {
        let request_id = provider_request_id(&response)?;
        let body: Value = response
            .json()
            .map_err(|_| JevError::InvalidResponse("response body was not valid JSON".into()))?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| JevError::InvalidResponse("response model is missing".into()))?
            .to_owned();
        let evaluation = parse_evaluation(body, questions, Some(request_id), model.clone())?;
        self.pin(&model)?;
        Ok(evaluation)
    }

    fn pin(&self, model: &str) -> Result<(), JevError> {
        let mut pinned = self
            .pinned_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*pinned {
            Some(existing) if existing != model => Err(JevError::ModelChanged),
            Some(_) => Ok(()),
            None => {
                *pinned = Some(model.to_owned());
                Ok(())
            }
        }
    }

    fn request_for_run(&self, request: &Value) -> Result<Value, JevError> {
        validate_request(request)?;
        let mut request = request.clone();
        let pinned = self
            .pinned_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(model) = pinned {
            request
                .as_object_mut()
                .ok_or_else(|| JevError::InvalidResponse("request was not an object".into()))?
                .insert("model".into(), Value::String(model));
        }
        Ok(request)
    }
}

fn classify_response(response: Response, can_retry: bool) -> Attempt<Response> {
    if response.status().is_success() {
        Attempt::Success(response)
    } else if can_retry && retryable(response.status()) {
        Attempt::Retry(retry_after(&response))
    } else {
        terminal_response(response.status())
    }
}

fn terminal_response(status: StatusCode) -> Attempt<Response> {
    if [StatusCode::UNPROCESSABLE_ENTITY, StatusCode::BAD_REQUEST].contains(&status) {
        Attempt::Fail(JevError::InvalidResponse(
            "provider rejected the request shape".into(),
        ))
    } else {
        Attempt::Fail(JevError::Unavailable)
    }
}

fn classify_error(error: &reqwest::Error, can_retry: bool) -> Attempt<Response> {
    if can_retry && (error.is_timeout() || error.is_connect()) {
        Attempt::Retry(None)
    } else {
        Attempt::Fail(JevError::Unavailable)
    }
}

fn classify_evaluation(
    evaluation: Result<Evaluation, JevError>,
    can_retry: bool,
) -> Attempt<Evaluation> {
    match evaluation {
        Ok(evaluation) => Attempt::Success(evaluation),
        Err(JevError::InvalidResponse(_) | JevError::ModelChanged) if can_retry => {
            Attempt::Retry(None)
        }
        Err(error) => Attempt::Fail(error),
    }
}

impl Evaluator for Client {
    fn evaluate(&self, request: &Value, deadline: Instant) -> Result<Evaluation, JevError> {
        let request = self.request_for_run(request)?;
        let questions = validate_request(&request)?;
        self.send(&request, questions, deadline)
    }
}

fn provider_request_id(response: &Response) -> Result<String, JevError> {
    let value = response
        .headers()
        .get("x-typesafe-request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .ok_or_else(|| JevError::InvalidResponse("provider request id is missing".into()))?
        .to_str()
        .map_err(|_| JevError::InvalidResponse("provider request id is not ASCII".into()))?;
    if value.is_empty() || value.len() > 128 {
        return Err(JevError::InvalidResponse(
            "provider request id must contain 1 to 128 characters".into(),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(JevError::InvalidResponse(
            "provider request id contains unsafe characters".into(),
        ));
    }
    Ok(value.to_owned())
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_after(response: &Response) -> Option<Duration> {
    let milliseconds = response
        .headers()
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis);
    milliseconds.or_else(|| {
        let value = response.headers().get("retry-after")?.to_str().ok()?;
        value
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
            .or_else(|| {
                httpdate::parse_http_date(value)
                    .ok()?
                    .duration_since(SystemTime::now())
                    .ok()
            })
    })
}

fn wait_before_retry(
    attempt: u32,
    retry_after: Option<Duration>,
    deadline: Instant,
) -> Result<(), JevError> {
    let base = Duration::from_millis(500_u64.saturating_mul(1_u64 << attempt).min(5_000));
    let jitter = rand::rng().random_range(0.75..=1.0);
    let delay = retry_after.unwrap_or(base.mul_f64(jitter));
    if Instant::now()
        .checked_add(delay)
        .is_none_or(|at| at >= deadline)
    {
        return Err(JevError::Deadline);
    }
    std::thread::sleep(delay);
    Ok(())
}

fn validate_request(request: &Value) -> Result<&Map<String, Value>, JevError> {
    let model = request.get("model").and_then(Value::as_str);
    if !model.is_some_and(|model| model == REQUEST_MODEL || model.starts_with("jev-")) {
        return Err(JevError::InvalidResponse(
            "request model is not a Jev model".into(),
        ));
    }
    request
        .get("questions")
        .and_then(Value::as_object)
        .filter(|questions| !questions.is_empty())
        .ok_or_else(|| JevError::InvalidResponse("request has no questions".into()))
}

fn parse_evaluation(
    body: Value,
    questions: &Map<String, Value>,
    request_id: Option<String>,
    model: String,
) -> Result<Evaluation, JevError> {
    let raw = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::InvalidResponse("answers object is missing".into()))?;
    if raw.len() != questions.len() || raw.keys().any(|id| !questions.contains_key(id)) {
        return Err(JevError::InvalidResponse(
            "answer ids do not match questions".into(),
        ));
    }
    let mut answers = BTreeMap::new();
    for (id, question) in questions {
        let answer = raw
            .get(id)
            .ok_or_else(|| JevError::InvalidResponse(format!("answer {id} is missing")))?;
        answers.insert(id.clone(), parse_answer(id, question, answer)?);
    }
    let usage = parse_usage(&body)?;
    Ok(Evaluation {
        answers,
        usage,
        request_id,
        model,
    })
}

fn parse_answer(id: &str, question: &Value, answer: &Value) -> Result<Answer, JevError> {
    let question_type = question.get("type").and_then(Value::as_str);
    let answer_type = answer.get("type").and_then(Value::as_str);
    match (question_type, answer_type) {
        (Some("choice"), Some("choice")) => parse_choice(id, question, answer),
        (Some("noul"), Some("noul")) => parse_noul(id, answer),
        (Some("choice" | "noul"), _) => Err(type_mismatch(id)),
        _ => Err(JevError::InvalidResponse(format!(
            "question {id} has an unsupported type"
        ))),
    }
}

fn parse_noul(id: &str, answer: &Value) -> Result<Answer, JevError> {
    finite_probability(answer.get("noul"), id).map(|noul| Answer::Noul { noul })
}

fn type_mismatch(id: &str) -> JevError {
    JevError::InvalidResponse(format!("answer {id} type did not match its question"))
}

fn parse_usage(body: &Value) -> Result<BTreeMap<String, u64>, JevError> {
    let usage = body
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::InvalidResponse("usage object is missing".into()))?;
    usage
        .iter()
        .map(|(key, value)| {
            value
                .as_u64()
                .map(|value| (key.clone(), value))
                .ok_or_else(|| JevError::InvalidResponse(format!("usage {key} was not an integer")))
        })
        .collect()
}

fn parse_choice(id: &str, question: &Value, answer: &Value) -> Result<Answer, JevError> {
    let parts = choice_parts(id, question, answer)?;
    validate_choice_keys(id, parts.choice, parts.criteria, parts.probabilities)?;
    let probabilities = choice_probabilities(id, parts.probabilities)?;
    validate_choice_distribution(id, parts.choice, &probabilities)?;
    let confidence = finite_probability(answer.get("confidence"), id)?;
    Ok(Answer::Choice {
        choice: parts.choice.into(),
        probabilities,
        confidence,
    })
}

struct ChoiceParts<'a> {
    choice: &'a str,
    criteria: &'a Map<String, Value>,
    probabilities: &'a Map<String, Value>,
}

fn choice_parts<'a>(
    id: &str,
    question: &'a Value,
    answer: &'a Value,
) -> Result<ChoiceParts<'a>, JevError> {
    let choice = required_string(answer, "choice", id)?;
    let criteria = required_object(question, "criteria", id)?;
    let probabilities = required_object(answer, "probabilities", id)?;
    Ok(ChoiceParts {
        choice,
        criteria,
        probabilities,
    })
}

fn required_string<'a>(value: &'a Value, key: &str, id: &str) -> Result<&'a str, JevError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| JevError::InvalidResponse(format!("answer {id} has no {key}")))
}

fn required_object<'a>(
    value: &'a Value,
    key: &str,
    id: &str,
) -> Result<&'a Map<String, Value>, JevError> {
    value
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::InvalidResponse(format!("answer {id} has no {key}")))
}

fn validate_choice_keys(
    id: &str,
    choice: &str,
    criteria: &Map<String, Value>,
    raw: &Map<String, Value>,
) -> Result<(), JevError> {
    if raw.len() != criteria.len()
        || raw.keys().any(|key| !criteria.contains_key(key))
        || !criteria.contains_key(choice)
    {
        return Err(JevError::InvalidResponse(format!(
            "answer {id} probability ids do not match criteria"
        )));
    }
    Ok(())
}

fn choice_probabilities(
    id: &str,
    raw: &Map<String, Value>,
) -> Result<BTreeMap<String, f64>, JevError> {
    raw.iter()
        .map(|(key, value)| Ok((key.clone(), finite_probability(Some(value), id)?)))
        .collect()
}

fn validate_choice_distribution(
    id: &str,
    choice: &str,
    probabilities: &BTreeMap<String, f64>,
) -> Result<(), JevError> {
    let sum: f64 = probabilities.values().sum();
    if (sum - 1.0).abs() > 0.02 {
        return Err(JevError::InvalidResponse(format!(
            "answer {id} probabilities do not sum to one"
        )));
    }
    let maximum = probabilities
        .values()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if probabilities.get(choice).copied() != Some(maximum) {
        return Err(JevError::InvalidResponse(format!(
            "answer {id} choice is not an argmax"
        )));
    }
    Ok(())
}

fn finite_probability(value: Option<&Value>, id: &str) -> Result<f64, JevError> {
    let value = value.and_then(Value::as_f64).ok_or_else(|| {
        JevError::InvalidResponse(format!("answer {id} has a nonnumeric probability"))
    })?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(JevError::InvalidResponse(format!(
            "answer {id} probability is outside zero through one"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn request() -> Value {
        json!({"model":"jev-latest","state":{},"questions":{
            "operation":{"type":"choice","instructions":"pick","criteria":{"CLICK":null,"WAIT":null}},
            "step_done":{"type":"noul","instructions":"done"}
        }})
    }

    #[test]
    fn validates_complete_choice_and_noul_answers() {
        let questions = validate_request(&request()).unwrap().clone();
        let evaluation = parse_evaluation(
            json!({"answers":{
                "operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.7,"WAIT":0.3},"confidence":0.8},
                "step_done":{"type":"noul","noul":0.1}},"usage":{"input_tokens":12}}),
            &questions,
            Some("request-1".into()),
            "jev-1.13".into(),
        )
        .unwrap();
        assert_eq!(evaluation.model, "jev-1.13");
        assert_eq!(evaluation.usage["input_tokens"], 12);
    }

    #[test]
    fn rejects_bad_sum_unknown_and_missing_answers() {
        let questions = validate_request(&request()).unwrap().clone();
        for body in [
            json!({"answers":{"operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.4,"WAIT":0.4},"confidence":0.8},"step_done":{"type":"noul","noul":0.1}},"usage":{}}),
            json!({"answers":{"operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.8,"OTHER":0.2},"confidence":0.8},"step_done":{"type":"noul","noul":0.1}},"usage":{}}),
            json!({"answers":{"operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.8,"WAIT":0.2},"confidence":0.8}},"usage":{}}),
            json!({"answers":{"operation":{"type":"noul","choice":"CLICK","probabilities":{"CLICK":0.8,"WAIT":0.2},"confidence":0.8},"step_done":{"type":"noul","noul":0.1}},"usage":{}}),
            json!({"answers":{"operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.8,"WAIT":0.2},"confidence":0.8},"step_done":{"type":"noul","noul":0.1}}}),
            json!({"answers":{"operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.8,"WAIT":0.2},"confidence":0.8},"step_done":{"type":"noul","noul":0.1}},"usage":{"input_tokens":-1}}),
        ] {
            assert!(matches!(
                parse_evaluation(body, &questions, None, "jev".into()),
                Err(JevError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn rejects_a_choice_that_is_not_its_probability_argmax() {
        let questions = validate_request(&request()).unwrap().clone();
        let mismatched = json!({"answers":{
            "operation":{"type":"choice","choice":"WAIT","probabilities":{"CLICK":0.8,"WAIT":0.2},"confidence":0.8},
            "step_done":{"type":"noul","noul":0.1}},"usage":{}});
        assert_eq!(
            parse_evaluation(mismatched, &questions, None, "jev-1.13.0".into()),
            Err(JevError::InvalidResponse(
                "answer operation choice is not an argmax".into()
            ))
        );
    }

    #[test]
    fn model_pin_rejects_drift() {
        let client = Client::new("not-a-real-key".into(), "http://127.0.0.1").unwrap();
        client.pin("jev-1.13").unwrap();
        assert_eq!(client.pin("jev-1.14"), Err(JevError::ModelChanged));
    }

    #[test]
    fn retries_request_timeout_and_evaluates_the_second_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut first = listener.accept().unwrap().0;
            read_request(&mut first);
            first
                .write_all(
                    b"HTTP/1.1 408 Request Timeout\r\nRetry-After-Ms: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();

            let mut second = listener.accept().unwrap().0;
            read_request(&mut second);
            let body = json!({"answers":{
                "operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.7,"WAIT":0.3},"confidence":0.8},
                "step_done":{"type":"noul","noul":0.1}
            },"model":"jev-1.13","usage":{"input_tokens":12}})
            .to_string();
            write!(
                second,
                "HTTP/1.1 200 OK\r\nX-TypeSafe-Request-Id: request-2\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let client = Client::new("transport-test-key".into(), endpoint).unwrap();
        let evaluation = client
            .evaluate(&request(), Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(evaluation.request_id.as_deref(), Some("request-2"));
        assert_eq!(evaluation.model, "jev-1.13");
        assert_eq!(
            client.request_for_run(&request()).unwrap()["model"],
            "jev-1.13"
        );
        server.join().unwrap();
    }

    #[test]
    fn retries_an_invalid_success_response_within_the_logical_call() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut first = listener.accept().unwrap().0;
            read_request(&mut first);
            let invalid = json!({"model":"jev-transient","usage":{}}).to_string();
            write_response(&mut first, "request-invalid", &invalid);

            let mut second = listener.accept().unwrap().0;
            read_request(&mut second);
            let valid = valid_response_body();
            write_response(&mut second, "request-valid", &valid);
        });

        let client = Client::new("transport-test-key".into(), endpoint).unwrap();
        let evaluation = client
            .evaluate(&request(), Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(evaluation.request_id.as_deref(), Some("request-valid"));
        assert_eq!(evaluation.model, "jev-1.13");
        server.join().unwrap();
    }

    #[test]
    fn does_not_retry_a_rejected_request_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = Client::new("transport-test-key".into(), endpoint).unwrap();
        assert_eq!(
            client.evaluate(&request(), Instant::now() + Duration::from_secs(2)),
            Err(JevError::InvalidResponse(
                "provider rejected the request shape".into()
            ))
        );
        server.join().unwrap();
    }

    #[test]
    fn rejects_missing_empty_oversized_and_unsafe_provider_request_ids_on_the_wire() {
        for header in [
            None,
            Some("".to_owned()),
            Some("x".repeat(129)),
            Some("unsafe/request".to_owned()),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                for _ in 0..3 {
                    let mut stream = listener.accept().unwrap().0;
                    read_request(&mut stream);
                    let body = valid_response_body();
                    let request_id = header
                        .as_ref()
                        .map(|value| format!("X-TypeSafe-Request-Id: {value}\r\n"))
                        .unwrap_or_default();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\n{request_id}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            });
            let client = Client::new("transport-test-key".into(), endpoint).unwrap();
            assert!(matches!(
                client.evaluate(&request(), Instant::now() + Duration::from_secs(2)),
                Err(JevError::InvalidResponse(_))
            ));
            server.join().unwrap();
        }
    }

    #[test]
    fn retry_statuses_and_elapsed_deadlines_are_explicit() {
        assert!(retryable(StatusCode::REQUEST_TIMEOUT));
        assert!(retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!retryable(StatusCode::BAD_REQUEST));
        assert_eq!(
            wait_before_retry(0, Some(Duration::ZERO), Instant::now()),
            Err(JevError::Deadline)
        );
    }

    #[test]
    fn connect_failures_retry_only_within_the_attempt_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = Client::new("transport-test-key".into(), endpoint).unwrap();
        assert_eq!(
            client.evaluate(&request(), Instant::now() + Duration::from_secs(4)),
            Err(JevError::Unavailable)
        );
    }

    fn read_request(stream: &mut TcpStream) {
        let mut buffer = [0_u8; 8_192];
        let _ = stream.read(&mut buffer).unwrap();
    }

    fn valid_response_body() -> String {
        json!({"answers":{
            "operation":{"type":"choice","choice":"CLICK","probabilities":{"CLICK":0.7,"WAIT":0.3},"confidence":0.8},
            "step_done":{"type":"noul","noul":0.1}
        },"model":"jev-1.13","usage":{"input_tokens":12}})
        .to_string()
    }

    fn write_response(stream: &mut TcpStream, request_id: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nX-TypeSafe-Request-Id: {request_id}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }
}
