use crate::FocusAnchor;
use crate::transport::{CdpClient, CommandOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
#[path = "input/darwin.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "input/linux.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::Key;
    use serde_json::Value;

    pub(super) fn add_editing_commands(_key: Key, _event: &mut Value) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedOperation {
    Click,
    TypeText,
    Select,
    SetValue,
    ScrollUp,
    ScrollDown,
    /// Moves the pointer over a hover region's first hidden control, which is revalidated like a
    /// click target, to reveal the region's controls.
    Hover,
    PressKey(Key),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Escape,
    Tab,
    #[serde(rename = "Shift+Tab")]
    ShiftTab,
    Enter,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
}

impl Key {
    pub fn from_choice(choice: &str) -> Option<Self> {
        serde_json::from_value(Value::String(choice.to_owned())).ok()
    }
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
    pub focus_anchor: Option<FocusAnchor>,
    pub scroll_region: Option<crate::ScrollRegion>,
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
    let fence = client.cursor();
    let result = perform_input(client, &input, cancellation);
    complete_with_journal(client, fence, result)
}

fn perform_input(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    if matches!(input.operation, PreparedOperation::PressKey(_)) {
        revalidate_focus(client, input, cancellation)?;
        return dispatch_prepared(client, input, &Value::Null, cancellation);
    }
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
        PreparedOperation::ScrollUp | PreparedOperation::ScrollDown => {
            scroll_input(client, input, cancellation)
        }
        PreparedOperation::Hover => hover(client, target, cancellation),
        PreparedOperation::PressKey(key) => press_key(client, key, cancellation),
    }
}

fn revalidate_focus(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<(), PerformError> {
    let response = command(
        client,
        "Runtime.evaluate",
        json!({"expression":include_str!("snapshot.js"),"returnByValue":true}),
        cancellation,
    )?;
    let mut fresh: crate::Observation = serde_json::from_value(
        response
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|_| PerformError::Rejected("focus observation unavailable".into()))?;
    if fresh.document_id != input.document_id {
        return Err(PerformError::Rejected("document_changed".into()));
    }
    crate::observation::mark_closed_shadow_focus(&mut fresh, |method, params| {
        command(client, method, params, cancellation)
    })?;
    if !focus_identity_matches(&fresh.focus_anchor, &input.focus_anchor) {
        return Err(PerformError::Rejected("focus_changed".into()));
    }
    Ok(())
}

fn focus_identity_matches(fresh: &Option<FocusAnchor>, expected: &Option<FocusAnchor>) -> bool {
    match (fresh, expected) {
        (None, None) => true,
        (Some(fresh), Some(expected)) => fresh.same_identity(expected),
        _ => false,
    }
}

fn press_key(
    client: &CdpClient,
    key: Key,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let fields = key_fields(key)
        .ok_or_else(|| PerformError::Rejected("key has no native key mapping".into()))?;
    command(
        client,
        "Input.dispatchKeyEvent",
        key_down(key, fields),
        cancellation,
    )?;
    command_after_suboperation(
        client,
        "Input.dispatchKeyEvent",
        key_event("keyUp", fields),
        cancellation,
    )?;
    Ok(PerformFact {
        readback: None,
        readback_matches: None,
        suboperations: vec!["key_down".into(), "key_up".into()],
    })
}

fn key_down(key: Key, fields: KeyFields) -> Value {
    let mut event = key_event("keyDown", fields);
    if let Some(text) = key_text(key) {
        event["text"] = json!(text);
    }
    platform::add_editing_commands(key, &mut event);
    event
}

fn key_text(key: Key) -> Option<&'static str> {
    match key {
        Key::Enter => Some("\r"),
        Key::Space => Some(" "),
        _ => None,
    }
}

fn key_event(event_type: &str, (name, code, virtual_key, modifiers): KeyFields) -> Value {
    json!({
        "type":event_type,"key":name,"code":code,
        "windowsVirtualKeyCode":virtual_key,"nativeVirtualKeyCode":virtual_key,
        "modifiers":modifiers
    })
}

