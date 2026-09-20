use crate::transport::{CdpClient, CommandOutcome};
use serde_json::{Value, json};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedOperation {
    Click,
    TypeText,
    Select,
    SetValue,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone)]
pub struct PreparedInput {
    pub document_id: String,
    pub node_id: u64,
    pub operation: PreparedOperation,
    pub text: Option<String>,
    pub previous_text: Option<String>,
    pub option_node_id: Option<u64>,
    pub combobox: bool,
    pub action_sequence: u64,
}

#[derive(Debug, Clone, Default)]
pub struct InputCancellation {
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    cancel_at_boundary: Arc<AtomicUsize>,
}

impl InputCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn shared(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    #[cfg(test)]
    fn cancel_before_boundary(&self, boundary: usize) {
        assert!(boundary > 0);
        self.cancel_at_boundary.store(boundary, Ordering::SeqCst);
    }

    fn enter_suboperation(&self) {
        #[cfg(test)]
        {
            let remaining = self.cancel_at_boundary.load(Ordering::SeqCst);
            if remaining > 0 && self.cancel_at_boundary.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.cancel();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformFact {
    pub readback: Option<String>,
    pub readback_matches: Option<bool>,
    pub suboperations: Vec<String>,
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
    let result = perform_input(client, &input, cancellation);
    complete_with_journal(client, fence, result)
}

fn perform_input(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let target = if matches!(
        input.operation,
        PreparedOperation::ScrollUp | PreparedOperation::ScrollDown
    ) {
        Value::Null
    } else {
        revalidate(client, input, cancellation)?
    };
    dispatch_prepared(client, input, &target, cancellation)
}

fn dispatch_prepared(
    client: &CdpClient,
    input: &PreparedInput,
    target: &Value,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    match input.operation {
        PreparedOperation::Click => click(client, target, cancellation),
        PreparedOperation::TypeText => type_text(client, input, target, cancellation),
        PreparedOperation::Select => select(client, input, cancellation),
        PreparedOperation::SetValue => set_value(client, input, cancellation),
        PreparedOperation::ScrollUp => scroll(client, -1, cancellation),
        PreparedOperation::ScrollDown => scroll(client, 1, cancellation),
    }
}

fn complete_with_journal(
    client: &CdpClient,
    fence: u64,
    result: Result<PerformFact, PerformError>,
) -> Result<PerformFact, PerformError> {
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
    let editable = matches!(
        input.operation,
        PreparedOperation::TypeText | PreparedOperation::SetValue | PreparedOperation::Select
    );
    let script = format!(
        r#"(() => {{
      const element=window.__manuvra?.nodes?.get({});
      if (!element || !element.isConnected) return {{ok:false,reason:'target_missing'}};
      if (String(performance.timeOrigin)!=={}) return {{ok:false,reason:'document_changed'}};
      if (element.disabled || element.getAttribute('aria-disabled')==='true') return {{ok:false,reason:'disabled'}};
      const rect=element.getBoundingClientRect();
      if (!(rect.width>0 && rect.height>0 && rect.x+rect.width>0 && rect.y+rect.height>0 && rect.x<innerWidth && rect.y<innerHeight)) return {{ok:false,reason:'not_in_view'}};
      const x=Math.max(0,Math.min(element.ownerDocument.defaultView.innerWidth-1,rect.x+rect.width/2)); const y=Math.max(0,Math.min(element.ownerDocument.defaultView.innerHeight-1,rect.y+rect.height/2));
      const root=element.getRootNode(); const hit=(root.elementFromPoint||element.ownerDocument.elementFromPoint).call(root,x,y);
      if (hit!==element && !element.contains(hit)) return {{ok:false,reason:'covered'}};
      if ({} && (element.readOnly || element.getAttribute('aria-readonly')==='true' || !('value' in element || element.isContentEditable))) return {{ok:false,reason:'not_editable'}};
      let globalX=x, globalY=y, view=element.ownerDocument.defaultView;
      while (view && view!==window) {{ const frame=view.frameElement; if (!frame) return {{ok:false,reason:'cross_origin_frame'}}; const frameRect=frame.getBoundingClientRect(); globalX+=frameRect.x; globalY+=frameRect.y; view=view.parent; }}
      return {{ok:true,x:globalX,y:globalY}};
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
    Ok(PerformFact {
        readback: None,
        readback_matches: None,
        suboperations: vec!["mouse_press".into(), "mouse_release".into()],
    })
}

fn type_text(
    client: &CdpClient,
    input: &PreparedInput,
    _target: &Value,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    focus_and_select(client, input.node_id, cancellation)?;
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
    let first = read_text(
        client,
        input.node_id,
        cancellation,
        None,
        vec!["select_all".into(), "insert_text".into()],
    )?;
    match classify_insert_readback(&first, input.previous_text.as_deref(), text) {
        InsertReadback::Matched => settle_typed(first, true, input.combobox),
        InsertReadback::ChangedWrong => settle_typed(first, false, input.combobox),
        InsertReadback::Unchanged => per_key_text(client, input, text, cancellation),
    }
}

fn focus_and_select(
    client: &CdpClient,
    node_id: u64,
    cancellation: &InputCancellation,
) -> Result<(), PerformError> {
    let script = format!(
        r#"(() => {{ const element=window.__manuvra.nodes.get({}); element.focus(); if (typeof element.select==='function') element.select(); else {{ const selection=getSelection(); const range=document.createRange(); range.selectNodeContents(element); selection.removeAllRanges(); selection.addRange(range); }} return true; }})()"#,
        node_id
    );
    command(
        client,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true}),
        cancellation,
    )?;
    Ok(())
}

fn read_text(
    client: &CdpClient,
    node_id: u64,
    cancellation: &InputCancellation,
    expected: Option<&str>,
    suboperations: Vec<String>,
) -> Result<PerformFact, PerformError> {
    let readback_script = format!(
        r#"(() => {{ const element=window.__manuvra?.nodes?.get({}); if (!element || !element.isConnected) return {{ok:false}}; return {{ok:true,value:'value' in element?String(element.value):String(element.innerText||'')}}; }})()"#,
        node_id
    );
    let response = command_after_suboperation(
        client,
        "Runtime.evaluate",
        json!({"expression":readback_script,"returnByValue":true}),
        cancellation,
    )?;
    readback_fact(&response, expected, suboperations)
}

enum InsertReadback {
    Matched,
    ChangedWrong,
    Unchanged,
}

fn classify_insert_readback(
    fact: &PerformFact,
    previous: Option<&str>,
    expected: &str,
) -> InsertReadback {
    if fact.readback.as_deref() == Some(expected) {
        InsertReadback::Matched
    } else if fact.readback.as_deref() == previous {
        InsertReadback::Unchanged
    } else {
        InsertReadback::ChangedWrong
    }
}

fn settle_typed(
    mut fact: PerformFact,
    matches: bool,
    combobox: bool,
) -> Result<PerformFact, PerformError> {
    fact.readback_matches = Some(matches);
    if combobox && matches {
        std::thread::sleep(Duration::from_millis(300));
        fact.suboperations.push("combobox_option_wait".into());
    }
    Ok(fact)
}

fn per_key_text(
    client: &CdpClient,
    input: &PreparedInput,
    text: &str,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    revalidate(client, input, cancellation)?;
    for character in text.chars() {
        let text = character.to_string();
        command_after_suboperation(
            client,
            "Input.dispatchKeyEvent",
            json!({"type":"keyDown","key":text,"text":text}),
            cancellation,
        )?;
        command_after_suboperation(
            client,
            "Input.dispatchKeyEvent",
            json!({"type":"keyUp","key":text}),
            cancellation,
        )?;
    }
    let fact = read_text(
        client,
        input.node_id,
        cancellation,
        Some(text),
        vec![
            "select_all".into(),
            "insert_text_no_effect".into(),
            "dispatch_key_events".into(),
        ],
    )?;
    let matches = fact.readback_matches.unwrap_or(false);
    settle_typed(fact, matches, input.combobox)
}

fn readback_fact(
    response: &Value,
    expected: Option<&str>,
    suboperations: Vec<String>,
) -> Result<PerformFact, PerformError> {
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
        readback_matches: expected.map(|expected| value == expected),
        suboperations,
    })
}

