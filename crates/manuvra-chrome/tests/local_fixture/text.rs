use super::*;
use serde_json::{Value, json};

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_source_animation_triggered_by_masks_is_withheld() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><style>@keyframes slide{from{transform:translateX(0)}to{transform:translateX(500px)}}#bare{display:inline-block}html:has(>div) #bare{animation:slide .1s linear infinite alternate}</style><p>Ready</p><span id='bare' aria-hidden='true'></span><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(
        observed
            .painted_text
            .as_ref()
            .unwrap()
            .viewport
            .painted_aria_hidden
            .contains("bare-shadow-secret")
    );
    let result = browser.capture_redacted(&["bare-shadow-secret".into()]);
    if let Ok(masked) = &result {
        eprintln!(
            "mask-triggered source animation proof {:?}",
            masked.redaction
        );
        if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("mask-triggered-animation.png"),
                &masked.screenshot.bytes,
            )
            .unwrap();
        }
    }
    browser.close().unwrap();
    assert!(
        matches!(result, Err(BrowserError::Control(message)) if message == "redaction_unverifiable")
    );
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_shadow_animation_cannot_publish_unfenced_masking() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><span id='host'></span><script>host.attachShadow({mode:'open'}).innerHTML='<style>@keyframes slide{from{transform:translateX(0)}to{transform:translateX(500px)}}#bare{display:block;animation:slide .1s linear infinite alternate}</style><span id=bare aria-hidden=true>bare-shadow-secret</span>'</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(
        observed
            .painted_text
            .as_ref()
            .unwrap()
            .viewport
            .painted_aria_hidden
            .contains("bare-shadow-secret")
    );
    let result = browser.capture_redacted(&["bare-shadow-secret".into()]);
    if let Ok(masked) = &result {
        eprintln!("shadow animation proof {:?}", masked.redaction);
        if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("shadow-animation-masked.png"),
                &masked.screenshot.bytes,
            )
            .unwrap();
        }
    }
    browser.close().unwrap();
    assert!(
        matches!(result, Err(BrowserError::Control(message)) if message == "redaction_unverifiable")
    );
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_shadow_text_with_reflected_paint_is_withheld() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><span id='bare' aria-hidden='true' style='font:32px monospace;display:inline-block;-webkit-box-reflect:below 20px'></span><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let painted = &observed.painted_text.as_ref().unwrap().viewport;
    assert!(painted.complete);
    assert_eq!(painted.painted_aria_hidden, "bare-shadow-secret");
    let masked = browser.capture_redacted(&["bare-shadow-secret".into()]);
    browser.close().unwrap();
    assert!(
        matches!(masked, Err(BrowserError::Control(message)) if message == "redaction_unverifiable"),
        "reflected classified paint cannot be covered by masks on the foreground ranges"
    );
}

const TEXT_FIXTURE: &str = include_str!("../../../../tests/browser/text.html");

