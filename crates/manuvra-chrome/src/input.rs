use crate::transport::{CdpClient, CommandOutcome};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedOperation {
    Click,
    TypeText,
}

#[derive(Debug, Clone)]
pub struct PreparedInput {
    pub document_id: String,
    pub node_id: u64,
    pub operation: PreparedOperation,
    pub text: Option<String>,
    pub action_sequence: u64,
}

#[derive(Debug, Clone, Default)]
pub struct InputCancellation(Arc<AtomicBool>);

impl InputCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn shared(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    #[cfg(test)]
    fn test_signal(&self) -> Arc<AtomicBool> {
        self.shared()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformFact {
    pub readback: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PerformError {
    #[error("input was not performed: {0}")]
    NotPerformed(String),
    #[error("input outcome is uncertain: {0}")]
    Uncertain(String),
    #[error("target changed before input: {0}")]
    Rejected(String),
}

pub(crate) fn perform(
    client: &Arc<CdpClient>,
    input: PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    client.set_action_sequence(input.action_sequence);
    let fence = client.cursor();
    let target = revalidate(client, &input, cancellation)?;
    let result = match input.operation {
        PreparedOperation::Click => click(client, &target, cancellation),
        PreparedOperation::TypeText => type_text(client, &input, &target, cancellation),
    };
    if result.is_ok() && client.snapshot_since(fence).overflowed {
        return Err(PerformError::Uncertain("CDP journal overflowed".into()));
    }
    result
}

fn revalidate(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<Value, PerformError> {
    let document = serde_json::to_string(&input.document_id).expect("document id JSON");
    let editable = input.operation == PreparedOperation::TypeText;
    let script = format!(
        r#"(() => {{
      const element=window.__manuvra?.nodes?.get({});
      if (!element || !element.isConnected) return {{ok:false,reason:'target_missing'}};
      if (String(performance.timeOrigin)!=={}) return {{ok:false,reason:'document_changed'}};
      if (element.disabled || element.getAttribute('aria-disabled')==='true') return {{ok:false,reason:'disabled'}};
      const rect=element.getBoundingClientRect();
      if (!(rect.width>0 && rect.height>0 && rect.x+rect.width>0 && rect.y+rect.height>0 && rect.x<innerWidth && rect.y<innerHeight)) return {{ok:false,reason:'not_in_view'}};
      const x=Math.max(0,Math.min(innerWidth-1,rect.x+rect.width/2)); const y=Math.max(0,Math.min(innerHeight-1,rect.y+rect.height/2));
      const hit=element.ownerDocument.elementFromPoint(x,y);
      if (hit!==element && !element.contains(hit)) return {{ok:false,reason:'covered'}};
      if ({} && (element.readOnly || element.getAttribute('aria-readonly')==='true' || !('value' in element || element.isContentEditable))) return {{ok:false,reason:'not_editable'}};
      return {{ok:true,x,y}};
    }})()"#,
        input.node_id, document, editable
    );
    let response = command(
        client,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true}),
        cancellation,
    )?;
    let value = response
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(PerformError::Rejected(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("invalid target")
                .into(),
        ));
    }
    Ok(value)
}

fn click(
    client: &CdpClient,
    target: &Value,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let x = target
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| PerformError::Rejected("missing target geometry".into()))?;
    let y = target
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| PerformError::Rejected("missing target geometry".into()))?;
    command(
        client,
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
        cancellation,
    )?;
    command_after_suboperation(
        client,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
        cancellation,
    )?;
    Ok(PerformFact { readback: None })
}

fn type_text(
    client: &CdpClient,
    input: &PreparedInput,
    _target: &Value,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let script = format!(
        r#"(() => {{ const element=window.__manuvra.nodes.get({}); element.focus(); if (typeof element.select==='function') element.select(); else {{ const selection=getSelection(); const range=document.createRange(); range.selectNodeContents(element); selection.removeAllRanges(); selection.addRange(range); }} return true; }})()"#,
        input.node_id
    );
    command(
        client,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true}),
        cancellation,
    )?;
    let text = input
        .text
        .as_deref()
        .ok_or_else(|| PerformError::Rejected("TYPE_TEXT omitted text".into()))?;
    command_after_suboperation(
        client,
        "Input.insertText",
        json!({"text":text}),
        cancellation,
    )?;
    let readback_script = format!(
        r#"(() => {{ const element=window.__manuvra?.nodes?.get({}); if (!element || !element.isConnected) return {{ok:false}}; return {{ok:true,value:'value' in element?String(element.value):String(element.innerText||'')}}; }})()"#,
        input.node_id
    );
    let response = command_after_suboperation(
        client,
        "Runtime.evaluate",
        json!({"expression":readback_script,"returnByValue":true}),
        cancellation,
    )?;
    readback_fact(&response)
}

fn readback_fact(response: &Value) -> Result<PerformFact, PerformError> {
    let readback = response
        .pointer("/result/value")
        .ok_or_else(|| PerformError::Uncertain("readback response was unavailable".into()))?;
    if readback.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(PerformError::Uncertain(
            "readback target was unavailable".into(),
        ));
    }
    let value = readback
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| PerformError::Uncertain("readback value was unavailable".into()))?;
    Ok(PerformFact {
        readback: Some(value.to_owned()),
    })
}

fn command(
    client: &CdpClient,
    method: &str,
    params: Value,
    cancellation: &InputCancellation,
) -> Result<Value, PerformError> {
    classify(client.command(method, params, deadline(), cancellation.shared()))
}