fn select(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let option = input
        .option_node_id
        .ok_or_else(|| PerformError::Rejected("SELECT omitted observed option identity".into()))?;
    let expected = input
        .text
        .as_deref()
        .ok_or_else(|| PerformError::Rejected("SELECT omitted option text".into()))?;
    let script = format!(
        r#"(() => {{
          const select=window.__manuvra?.nodes?.get({}); const option=window.__manuvra?.nodes?.get({});
          if (!select?.isConnected || !option?.isConnected || option.closest('select')!==select || option.disabled) return {{ok:false}};
          const setter=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;
          setter.call(select,option.value); select.dispatchEvent(new Event('input',{{bubbles:true}})); select.dispatchEvent(new Event('change',{{bubbles:true}}));
          const selected=select.selectedOptions[0]; return {{ok:true,value:String(select.value),label:selected?.label||selected?.textContent?.trim()||''}};
        }})()"#,
        input.node_id, option
    );
    let response = command_after_suboperation(
        client,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true}),
        cancellation,
    )?;
    let (value, label) = select_readback(&response)?;
    Ok(PerformFact {
        readback: Some(value.to_owned()),
        readback_matches: Some(value == expected || label == expected),
        suboperations: vec![
            "select_option".into(),
            "input_event".into(),
            "change_event".into(),
        ],
    })
}