fn snapshot(browser: &OwnedBrowser) -> Value {
    let observation = browser.observe().unwrap();
    let mut wire = serde_json::to_value(&observation).unwrap();
    wire["colors_complete"] = json!(observation.colors_complete);
    wire
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_inventory_follows_composed_ancestors_and_excludes_unpainted_content() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(TEXT_FIXTURE);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = snapshot(&browser);
    let inventory = &snapshot["painted_text"]["viewport"];
    assert_eq!(inventory["complete"], true, "{snapshot}");
    let hidden = inventory["painted_aria_hidden"]
        .as_str()
        .expect("painted aria-hidden inventory");
    for amount in [
        "$0.00", "$9.00", "$10.00", "$11.00", "$12.00", "$13.00", "$20.00", "$23.00", "$24.00",
    ] {
        assert!(
            hidden.lines().any(|line| line == amount),
            "missing {amount}: {snapshot}"
        );
    }
    for amount in [
        "$3.00", "$4.00", "$5.00", "$6.00", "$7.00", "$8.00", "$14.00", "$15.00", "$21.00",
        "$30.00",
    ] {
        assert!(
            !hidden.lines().any(|line| line == amount),
            "ineligible {amount}: {snapshot}"
        );
    }
    assert!(
        inventory["accessible"]
            .as_str()
            .unwrap()
            .contains("Accessible $1.00")
    );
    assert_eq!(
        snapshot["painted_text"]["dialogs"]["Month details"]["painted_aria_hidden"],
        "$20.00\n$24.00"
    );
    assert!(
        !snapshot["painted_text"]["dialogs"]["Month details"]["accessible"]
            .as_str()
            .unwrap()
            .contains("$21.00")
    );
    assert!(
        snapshot["dialog_texts"]["Month details"]
            .as_str()
            .unwrap()
            .contains("$21.00"),
        "legacy innerText retained"
    );
    for name in ["Hidden control", "Shadow hidden control"] {
        assert!(
            !snapshot["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == name)
        );
    }
    assert!(hidden.contains("Amount spaced"));
    assert!(
        hidden.contains("Segment start\nSegment end"),
        "normalized-empty painted segments must not change the searchable text: {snapshot}"
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn unsupported_painted_geometry_is_incomplete_without_changing_legacy_coverage() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><p aria-hidden='true' style='clip-path:circle(10px)'>Unverifiable amount</p>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = snapshot(&browser);
    assert_eq!(snapshot["painted_text"]["viewport"]["complete"], false);
    assert!(
        !snapshot["painted_text"]["viewport"]["painted_aria_hidden"]
            .as_str()
            .unwrap()
            .contains("Unverifiable amount")
    );
    assert_eq!(snapshot["coverage"]["viewport_complete"], true);
    assert_eq!(snapshot["coverage"]["gaps"], json!([]));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_is_bounded_independently_of_the_color_owner_limit() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let cells = (0..520)
        .map(|i| format!("<span aria-hidden='true'>a{i}</span>"))
        .collect::<String>();
    let server = FixtureServer::with_body(&format!(
        "<!doctype html><main style='font:1px system-ui;display:grid;grid-template-columns:repeat(30,15px)'>{cells}</main>"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let many = snapshot(&browser);
    assert_eq!(many["painted_text"]["viewport"]["complete"], true);
    assert!(
        many["painted_text"]["viewport"]["painted_aria_hidden"]
            .as_str()
            .unwrap()
            .contains("a519")
    );
    assert_eq!(many["colors_complete"], false);
    browser.close().unwrap();
    for (attributes, channel) in [
        ("aria-hidden='true'", "painted_aria_hidden"),
        ("", "accessible"),
    ] {
        let large = "x".repeat(8001);
        let server = FixtureServer::with_body(&format!(
            "<!doctype html><p>Ready</p><dialog open aria-label='Details' style='position:static'><span {attributes} style='font:1px system-ui'>{large}</span></dialog>"
        ));
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let limited = snapshot(&browser);
        for inventory in [
            &limited["painted_text"]["viewport"],
            &limited["painted_text"]["dialogs"]["Details"],
        ] {
            assert_eq!(inventory["complete"], false);
            assert_eq!(inventory[channel].as_str().unwrap().len(), 8000);
        }
        browser.close().unwrap();
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_respects_embedding_clips_and_native_normalization_failures() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for (body, complete, retained) in [
        (
            r#"<iframe aria-hidden="true" style="width:300px;height:100px;border:0;clip-path:inset(100%)" srcdoc="<p>Amount</p>"></iframe>"#,
            true,
            false,
        ),
        (
            r#"<div style="clip-path:inset(100%)"><iframe aria-hidden="true" srcdoc="<p>Amount</p>"></iframe></div>"#,
            true,
            false,
        ),
        (
            r#"<div style="height:1px;overflow:hidden"><iframe aria-hidden="true" style="margin-top:40px" srcdoc="<p>Amount</p>"></iframe></div>"#,
            true,
            false,
        ),
        (
            r#"<iframe aria-hidden="true" style="clip-path:circle(10px)" srcdoc="<p>Amount</p>"></iframe>"#,
            false,
            false,
        ),
        (
            r#"<p aria-hidden="true">Amount</p><script>window.OffscreenCanvas=undefined</script>"#,
            false,
            false,
        ),
        (
            r#"<div id="shadow" inert></div><script>shadow.attachShadow({mode:'open'}).innerHTML='<p aria-hidden=true>Amount</p>'</script>"#,
            true,
            false,
        ),
        (
            r#"<div id="shadow" style="opacity:0"></div><script>shadow.attachShadow({mode:'open'}).innerHTML='<p aria-hidden=true>Amount</p>'</script>"#,
            true,
            false,
        ),
        (
            r#"<div id="slotted"><span>Amount</span></div><script>slotted.attachShadow({mode:'open'}).innerHTML='<div aria-hidden=true style="height:1px;overflow:hidden"><div style="margin-top:40px"><slot></slot></div></div>'</script>"#,
            true,
            false,
        ),
        (
            r#"<script type="application/json" aria-hidden="true" style="display:block">"Amount"</script>"#,
            true,
            false,
        ),
    ] {
        let server =
            FixtureServer::with_body(&format!("<!doctype html><body><p>Ready</p>{body}</body>"));
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let observed = snapshot(&browser);
        let viewport = &observed["painted_text"]["viewport"];
        assert_eq!(viewport["complete"], complete, "{body}: {observed}");
        for channel in ["accessible", "painted_aria_hidden"] {
            assert_eq!(
                viewport[channel].as_str().unwrap().contains("Amount"),
                retained,
                "{body}: {observed}"
            );
        }
        browser.close().unwrap();
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_truncation_keeps_unicode_valid_without_breaking_default_observation() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let prefix = "x".repeat(7999);
    let server = FixtureServer::with_body(&format!(
        "<!doctype html><meta charset='utf-8'><p>Ready</p><span inert>I</span><span aria-hidden='true' style='font:1px system-ui'>{prefix}🔥</span>"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = snapshot(&browser);
    assert_eq!(observed["visible_text"], "Ready");
    assert_eq!(observed["painted_text"]["viewport"]["complete"], false);
    assert_eq!(
        observed["painted_text"]["viewport"]["painted_aria_hidden"],
        prefix
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn bare_shadow_and_assigned_text_follow_their_painted_parent() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><div id='bare' aria-hidden='true'></div><div id='assigned'>Assigned amount</div><div id='inert'>Inert assigned amount</div><script>
        bare.attachShadow({mode:'open'}).textContent='Bare shadow amount';
        assigned.attachShadow({mode:'open'}).innerHTML='<div aria-hidden=true><slot></slot><slot></slot></div>';
        inert.attachShadow({mode:'open'}).innerHTML='<div inert><slot></slot></div>';
        </script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = snapshot(&browser);
    let viewport = &observed["painted_text"]["viewport"];
    assert_eq!(viewport["complete"], true);
    let accessible = viewport["accessible"].as_str().unwrap();
    let hidden = viewport["painted_aria_hidden"].as_str().unwrap();
    assert!(hidden.contains("Bare shadow amount"));
    assert!(hidden.contains("Assigned amount"));
    assert_eq!(
        hidden
            .lines()
            .filter(|line| *line == "Assigned amount")
            .count(),
        1
    );
    assert!(!accessible.contains("Assigned amount"));
    assert!(!accessible.contains("Inert assigned amount"));
    assert!(!hidden.contains("Inert assigned amount"));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_bare_shadow_text_is_masked_in_the_screenshot() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let secret = "bare-shadow-secret";
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><div id='bare' aria-hidden='true' style='font:20px system-ui'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let plain = browser.capture().unwrap();
    let masked = browser.capture_redacted(&[secret.into()]).unwrap();
    browser.close().unwrap();
    if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("painted-shadow-secret.png"),
            &masked.screenshot.bytes,
        )
        .unwrap();
    }
    assert_eq!(
        masked
            .observation
            .painted_text
            .as_ref()
            .unwrap()
            .viewport
            .painted_aria_hidden,
        secret
    );
    assert!(masked.redaction.verifies(1));
    assert_eq!(masked.redaction.matched_values, 1);
    assert!(masked.redaction.mask_count >= 1);
    assert_ne!(plain.screenshot.bytes, masked.screenshot.bytes);
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_boxless_and_assigned_text_is_masked_in_the_screenshot() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for body in [
        "<span aria-hidden='true' style='display:contents'>painted-secret</span>",
        r#"<iframe aria-hidden='true' style='border:0;transform:translate(20px,20px)' srcdoc='<span>painted-secret</span>'></iframe>"#,
        "<style>html{margin:50px;filter:opacity(1)}</style><span aria-hidden='true'>painted-secret</span>",
        "<style>@keyframes spin{to{transform:rotate(360deg)}}#unrelated{position:fixed;right:0;top:0;animation:spin 1s linear infinite}</style><span id='unrelated'>Spinner</span><span aria-hidden='true'>painted-secret</span>",
        "<div id='assigned'>painted-secret</div><script>assigned.attachShadow({mode:'open'}).innerHTML='<span aria-hidden=true style=display:contents><slot></slot></span>'</script>",
    ] {
        let server = FixtureServer::with_body(&format!("<!doctype html><p>Ready</p>{body}"));
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let plain = browser.capture().unwrap();
        let masked = browser
            .capture_redacted(&["painted-secret".into()])
            .unwrap();
        browser.close().unwrap();
        assert!(
            masked
                .observation
                .painted_text
                .as_ref()
                .unwrap()
                .viewport
                .painted_aria_hidden
                .contains("painted-secret"),
            "{body}"
        );
        assert!(masked.redaction.verifies(1));
        assert_eq!(masked.redaction.matched_values, 1);
        assert!(masked.redaction.mask_count >= 1);
        assert_ne!(plain.screenshot.bytes, masked.screenshot.bytes);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_shadow_text_with_unsupported_mask_geometry_is_withheld() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for body in [
        "<dialog id='d' aria-label='Details'><div id='bare' aria-hidden='true'></div></dialog><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret';d.showModal()</script>",
        "<div id='p' popover><div id='bare' aria-hidden='true'></div></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret';p.showPopover()</script>",
        r#"<iframe aria-hidden='true' style='width:300px;height:100px;border:0;transform:scale(2);transform-origin:0 0' srcdoc="<div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>"></iframe>"#,
        "<style>html{zoom:2}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>html{transform:translate(50px,50px)}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>html{margin:50px;contain:paint}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>html{margin:50px;will-change:transform}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>[data-manuvra-mask]::after{content:'bare-shadow-secret';color:white;font:32px monospace}</style><span id='bare' aria-hidden='true'></span><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>@keyframes slide{from{transform:translateX(0)}to{transform:translateX(500px)}}#bare{animation:slide .1s linear infinite alternate}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>@keyframes slide{from{transform:translateX(0)}to{transform:translateX(500px)}}#bare{position:fixed;animation:slide .1s linear infinite alternate}</style><span id='bare' aria-hidden='true'></span><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<span id='bare'>bare-shadow-secret</span><script>bare.attachShadow({mode:'open'}).innerHTML='<span aria-hidden=true style=\"display:inline-block;filter:drop-shadow(0 40px 0 black)\"><slot></slot></span>'</script>",
        r#"<iframe aria-hidden='true' style='border:0;filter:drop-shadow(0 40px 0 black)' srcdoc="<span>bare-shadow-secret</span>"></iframe>"#,
        r#"<iframe aria-hidden='true' style='margin:100px;width:400px;height:200px;border:0;rotate:15deg' srcdoc="<span>bare-shadow-secret</span>"></iframe>"#,
        r#"<iframe aria-hidden='true' style='margin:100px;width:400px;height:200px;border:0;scale:2' srcdoc="<span>bare-shadow-secret</span>"></iframe>"#,
        r#"<iframe aria-hidden='true' style='margin:100px;width:400px;height:200px;border:0;zoom:2' srcdoc="<span>bare-shadow-secret</span>"></iframe>"#,
        "<span id='bare' aria-hidden='true' style='display:inline-block;margin-top:150px;margin-left:150px;font:32px monospace'></span><script>const shadow=bare.attachShadow({mode:'open'});shadow.textContent='bare-shadow-secret';const range=document.createRange();range.selectNodeContents(shadow.firstChild);const r=range.getBoundingClientRect();document.documentElement.style.transformOrigin=`${r.x+r.width/2}px ${r.y+r.height/2}px`;document.documentElement.style.transform='rotate(45deg)'</script>",
        "<div id='bare' aria-hidden='true' style='text-shadow:0 40px black'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<div id='bare' aria-hidden='true' style='filter:drop-shadow(0 40px 0 black)'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
        "<style>@keyframes grow{from{width:0}to{width:500px}}#row{display:flex}#sibling{flex:none;animation:grow .1s linear infinite alternate}</style><div id='row'><div id='sibling'>Spacer</div><div id='bare' aria-hidden='true'></div></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
    ] {
        let server = FixtureServer::with_body(&format!("<!doctype html><p>Ready</p>{body}"));
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert!(
            observed
                .painted_text
                .as_ref()
                .unwrap()
                .viewport
                .painted_aria_hidden
                .contains("bare-shadow-secret"),
            "{body}"
        );
        assert!(
            matches!(browser.capture_redacted(&["bare-shadow-secret".into()]), Err(BrowserError::Control(message)) if message == "redaction_unverifiable"),
            "{body}"
        );
        browser.close().unwrap();
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_normalized_and_split_text_with_paint_effects_is_withheld() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for (body, retained) in [
        (
            "<span id='bare' aria-hidden='true' style='text-shadow:0 40px black'></span><script>bare.attachShadow({mode:'open'}).textContent='bare   shadow   secret'</script>",
            "bare shadow secret",
        ),
        (
            "<span id='bare' aria-hidden='true'></span><script>bare.attachShadow({mode:'open'}).innerHTML='bare shadow <span style=\"filter:drop-shadow(0 40px 0 black)\">secret</span>'</script>",
            "bare shadow\nsecret",
        ),
    ] {
        let server = FixtureServer::with_body(&format!("<!doctype html><p>Ready</p>{body}"));
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(
            observed
                .painted_text
                .as_ref()
                .unwrap()
                .viewport
                .painted_aria_hidden,
            retained
        );
        let masked = browser.capture_redacted(&["bare shadow secret".into()]);
        browser.close().unwrap();
        assert!(
            matches!(masked, Err(BrowserError::Control(message)) if message == "redaction_unverifiable"),
            "{body}"
        );
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_normalized_shadow_text_masks_the_complete_raw_range() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><meta charset='utf-8'><p>Ready</p><div id='bare' aria-hidden='true' style='font:20px system-ui'></div><p id='result'>Mask pending</p><script>
const root = bare.attachShadow({mode:'open'});
root.textContent='🔥prefix   bare   shadow   secret   suffix';
const text=root.firstChild, masks=new Map();
new MutationObserver(records=>{
  for(const record of records) {
    for(const node of record.addedNodes) if(node.hasAttribute?.('data-manuvra-mask')) masks.set(node,node.getBoundingClientRect());
    for(const node of record.removedNodes) if(masks.has(node)) {
      const mask=masks.get(node);masks.delete(node);
      const range=document.createRange(), start=text.textContent.indexOf('bare');
      range.setStart(text,start);range.setEnd(text,start+'bare   shadow   secret'.length);
      const rect=range.getBoundingClientRect();
      const covered=mask.left<=rect.left && mask.top<=rect.top && mask.right>=rect.right && mask.bottom>=rect.bottom;
      result.textContent=`Complete normalized mask: ${covered}`;
    }
  }
}).observe(document.documentElement,{childList:true});
</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let captured = browser
        .capture_redacted(&["bare shadow secret".into()])
        .unwrap();
    assert!(
        captured
            .observation
            .painted_text
            .as_ref()
            .unwrap()
            .viewport
            .painted_aria_hidden
            .contains("bare shadow secret")
    );
    assert!(captured.redaction.verifies(1));
    assert_eq!(captured.redaction.matched_values, 1);
    assert!(captured.redaction.mask_count >= 1);
    assert!(has_line(
        &browser.observe().unwrap(),
        "Complete normalized mask: true"
    ));
    if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("normalized-shadow-secret.png"),
            &captured.screenshot.bytes,
        )
        .unwrap();
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_node_joined_shadow_text_is_withheld_when_assembly_is_unverifiable() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).innerHTML='First<span>Second</span>'</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed
            .painted_text
            .as_ref()
            .unwrap()
            .viewport
            .painted_aria_hidden,
        "First\nSecond"
    );
    assert!(
        matches!(browser.capture_redacted(&["First\nSecond".into()]), Err(BrowserError::Control(message)) if message == "redaction_unverifiable")
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn classified_masks_paint_opaque_rectangles_despite_page_styles() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for (name, css) in [
        ("clipped", "div{clip-path:inset(0 100% 0 0)!important}"),
        ("rounded", "div{border-radius:50%!important}"),
        ("transparent", "[data-manuvra-mask]{opacity:0!important}"),
        ("translucent", "[data-manuvra-mask]{opacity:.25!important}"),
        ("hidden", "[data-manuvra-mask]{visibility:hidden!important}"),
        (
            "masked",
            "div{mask-image:linear-gradient(transparent,transparent)!important}",
        ),
        ("blended", "div{mix-blend-mode:screen!important}"),
    ] {
        // SVG provides an independent opaque-paint oracle, unaffected by the page's div rules.
        let body = format!(
            r#"<!doctype html><style>{css}</style><p>Ready</p>
<span id='bare' aria-hidden='true' style='font:32px monospace;color:#b91c1c;position:relative;left:.25px;top:.25px;z-index:1'></span>
<script>
const root=bare.attachShadow({{mode:'open'}});root.textContent='bare-shadow-secret';
if(location.search) {{
  const range=document.createRange();range.selectNodeContents(root.firstChild);
  const ns='http://www.w3.org/2000/svg', panel=document.createElementNS(ns,'svg');
  panel.setAttribute('width',innerWidth);panel.setAttribute('height',innerHeight);
  Object.assign(panel.style,{{position:'fixed',left:'0',top:'0',zIndex:'2147483647',pointerEvents:'none'}});
  for(const rect of range.getClientRects()) {{
    const box=document.createElementNS(ns,'rect');
    box.setAttribute('x',Math.floor(rect.left));box.setAttribute('y',Math.floor(rect.top));
    box.setAttribute('width',Math.ceil(rect.right)-Math.floor(rect.left));
    box.setAttribute('height',Math.ceil(rect.bottom)-Math.floor(rect.top));
    box.setAttribute('fill','black');panel.append(box);
  }}
  document.documentElement.append(panel);
}}
</script>"#
        );
        let server = FixtureServer::with_body(&body);
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let plain = browser.capture().unwrap();
        let masked = browser
            .capture_redacted(&["bare-shadow-secret".into()])
            .unwrap();
        assert!(masked.redaction.verifies(1), "{name}");
        assert_eq!(masked.redaction.matched_values, 1, "{name}");
        browser
            .navigate(&format!("{}?reference=1", server.url()))
            .unwrap();
        let reference = browser.capture().unwrap();
        if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("mask-{name}.png")),
                &masked.screenshot.bytes,
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("mask-{name}-reference.png")),
                &reference.screenshot.bytes,
            )
            .unwrap();
        }
        assert_ne!(plain.screenshot.bytes, reference.screenshot.bytes, "{name}");
        assert!(
            masked.screenshot.bytes == reference.screenshot.bytes,
            "{name}: a verified mask must paint the complete opaque reference rectangle"
        );
        browser.close().unwrap();
    }
}