/// DOM key, DOM code, virtual key code, and modifiers.
type KeyFields = (&'static str, &'static str, u32, u32);

const SHIFT_MODIFIER: u32 = 8;

const KEY_FIELDS: [(Key, KeyFields); 11] = [
    (Key::Escape, ("Escape", "Escape", 27, 0)),
    (Key::Tab, ("Tab", "Tab", 9, 0)),
    (Key::ShiftTab, ("Tab", "Tab", 9, SHIFT_MODIFIER)),
    (Key::Enter, ("Enter", "Enter", 13, 0)),
    (Key::Space, (" ", "Space", 32, 0)),
    (Key::ArrowUp, ("ArrowUp", "ArrowUp", 38, 0)),
    (Key::ArrowDown, ("ArrowDown", "ArrowDown", 40, 0)),
    (Key::ArrowLeft, ("ArrowLeft", "ArrowLeft", 37, 0)),
    (Key::ArrowRight, ("ArrowRight", "ArrowRight", 39, 0)),
    (Key::Home, ("Home", "Home", 36, 0)),
    (Key::End, ("End", "End", 35, 0)),
];

fn key_fields(key: Key) -> Option<KeyFields> {
    KEY_FIELDS
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, fields)| *fields)
}

/// A performed input is uncertain when the journal lost events from its own range,
/// because the effects of the dispatch can no longer be read back.
fn complete_with_journal(
    client: &CdpClient,
    fence: u64,
    result: Result<PerformFact, PerformError>,
) -> Result<PerformFact, PerformError> {
    if result.is_ok() && client.lost_events_since(fence) {
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
      while (view && view!==window) {{ const frame=view.frameElement; if (!frame) return {{ok:false,reason:'cross_origin_frame'}}; const frameRect=frame.getBoundingClientRect(), frameStyle=frame.ownerDocument.defaultView.getComputedStyle(frame); globalX+=frameRect.x+frame.clientLeft+parseFloat(frameStyle.paddingLeft); globalY+=frameRect.y+frame.clientTop+parseFloat(frameStyle.paddingTop); view=view.parent; }}
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
    let (x, y) = revalidated_point(target)?;
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

/// Moves the pointer once to the revalidated point. The move is the hover's only dispatch, so its
/// transport outcome is the hover's outcome.
fn hover(
    client: &CdpClient,
    target: &Value,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let (x, y) = revalidated_point(target)?;
    command(
        client,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseMoved","x":x,"y":y}),
        cancellation,
    )?;
    Ok(PerformFact {
        readback: None,
        readback_matches: None,
        suboperations: vec!["mouse_move".into()],
    })
}

fn revalidated_point(target: &Value) -> Result<(f64, f64), PerformError> {
    let coordinate = |axis: &str| target.get(axis).and_then(Value::as_f64);
    coordinate("x")
        .zip(coordinate("y"))
        .ok_or_else(|| PerformError::Rejected("missing target geometry".into()))
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

fn scroll_input(
    client: &CdpClient,
    input: &PreparedInput,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    match &input.scroll_region {
        Some(region) => scroll_region(client, input, region, cancellation),
        None => scroll(client, input.operation, cancellation),
    }
}

fn scroll_region(
    client: &CdpClient,
    input: &PreparedInput,
    region: &crate::ScrollRegion,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let direction = if input.operation == PreparedOperation::ScrollUp {
        -1
    } else {
        1
    };
    let expression = format!(
        "({})({},{},{})",
        include_str!("scroll-locate.js"),
        serde_json::to_string(&input.document_id)
            .map_err(|e| PerformError::NotPerformed(e.to_string()))?,
        region.node_id,
        direction
    );
    let located = command(
        client,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true}),
        cancellation,
    )?;
    let target = scroll_point(&located)?;
    cancellation.enter_suboperation();
    command(
        client,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseWheel","x":target.0,"y":target.1,"deltaX":0,"deltaY":target.2}),
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

fn scroll_point(located: &Value) -> Result<(f64, f64, f64), PerformError> {
    let target = &located["result"]["value"];
    if target["ok"] != true {
        return Err(PerformError::Rejected(
            target["reason"]
                .as_str()
                .unwrap_or("scroll_region_unavailable")
                .into(),
        ));
    }
    let number = |key| {
        target[key]
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| PerformError::Uncertain("malformed scroll point".into()))
    };
    Ok((number("x")?, number("y")?, number("delta")?))
}

fn scroll(
    client: &CdpClient,
    operation: PreparedOperation,
    cancellation: &InputCancellation,
) -> Result<PerformFact, PerformError> {
    let (direction, suboperation) = if operation == PreparedOperation::ScrollUp {
        (-1, "scroll_up")
    } else {
        (1, "scroll_down")
    };
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
        suboperations: vec![suboperation.into()],
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
            scroll_region: None,
            document_id: "d".into(),
            node_id: 7,
            operation,
            text: Some("Wanted".into()),
            previous_text: Some(String::new()),
            option_node_id: None,
            combobox: false,
            action_sequence: 1,
            focus_anchor: None,
        }
    }

    fn region_input() -> PreparedInput {
        let mut prepared = input(PreparedOperation::ScrollDown);
        prepared.scroll_region = Some(crate::ScrollRegion {
            node_id: 11,
            name: "Rows".into(),
            overlay: None,
            parent_node_id: None,
            can_scroll_up: false,
            can_scroll_down: true,
            scroll_top: 0.0,
            scroll_height: 1000.0,
            client_height: 300.0,
            rect: crate::Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 300.0,
            },
        });
        prepared
    }
    fn reply_scroll_point(chrome: &ScriptedChrome) {
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":100.0,"y":150.0,"delta":292.0}}}),
        );
    }
    #[test]
    fn region_scroll_revalidates_before_wheel_and_preserves_transport_outcomes() {
        let chrome = ScriptedChrome::start();
        reply_scroll_point(&chrome);
        let fact = perform(
            &chrome.connect_raw(),
            region_input(),
            &InputCancellation::default(),
        )
        .unwrap();
        assert_eq!(fact.suboperations, ["scroll_down"]);
        let commands = chrome.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands[1],
            (
                "Input.dispatchMouseEvent".into(),
                json!({"type":"mouseWheel","x":100.0,"y":150.0,"deltaX":0,"deltaY":292.0})
            )
        );
        for reason in [
            "document_changed",
            "target_missing",
            "scroll_region_at_end",
            "covered",
        ] {
            let chrome = ScriptedChrome::start();
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"value":{"ok":false,"reason":reason}}}),
            );
            assert_eq!(
                perform(
                    &chrome.connect_raw(),
                    region_input(),
                    &InputCancellation::default()
                ),
                Err(PerformError::Rejected(reason.into()))
            );
            assert!(chrome.received("Input.dispatchMouseEvent").is_empty());
        }
        for boundary in ["Runtime.evaluate", "Input.dispatchMouseEvent"] {
            let chrome = ScriptedChrome::start();
            reply_scroll_point(&chrome);
            chrome.reject(boundary);
            assert!(matches!(
                perform(
                    &chrome.connect_raw(),
                    region_input(),
                    &InputCancellation::default()
                ),
                Err(PerformError::Rejected(_))
            ));
            let chrome = ScriptedChrome::start();
            reply_scroll_point(&chrome);
            chrome.disconnect_on(boundary);
            assert!(matches!(
                perform(
                    &chrome.connect_raw(),
                    region_input(),
                    &InputCancellation::default()
                ),
                Err(PerformError::Uncertain(_))
            ));
        }
        let chrome = ScriptedChrome::start();
        reply_scroll_point(&chrome);
        let cancellation = InputCancellation::default();
        cancellation.cancel_before_boundary(1);
        assert!(matches!(
            perform(&chrome.connect_raw(), region_input(), &cancellation),
            Err(PerformError::NotPerformed(_))
        ));
        assert_eq!(chrome.commands().len(), 1);
        let chrome = ScriptedChrome::start();
        let cancellation = InputCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            perform(&chrome.connect_raw(), region_input(), &cancellation),
            Err(PerformError::NotPerformed(_))
        ));
        assert!(chrome.commands().is_empty());
    }

    fn focus_input(key: Key) -> PreparedInput {
        PreparedInput {
            scroll_region: None,
            document_id: "d".into(),
            node_id: 0,
            operation: PreparedOperation::PressKey(key),
            text: None,
            previous_text: None,
            option_node_id: None,
            combobox: false,
            action_sequence: 1,
            focus_anchor: Some(FocusAnchor {
                node_id: 7,
                context: "main".into(),
                role: "button".into(),
                name: "Open".into(),
                in_dialog: None,
                container: None,
                covered: true,
                surface: None,
                active_descendant: None,
                expanded: None,
                selected: None,
                checked: None,
                position: None,
            }),
        }
    }

    fn reply_focus(chrome: &ScriptedChrome, anchor: Option<FocusAnchor>, document_id: &str) {
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{
                "document_id":document_id,"url":"http://example.test/","route":"/","title":"x",
                "focused":null,"focus_anchor":anchor,"elements":[],
                "viewport":{"width":10,"height":10,"scroll_x":0,"scroll_y":0,"document_height":10}
            }}}),
        );
    }

    fn press(
        expected: Option<FocusAnchor>,
        script: impl FnOnce(&ScriptedChrome),
    ) -> (Result<PerformFact, PerformError>, usize) {
        let chrome = ScriptedChrome::start();
        script(&chrome);
        let mut input = focus_input(Key::Tab);
        input.focus_anchor = expected;
        let result = perform(&chrome.connect_raw(), input, &InputCancellation::default());
        (result, chrome.received("Input.dispatchKeyEvent").len())
    }

    fn anchor() -> FocusAnchor {
        focus_input(Key::Tab).focus_anchor.unwrap()
    }

    fn descendant(id: &str, name: &str, selected: Option<bool>) -> crate::ActiveDescendant {
        crate::ActiveDescendant {
            id: id.into(),
            role: "option".into(),
            name: name.into(),
            selected,
            checked: None,
        }
    }

    #[test]
    fn stale_focus_or_document_rejects_without_key_dispatch() {
        let mut widget = anchor();
        widget.active_descendant = Some(descendant("alpha", "Alpha", None));
        widget.position = Some(1);
        let mut stale = vec![(Some(anchor()), None), (None, Some(anchor()))];
        for change in [
            (|anchor: &mut FocusAnchor| anchor.name = "Other".into()) as fn(&mut FocusAnchor),
            |anchor| anchor.node_id = 8,
            |anchor| anchor.container = Some("Other row".into()),
            |anchor| anchor.expanded = Some(true),
            |anchor| anchor.selected = Some(true),
            |anchor| anchor.checked = Some(false),
            |anchor| anchor.position = Some(2),
            |anchor| anchor.active_descendant = None,
            |anchor| anchor.active_descendant.as_mut().unwrap().id = "beta".into(),
            |anchor| anchor.active_descendant.as_mut().unwrap().name = "Beta".into(),
            |anchor| anchor.active_descendant.as_mut().unwrap().role = "treeitem".into(),
        ] {
            let mut fresh = widget.clone();
            change(&mut fresh);
            stale.push((Some(widget.clone()), Some(fresh)));
        }
        for (expected, fresh) in stale {
            let (result, key_events) = press(expected, |chrome| reply_focus(chrome, fresh, "d"));
            assert!(
                matches!(result, Err(PerformError::Rejected(ref reason)) if reason == "focus_changed"),
                "{result:?}"
            );
            assert_eq!(key_events, 0);
        }

        let (result, key_events) = press(Some(anchor()), |chrome| {
            reply_focus(chrome, Some(anchor()), "changed")
        });
        assert!(
            matches!(result, Err(PerformError::Rejected(ref reason)) if reason == "document_changed")
        );
        assert_eq!(key_events, 0);

        for snapshot in [json!(null), json!({"document_id":"d"})] {
            let (result, key_events) = press(Some(anchor()), |chrome| {
                chrome.reply("Runtime.evaluate", json!({"result":{"value":snapshot}}))
            });
            assert!(
                matches!(result, Err(PerformError::Rejected(ref reason)) if reason == "focus observation unavailable")
            );
            assert_eq!(key_events, 0);
        }
    }

    #[test]
    fn focus_revalidation_ignores_only_the_descendant_selection_state() {
        let mut expected = anchor();
        expected.active_descendant = Some(descendant("beta", "Beta", Some(false)));
        let mut fresh = expected.clone();
        fresh.active_descendant = Some(descendant("beta", "Beta", Some(true)));
        let (result, key_events) = press(Some(expected), |chrome| {
            reply_focus(chrome, Some(fresh), "d")
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(key_events, 2);
    }

    #[test]
    fn focus_inside_a_closed_shadow_host_is_revalidated_through_cdp() {
        let (result, key_events) = press(Some(anchor()), |chrome| {
            reply_focus(chrome, Some(anchor()), "d");
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"type":"object","objectId":"focus-7"}}),
            );
            chrome.reply(
                "DOM.describeNode",
                json!({"node":{"shadowRoots":[{"shadowRootType":"closed"}]}}),
            );
        });
        assert!(
            matches!(result, Err(PerformError::Rejected(ref reason)) if reason == "focus_changed"),
            "{result:?}"
        );
        assert_eq!(key_events, 0);

        let (result, key_events) = press(Some(anchor()), |chrome| {
            reply_focus(chrome, Some(anchor()), "d");
            chrome.reply(
                "Runtime.evaluate",
                json!({"result":{"type":"object","objectId":"focus-7"}}),
            );
            chrome.reply(
                "DOM.describeNode",
                json!({"node":{"shadowRoots":[{"shadowRootType":"open"}]}}),
            );
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(key_events, 2);
    }

    #[test]
    fn press_key_dispatches_exactly_two_events_with_native_key_parameters() {
        for (key, name, code, virtual_key) in [
            (Key::Escape, "Escape", "Escape", 27),
            (Key::Tab, "Tab", "Tab", 9),
            (Key::ShiftTab, "Tab", "Tab", 9),
            (Key::Enter, "Enter", "Enter", 13),
            (Key::Space, " ", "Space", 32),
            (Key::ArrowUp, "ArrowUp", "ArrowUp", 38),
            (Key::ArrowDown, "ArrowDown", "ArrowDown", 40),
            (Key::ArrowLeft, "ArrowLeft", "ArrowLeft", 37),
            (Key::ArrowRight, "ArrowRight", "ArrowRight", 39),
            (Key::Home, "Home", "Home", 36),
            (Key::End, "End", "End", 35),
        ] {
            // A new key fails to compile here until it is listed above and in KEY_FIELDS.
            match key {
                Key::Escape
                | Key::Tab
                | Key::ShiftTab
                | Key::Enter
                | Key::Space
                | Key::ArrowUp
                | Key::ArrowDown
                | Key::ArrowLeft
                | Key::ArrowRight
                | Key::Home
                | Key::End => {}
            }
            let chrome = ScriptedChrome::start();
            let input = focus_input(key);
            reply_focus(&chrome, input.focus_anchor.clone(), "d");
            let fact =
                perform(&chrome.connect_raw(), input, &InputCancellation::default()).unwrap();
            assert_eq!(fact.suboperations, ["key_down", "key_up"]);
            let events = chrome.received("Input.dispatchKeyEvent");
            assert_eq!(events.len(), 2, "{key:?}");
            let modifiers = if key == Key::ShiftTab { 8 } else { 0 };
            for (event, event_type) in events.iter().zip(["keyDown", "keyUp"]) {
                let params = &event["params"];
                assert_eq!(params["type"], event_type, "{key:?}");
                assert_eq!(params["key"], name, "{key:?} {event_type}");
                assert_eq!(params["code"], code, "{key:?} {event_type}");
                assert_eq!(params["windowsVirtualKeyCode"], virtual_key, "{key:?}");
                assert_eq!(params["modifiers"], modifiers, "{key:?} {event_type}");
            }
            match key {
                Key::Enter => assert_eq!(events[0]["params"]["text"], "\r"),
                Key::Space => assert_eq!(events[0]["params"]["text"], " "),
                _ => assert!(events[0]["params"].get("text").is_none()),
            }
            assert!(events[1]["params"].get("text").is_none());
            assert!(events[1]["params"].get("commands").is_none());
        }
    }

    #[test]
    fn key_dispatch_failures_preserve_the_suboperation_boundary() {
        let not_sent = ScriptedChrome::start();
        let cancellation = InputCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            press_key(&not_sent.connect_raw(), Key::Tab, &cancellation),
            Err(PerformError::NotPerformed(_))
        ));
        assert!(not_sent.received("Input.dispatchKeyEvent").is_empty());

        let revalidation_not_sent = ScriptedChrome::start();
        reply_focus(&revalidation_not_sent, Some(anchor()), "d");
        let cancellation = InputCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            perform(
                &revalidation_not_sent.connect_raw(),
                focus_input(Key::Tab),
                &cancellation
            ),
            Err(PerformError::NotPerformed(_))
        ));
        assert!(
            revalidation_not_sent
                .received("Input.dispatchKeyEvent")
                .is_empty()
        );

        for (script, expected_uncertain) in [
            (
                (|chrome: &ScriptedChrome| chrome.reply_invalid_json("Runtime.evaluate"))
                    as fn(&ScriptedChrome),
                true,
            ),
            (|chrome| chrome.reject("Runtime.evaluate"), false),
        ] {
            let (result, key_events) = press(Some(anchor()), script);
            if expected_uncertain {
                assert!(
                    matches!(result, Err(PerformError::Uncertain(_))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(PerformError::Rejected(_))),
                    "{result:?}"
                );
            }
            assert_eq!(key_events, 0);
        }

        for (script, rejected, expected_events) in [
            (
                (|chrome: &ScriptedChrome| chrome.reject_on_call("Input.dispatchKeyEvent", 1))
                    as fn(&ScriptedChrome),
                true,
                1,
            ),
            (
                |chrome| chrome.reply_invalid_json("Input.dispatchKeyEvent"),
                false,
                1,
            ),
            (
                |chrome| chrome.reject_on_call("Input.dispatchKeyEvent", 2),
                false,
                2,
            ),
            (
                |chrome| chrome.reply_invalid_json_on_call("Input.dispatchKeyEvent", 2),
                false,
                2,
            ),
        ] {
            let (result, key_events) = press(Some(anchor()), |chrome| {
                reply_focus(chrome, Some(anchor()), "d");
                script(chrome);
            });
            if rejected {
                assert!(
                    matches!(result, Err(PerformError::Rejected(_))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(PerformError::Uncertain(_))),
                    "{result:?}"
                );
            }
            assert_eq!(key_events, expected_events);
        }

        let cancelled = ScriptedChrome::start();
        reply_focus(&cancelled, Some(anchor()), "d");
        let cancellation = InputCancellation::default();
        cancellation.cancel_before_boundary(1);
        assert!(matches!(
            perform(
                &cancelled.connect_raw(),
                focus_input(Key::Tab),
                &cancellation
            ),
            Err(PerformError::Uncertain(_))
        ));
        assert_eq!(cancelled.received("Input.dispatchKeyEvent").len(), 1);
    }

    fn flood() -> Vec<(&'static str, Value)> {
        (0..10_100)
            .map(|index| ("DOM.attributeModified", json!({"nodeId":index})))
            .collect()
    }

    #[test]
    fn journal_loss_makes_only_a_performed_input_uncertain() {
        let (result, key_events) = press(Some(anchor()), |chrome| {
            reply_focus(chrome, None, "d");
            chrome.emit_before_reply("Runtime.evaluate", 1, flood());
        });
        assert!(
            matches!(result, Err(PerformError::Rejected(ref reason)) if reason == "focus_changed"),
            "a rejected input stays rejected: {result:?}"
        );
        assert_eq!(key_events, 0);

        let (result, key_events) = press(Some(anchor()), |chrome| {
            reply_focus(chrome, Some(anchor()), "d");
            chrome.emit_before_reply("Input.dispatchKeyEvent", 2, flood());
        });
        assert!(
            matches!(result, Err(PerformError::Uncertain(ref reason)) if reason == "CDP journal overflowed"),
            "{result:?}"
        );
        assert_eq!(key_events, 2);

        for (operation, dispatch) in [(PreparedOperation::Hover, 1), (PreparedOperation::Click, 2)]
        {
            let chrome = ScriptedChrome::start();
            chrome.reply("Runtime.evaluate", revalidated());
            chrome.emit_before_reply("Input.dispatchMouseEvent", dispatch, flood());
            let result = perform(
                &chrome.connect_raw(),
                input(operation),
                &InputCancellation::default(),
            );
            assert!(
                matches!(result, Err(PerformError::Uncertain(ref reason)) if reason == "CDP journal overflowed"),
                "{operation:?}: {result:?}"
            );
        }
    }

    #[test]
    fn a_long_event_stream_before_an_input_does_not_make_it_uncertain() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", revalidated());
        let client = chrome.connect_raw();
        for (method, params) in flood() {
            chrome.push_event(method, params);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.cursor() < 10_100 {
            assert!(Instant::now() < deadline, "flood was not recorded");
            client.wait_for_journal_change(client.cursor(), Duration::from_millis(20));
        }
        assert!(client.lost_events_since(0));
        for operation in [PreparedOperation::Click, PreparedOperation::Hover] {
            let fact = perform(&client, input(operation), &InputCancellation::default());
            assert!(fact.is_ok(), "{operation:?}: {fact:?}");
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

    fn revalidated() -> Value {
        json!({"result":{"value":{"ok":true,"x":12.5,"y":20.25}}})
    }

    fn mouse_moves(chrome: &ScriptedChrome) -> Vec<Value> {
        chrome
            .commands()
            .into_iter()
            .filter(|(method, params)| {
                method == "Input.dispatchMouseEvent" && params["type"] == "mouseMoved"
            })
            .map(|(_, params)| params)
            .collect()
    }

    fn hover_with(chrome: &ScriptedChrome) -> Result<PerformFact, PerformError> {
        perform(
            &chrome.connect_raw(),
            input(PreparedOperation::Hover),
            &InputCancellation::default(),
        )
    }

    #[test]
    fn hover_revalidates_and_moves_the_pointer_once_to_the_revalidated_point() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", revalidated());

        assert_eq!(
            hover_with(&chrome).unwrap(),
            PerformFact {
                readback: None,
                readback_matches: None,
                suboperations: vec!["mouse_move".into()]
            }
        );
        let methods: Vec<_> = chrome
            .commands()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(methods, ["Runtime.evaluate", "Input.dispatchMouseEvent"]);
        assert_eq!(
            mouse_moves(&chrome),
            [json!({"type":"mouseMoved","x":12.5,"y":20.25})]
        );
    }

    #[test]
    fn rejected_hover_revalidation_never_moves_the_pointer() {
        let mut rejections: Vec<_> = [
            "target_missing",
            "document_changed",
            "disabled",
            "not_in_view",
            "covered",
            "cross_origin_frame",
        ]
        .into_iter()
        .map(|reason| {
            (
                json!({"result":{"value":{"ok":false,"reason":reason}}}),
                reason,
            )
        })
        .collect();
        rejections.push((
            json!({"result":{"value":{"ok":true}}}),
            "missing target geometry",
        ));
        for (revalidation, reason) in rejections {
            let chrome = ScriptedChrome::start();
            chrome.reply("Runtime.evaluate", revalidation);
            let result = hover_with(&chrome);
            assert!(
                matches!(result, Err(PerformError::Rejected(ref actual)) if actual == reason),
                "{reason}: {result:?}"
            );
            assert!(mouse_moves(&chrome).is_empty(), "{reason}");
        }
    }

    #[test]
    fn hover_that_cdp_rejects_is_rejected() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", revalidated());
        chrome.reject("Input.dispatchMouseEvent");

        assert!(matches!(
            hover_with(&chrome),
            Err(PerformError::Rejected(_))
        ));
        assert_eq!(mouse_moves(&chrome).len(), 1);
    }

    #[test]
    fn hover_not_queued_is_not_performed() {
        let chrome = ScriptedChrome::start();
        let cancellation = InputCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            perform(
                &chrome.connect_raw(),
                input(PreparedOperation::Hover),
                &cancellation,
            ),
            Err(PerformError::NotPerformed(_))
        ));
        assert!(chrome.commands().is_empty());

        let target = revalidated()["result"]["value"].clone();
        assert!(matches!(
            hover(&chrome.connect_raw(), &target, &cancellation),
            Err(PerformError::NotPerformed(_))
        ));
        let disconnected = ScriptedChrome::start();
        disconnected.reject("Page.enable");
        let client = disconnected.connect_observation();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !client.is_disconnected() {
            assert!(Instant::now() < deadline, "connection did not disconnect");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            hover(&client, &target, &InputCancellation::default()),
            Err(PerformError::NotPerformed(reason)) if reason == "connection is disconnected"
        ));
        assert!(mouse_moves(&chrome).is_empty());
        assert!(mouse_moves(&disconnected).is_empty());
    }

    #[test]
    fn hover_sent_without_an_answer_is_uncertain() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", revalidated());
        chrome.silence("Input.dispatchMouseEvent");
        let cancellation = InputCancellation::default();
        let canceller = cancellation.clone();
        let started = Instant::now();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while mouse_moves(&chrome).is_empty() {
                    assert!(started.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(2));
                }
                canceller.cancel();
            });
            perform(
                &chrome.connect_raw(),
                input(PreparedOperation::Hover),
                &cancellation,
            )
        });

        // The caller and the connection worker both observe the cancellation once the
        // move is sent; whichever notices first names the reason.
        assert!(
            matches!(
                result,
                Err(PerformError::Uncertain(ref reason))
                    if ["cancelled while awaiting CDP reply", "cancelled after send"]
                        .contains(&reason.as_str())
            ),
            "{result:?}"
        );
        assert_eq!(mouse_moves(&chrome).len(), 1);
    }

    #[test]
    fn hover_interrupted_by_disconnect_after_sending_is_uncertain() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", revalidated());
        chrome.disconnect_on("Input.dispatchMouseEvent");

        let result = hover_with(&chrome);
        assert!(
            matches!(result, Err(PerformError::Uncertain(_))),
            "{result:?}"
        );
        assert_eq!(mouse_moves(&chrome).len(), 1);
    }

    #[test]
    fn scrolling_skips_revalidation_and_scrolls_in_its_direction() {
        for (operation, step, suboperation) in [
            (PreparedOperation::ScrollUp, "scrollBy(0,-1 *", "scroll_up"),
            (
                PreparedOperation::ScrollDown,
                "scrollBy(0,1 *",
                "scroll_down",
            ),
        ] {
            let chrome = ScriptedChrome::start();
            let fact = perform(
                &chrome.connect_raw(),
                input(operation),
                &InputCancellation::default(),
            )
            .unwrap();
            assert_eq!(fact.suboperations, [suboperation]);
            let received = chrome.commands();
            assert_eq!(received.len(), 1, "{operation:?}");
            let expression = received[0].1["expression"].as_str().unwrap();
            assert!(expression.contains(step), "{operation:?}: {expression}");
        }
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