fn select_readback(response: &Value) -> Result<(&str, &str), PerformError> {
    let readback = response
        .pointer("/result/value")
        .ok_or_else(|| PerformError::Uncertain("select readback was unavailable".into()))?;
    if readback.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(PerformError::Uncertain(
            "observed select option was unavailable".into(),
        ));
    }
    let value = readback
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| PerformError::Uncertain("select value was unavailable".into()))?;
    Ok((
        value,
        readback.get("label").and_then(Value::as_str).unwrap_or(""),
    ))
}

fn set_value(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let expected = input
        .text
        .as_deref()
        .ok_or_else(|| PerformError::Rejected("SET_VALUE omitted text".into()))?;
    let text = serde_json::to_string(expected).expect("input text JSON");
    let script = format!(
        r#"(() => {{
          const element=window.__manuvra?.nodes?.get({}); if (!element?.isConnected) return {{ok:false}};
          const setter=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set;
          setter.call(element,{}); element.dispatchEvent(new Event('input',{{bubbles:true}})); element.dispatchEvent(new Event('change',{{bubbles:true}}));
          return {{ok:true,value:String(element.value)}};
        }})()"#,
        input.node_id, text
    );
    let response = command_after_suboperation(
        client,
        "Runtime.evaluate",
        json!({"expression":script,"returnByValue":true}),
        cancellation,
    )?;
    readback_fact(
        &response,
        Some(expected),
        vec![
            "native_setter".into(),
            "input_event".into(),
            "change_event".into(),
        ],
    )
}