fn command_after_suboperation(
    client: &CdpClient,
    method: &str,
    params: Value,
    cancellation: &InputCancellation,
) -> Result<Value, PerformError> {
    match client.command(method, params, deadline(), cancellation.shared()) {
        CommandOutcome::Confirmed(value) => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        CommandOutcome::Rejected(_) => Err(PerformError::Uncertain(
            "CDP rejected a compound-input suboperation".into(),
        )),
        CommandOutcome::NotSent(_) => Err(PerformError::Uncertain(
            "compound input was interrupted between suboperations".into(),
        )),
        CommandOutcome::Unknown(_) => Err(PerformError::Uncertain(
            "compound-input transmission was uncertain".into(),
        )),
    }
}

fn classify(outcome: CommandOutcome) -> Result<Value, PerformError> {
    match outcome {
        CommandOutcome::Confirmed(value) => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        CommandOutcome::Rejected(_) => Err(PerformError::Rejected(
            "CDP rejected target revalidation or input".into(),
        )),
        CommandOutcome::NotSent(message) => Err(PerformError::NotPerformed(message)),
        CommandOutcome::Unknown(message) => Err(PerformError::Uncertain(message)),
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_support::ScriptedChrome;

    fn input(operation: PreparedOperation) -> PreparedInput {
        PreparedInput {
            document_id: "d".into(),
            node_id: 7,
            operation,
            text: Some("Wanted".into()),
            action_sequence: 1,
        }
    }

    #[test]
    fn click_revalidates_and_dispatches_press_release() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        let client = chrome.connect_raw();
        assert_eq!(
            perform(
                &client,
                input(PreparedOperation::Click),
                &InputCancellation::default(),
            )
            .unwrap(),
            PerformFact { readback: None }
        );
    }

    #[test]
    fn type_text_selects_inserts_and_reads_back() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        chrome.reply("Runtime.evaluate", json!({"result":{"value":true}}));
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"value":"Wanted"}}}),
        );
        let client = chrome.connect_raw();
        assert_eq!(
            perform(
                &client,
                input(PreparedOperation::TypeText),
                &InputCancellation::default(),
            )
            .unwrap(),
            PerformFact {
                readback: Some("Wanted".into())
            }
        );
    }

    #[test]
    fn missing_readback_value_is_uncertain() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        chrome.reply("Runtime.evaluate", json!({"result":{"value":true}}));
        chrome.reply("Runtime.evaluate", json!({"result":{"value":{"ok":true}}}));
        let client = chrome.connect_raw();
        assert!(matches!(
            perform(
                &client,
                input(PreparedOperation::TypeText),
                &InputCancellation::default(),
            ),
            Err(PerformError::Uncertain(reason)) if reason == "readback value was unavailable"
        ));
    }

    #[test]
    fn journal_overflow_makes_the_affected_action_uncertain() {
        let chrome = ScriptedChrome::start();
        for index in 0..10_100 {
            chrome.push_event("DOM.attributeModified", json!({"nodeId":index}));
        }
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        let client = chrome.connect_raw();
        let result = perform(
            &client,
            input(PreparedOperation::Click),
            &InputCancellation::default(),
        );
        assert!(
            matches!(
                result,
                Err(PerformError::Uncertain(ref reason)) if reason == "CDP journal overflowed"
            ),
            "{result:?}"
        );
    }

    #[test]
    fn revalidation_and_transport_outcomes_preserve_truth() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":false,"reason":"covered"}}}),
        );
        let client = chrome.connect_raw();
        assert!(matches!(
            perform(
                &client,
                input(PreparedOperation::Click),
                &InputCancellation::default(),
            ),
            Err(PerformError::Rejected(reason)) if reason == "covered"
        ));
        assert!(matches!(
            classify(CommandOutcome::NotSent("before send".into())),
            Err(PerformError::NotPerformed(_))
        ));
        assert!(matches!(
            classify(CommandOutcome::Unknown("after send".into())),
            Err(PerformError::Uncertain(_))
        ));
        assert!(matches!(
            classify(CommandOutcome::Rejected(json!({}))),
            Err(PerformError::Rejected(_))
        ));
    }

    #[test]
    fn interrupted_compound_suboperation_is_uncertain() {
        let chrome = ScriptedChrome::start();
        chrome.reject("Input.insertText");
        let client = chrome.connect_raw();
        assert!(matches!(
            command_after_suboperation(
                &client,
                "Input.insertText",
                json!({"text":"x"}),
                &InputCancellation::default(),
            ),
            Err(PerformError::Uncertain(_))
        ));
    }

    #[test]
    fn shared_cancellation_between_compound_suboperations_is_truthfully_uncertain() {
        for operation in [PreparedOperation::Click, PreparedOperation::TypeText] {
            let chrome = ScriptedChrome::start();
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            );
            if operation == PreparedOperation::TypeText {
                chrome.reply("Runtime.evaluate", json!({"result":{"value":true}}));
            }
            let cancellation = InputCancellation::default();
            let boundary = if operation == PreparedOperation::Click {
                "Input.dispatchMouseEvent"
            } else {
                "Runtime.evaluate"
            };
            chrome.cancel_after(
                boundary,
                if operation == PreparedOperation::TypeText {
                    2
                } else {
                    1
                },
                cancellation.test_signal(),
            );
            let client = chrome.connect_raw();
            let result = perform(&client, input(operation), &cancellation);
            assert!(
                matches!(result, Err(PerformError::Uncertain(ref reason)) if reason.contains("between suboperations")),
                "{operation:?}: {result:?}"
            );
        }
    }
}