fn scroll(
    client: &CdpClient,
    direction: i8,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let expression = format!(
        "(() => {{ const before=scrollY; scrollBy(0,{} * Math.max(120,innerHeight*0.75)); return {{before,after:scrollY}}; }})()",
        direction
    );
    command(
        client,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true}),
        cancellation,
    )?;
    Ok(PerformFact {
        readback: None,
        readback_matches: None,
        suboperations: vec![if direction < 0 {
            "scroll_up".into()
        } else {
            "scroll_down".into()
        }],
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
    cancellation.enter_suboperation();
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
            previous_text: Some(String::new()),
            option_node_id: None,
            combobox: false,
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
            PerformFact {
                readback: None,
                readback_matches: None,
                suboperations: vec!["mouse_press".into(), "mouse_release".into()]
            }
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
                readback: Some("Wanted".into()),
                readback_matches: Some(true),
                suboperations: vec!["select_all".into(), "insert_text".into()]
            }
        );
    }

    #[test]
    fn unchanged_insert_text_continues_with_per_key_events_in_the_same_action() {
        let chrome = ScriptedChrome::start();
        for response in [
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            json!({"result":{"value":true}}),
            json!({"result":{"value":{"ok":true,"value":""}}}),
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            json!({"result":{"value":{"ok":true,"value":"Wanted"}}}),
        ] {
            chrome.reply("Runtime.evaluate", response);
        }
        let client = chrome.connect_raw();
        let fact = perform(
            &client,
            input(PreparedOperation::TypeText),
            &InputCancellation::default(),
        )
        .unwrap();
        assert_eq!(fact.readback.as_deref(), Some("Wanted"));
        assert_eq!(fact.readback_matches, Some(true));
        assert_eq!(
            fact.suboperations,
            ["select_all", "insert_text_no_effect", "dispatch_key_events"]
        );
    }

    #[test]
    fn changed_wrong_insert_text_is_a_mismatch_without_per_key_continuation() {
        let chrome = ScriptedChrome::start();
        for response in [
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            json!({"result":{"value":true}}),
            json!({"result":{"value":{"ok":true,"value":"garbled"}}}),
        ] {
            chrome.reply("Runtime.evaluate", response);
        }
        let client = chrome.connect_raw();
        let fact = perform(
            &client,
            input(PreparedOperation::TypeText),
            &InputCancellation::default(),
        )
        .unwrap();
        assert_eq!(fact.readback.as_deref(), Some("garbled"));
        assert_eq!(fact.readback_matches, Some(false));
        assert_eq!(fact.suboperations, ["select_all", "insert_text"]);
    }

    #[test]
    fn successful_combobox_text_records_the_option_wait_but_mismatch_does_not_wait() {
        let run = |readback: &str| {
            let chrome = ScriptedChrome::start();
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            );
            chrome.reply("Runtime.evaluate", json!({"result":{"value":true}}));
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"value":{"ok":true,"value":readback}}}),
            );
            let client = chrome.connect_raw();
            let mut prepared = input(PreparedOperation::TypeText);
            prepared.combobox = true;
            perform(&client, prepared, &InputCancellation::default()).unwrap()
        };

        let matched = run("Wanted");
        assert_eq!(matched.readback_matches, Some(true));
        assert_eq!(
            matched.suboperations,
            ["select_all", "insert_text", "combobox_option_wait"]
        );
        let mismatched = run("garbled");
        assert_eq!(mismatched.readback_matches, Some(false));
        assert_eq!(mismatched.suboperations, ["select_all", "insert_text"]);
    }

    #[test]
    fn native_select_uses_the_observed_option_identity_and_reads_back() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"value":"Wanted","label":"Wanted"}}}),
        );
        let client = chrome.connect_raw();
        let mut prepared = input(PreparedOperation::Select);
        prepared.option_node_id = Some(8);
        let fact = perform(&client, prepared, &InputCancellation::default()).unwrap();
        assert_eq!(fact.readback_matches, Some(true));
        assert_eq!(
            fact.suboperations,
            ["select_option", "input_event", "change_event"]
        );
    }

    #[test]
    fn specialized_input_uses_native_setter_and_event_readback() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"value":"Wanted"}}}),
        );
        let client = chrome.connect_raw();
        let fact = perform(
            &client,
            input(PreparedOperation::SetValue),
            &InputCancellation::default(),
        )
        .unwrap();
        assert_eq!(fact.readback_matches, Some(true));
        assert_eq!(
            fact.suboperations,
            ["native_setter", "input_event", "change_event"]
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
        for (operation, boundary) in [
            (PreparedOperation::Click, 1),
            (PreparedOperation::TypeText, 1),
            (PreparedOperation::TypeText, 2),
        ] {
            let chrome = ScriptedChrome::start();
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            );
            if operation == PreparedOperation::TypeText {
                chrome.reply("Runtime.evaluate", json!({"result":{"value":true}}));
            }
            let cancellation = InputCancellation::default();
            cancellation.cancel_before_boundary(boundary);
            let client = chrome.connect_raw();
            let result = perform(&client, input(operation), &cancellation);
            assert!(
                matches!(result, Err(PerformError::Uncertain(ref reason)) if reason.contains("between suboperations")),
                "{operation:?}: {result:?}"
            );
        }
    }
}
