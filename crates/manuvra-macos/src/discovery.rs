use manuvra_runtime::TargetDescriptor;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName,
    kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID, kCGWindowSharingState,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const CAPABILITIES: &[&str] = &[
    "common.click",
    "common.type",
    "common.press",
    "common.scroll",
    "observation.query",
    "observation.screenshot",
    "observation.tree",
    "observation.evidence",
    "raw.ax.get",
    "raw.ax.set",
    "raw.ax.perform",
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WindowSnapshot {
    pub pid: i32,
    pub window_id: u32,
    pub owner: String,
    pub title: Option<String>,
    pub bounds: WindowBounds,
    pub is_on_screen: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowRecord {
    pub descriptor: TargetDescriptor,
    pub snapshot: WindowSnapshot,
    pub present: bool,
}

#[derive(Default)]
pub(crate) struct DiscoveryState {
    next_generation: u64,
    records: HashMap<String, WindowRecord>,
    pub last_error: Option<String>,
}

impl DiscoveryState {
    pub fn new() -> Self {
        Self {
            next_generation: 1,
            ..Self::default()
        }
    }

    pub fn refresh_native(&mut self) -> Vec<TargetDescriptor> {
        match window_server_windows() {
            Ok(windows) => {
                self.last_error = None;
                let ax_bounds = collect_ax_bounds(
                    windows.iter().map(|window| window.pid).chain(
                        self.records
                            .values()
                            .filter(|record| record.present)
                            .map(|record| record.snapshot.pid),
                    ),
                );
                self.apply_discovered(&windows, &ax_bounds)
            }
            Err(error) => {
                self.last_error = Some(error);
                Vec::new()
            }
        }
    }

    fn apply_discovered(
        &mut self,
        window_server: &[WindowSnapshot],
        ax_bounds: &HashMap<i32, Vec<WindowBounds>>,
    ) -> Vec<TargetDescriptor> {
        // Presence stays with a still-living pid+window_id even when AX bounds
        // have not caught up. Generation increments only after a true gap.
        let mut listed = ax_agreed_windows(window_server, ax_bounds);
        self.keep_known_window_server_windows(window_server, &mut listed);
        self.retain_ax_only_windows(ax_bounds, &mut listed);
        self.apply(listed)
    }

    fn keep_known_window_server_windows(
        &self,
        window_server: &[WindowSnapshot],
        listed: &mut Vec<WindowSnapshot>,
    ) {
        let mut listed_ids = listed
            .iter()
            .map(WindowSnapshot::target_id)
            .collect::<HashSet<_>>();
        for snapshot in window_server {
            let target_id = snapshot.target_id();
            if listed_ids.contains(&target_id) {
                continue;
            }
            let Some(record) = self.records.get(&target_id) else {
                continue;
            };
            if record.present
                && record.snapshot.pid == snapshot.pid
                && record.snapshot.window_id == snapshot.window_id
            {
                listed_ids.insert(target_id);
                listed.push(snapshot.clone());
            }
        }
    }

    fn retain_ax_only_windows(
        &self,
        ax_bounds: &HashMap<i32, Vec<WindowBounds>>,
        windows: &mut Vec<WindowSnapshot>,
    ) {
        let listed = windows
            .iter()
            .map(WindowSnapshot::target_id)
            .collect::<HashSet<_>>();
        for record in self
            .records
            .values()
            .filter(|record| record.present && !listed.contains(&record.descriptor.target_id))
        {
            let matches = ax_bounds
                .get(&record.snapshot.pid)
                .map(|bounds| {
                    bounds
                        .iter()
                        .filter(|bounds| crate::ax::same_bounds(**bounds, record.snapshot.bounds))
                        .count()
                })
                .unwrap_or(0);
            if matches == 1 && !listed_window_claims_bounds(windows, &record.snapshot) {
                let mut snapshot = record.snapshot.clone();
                snapshot.is_on_screen = false;
                windows.push(snapshot);
            }
        }
    }

    pub fn record(&self, target_id: &str, generation: u64) -> Option<WindowRecord> {
        self.records
            .get(target_id)
            .filter(|record| record.present && record.descriptor.generation == generation)
            .cloned()
    }

    pub fn validated_record(&mut self, target_id: &str, generation: u64) -> Option<WindowRecord> {
        let cached = self.record(target_id, generation)?;
        match validate_cached_window(&cached.snapshot) {
            Ok(Some(snapshot)) => {
                self.last_error = None;
                let record = self.records.get_mut(target_id)?;
                record.snapshot = snapshot;
                Some(record.clone())
            }
            Ok(None) => {
                self.refresh_native();
                self.record(target_id, generation)
            }
            Err(error) => {
                self.last_error = Some(error);
                None
            }
        }
    }

    fn apply(&mut self, windows: Vec<WindowSnapshot>) -> Vec<TargetDescriptor> {
        let previously_present = self
            .records
            .iter()
            .filter(|(_, record)| record.present)
            .map(|(target_id, _)| target_id.clone())
            .collect::<HashSet<_>>();
        for record in self.records.values_mut() {
            record.present = false;
        }

        for snapshot in windows {
            let target_id = snapshot.target_id();
            match self.records.get_mut(&target_id) {
                Some(record) if previously_present.contains(&target_id) => {
                    record.snapshot = snapshot;
                    record.present = true;
                }
                _ => {
                    let generation = self.next_generation;
                    self.next_generation = self.next_generation.saturating_add(1);
                    self.records.insert(
                        target_id.clone(),
                        WindowRecord {
                            descriptor: TargetDescriptor {
                                target_id,
                                generation,
                                kind: "macos".to_owned(),
                                capabilities: CAPABILITIES
                                    .iter()
                                    .map(|capability| (*capability).to_owned())
                                    .collect(),
                            },
                            snapshot,
                            present: true,
                        },
                    );
                }
            }
        }

        let mut targets = self
            .records
            .values()
            .filter(|record| record.present)
            .map(|record| record.descriptor.clone())
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| left.target_id.cmp(&right.target_id));
        targets
    }
}

impl WindowSnapshot {
    fn target_id(&self) -> String {
        let digest = Sha256::digest(format!("{}\0{}", self.pid, self.window_id));
        let suffix = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("macos_{suffix}")
    }
}

fn ax_agreed_windows(
    windows: &[WindowSnapshot],
    ax_bounds: &HashMap<i32, Vec<WindowBounds>>,
) -> Vec<WindowSnapshot> {
    windows
        .iter()
        .filter(|window| {
            ax_bounds
                .get(&window.pid)
                .map(|bounds| {
                    bounds
                        .iter()
                        .any(|bounds| crate::ax::same_bounds(*bounds, window.bounds))
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn collect_ax_bounds(pids: impl IntoIterator<Item = i32>) -> HashMap<i32, Vec<WindowBounds>> {
    pids.into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|pid| {
            crate::ax::application_window_bounds(pid)
                .ok()
                .map(|bounds| (pid, bounds))
        })
        .collect()
}

fn listed_window_claims_bounds(windows: &[WindowSnapshot], snapshot: &WindowSnapshot) -> bool {
    windows.iter().any(|listed| {
        listed.pid == snapshot.pid && crate::ax::same_bounds(listed.bounds, snapshot.bounds)
    })
}

fn validate_cached_window(cached: &WindowSnapshot) -> Result<Option<WindowSnapshot>, String> {
    if let Some(snapshot) = window_server_window(cached.window_id)? {
        if snapshot.pid != cached.pid {
            return Ok(None);
        }
        return Ok(Some(snapshot));
    }
    let ax_bounds = crate::ax::application_window_bounds(cached.pid).unwrap_or_default();
    let matches = ax_bounds
        .iter()
        .filter(|bounds| crate::ax::same_bounds(**bounds, cached.bounds))
        .count();
    Ok((matches == 1).then(|| {
        let mut snapshot = cached.clone();
        snapshot.is_on_screen = false;
        snapshot
    }))
}

fn window_server_window(window_id: u32) -> Result<Option<WindowSnapshot>, String> {
    let array = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionIncludingWindow | CGWindowListOption::ExcludeDesktopElements,
        window_id,
    )
    .ok_or_else(|| "CGWindowListCopyWindowInfo returned no exact window list".to_owned())?;
    // SAFETY: CoreGraphics documents the returned array as containing
    // CFDictionary window records with CFString keys and CF property-list values.
    let array: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(array) };
    Ok(array.iter().find_map(|dictionary| {
        decode_window(&dictionary).filter(|snapshot| snapshot.window_id == window_id)
    }))
}

fn window_server_windows() -> Result<Vec<WindowSnapshot>, String> {
    let array = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .ok_or_else(|| "CGWindowListCopyWindowInfo returned no window list".to_owned())?;

    // SAFETY: CoreGraphics documents the returned array as containing
    // CFDictionary window records with CFString keys and CF property-list values.
    let array: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(array) };
    let own_pid = std::process::id() as i32;
    let mut windows = Vec::new();
    for dictionary in array.iter() {
        if let Some(snapshot) = decode_window(&dictionary)
            && snapshot.pid != own_pid
            && snapshot.bounds.width >= 2.0
            && snapshot.bounds.height >= 2.0
            && snapshot.owner != "Window Server"
            && snapshot.owner != "Dock"
        {
            windows.push(snapshot);
        }
    }
    Ok(windows)
}

fn decode_window(dictionary: &CFDictionary<CFString, CFType>) -> Option<WindowSnapshot> {
    let layer = number(dictionary, unsafe { kCGWindowLayer })?;
    let sharing = number(dictionary, unsafe { kCGWindowSharingState })?;
    if layer != 0 || sharing == 0 {
        return None;
    }
    Some(WindowSnapshot {
        pid: number(dictionary, unsafe { kCGWindowOwnerPID })?,
        window_id: number(dictionary, unsafe { kCGWindowNumber })?
            .try_into()
            .ok()?,
        owner: string(dictionary, unsafe { kCGWindowOwnerName })?,
        title: string(dictionary, unsafe { kCGWindowName }).filter(|title| !title.is_empty()),
        bounds: bounds(dictionary)?,
        is_on_screen: boolean(dictionary, unsafe { kCGWindowIsOnscreen }).unwrap_or(false),
    })
}

fn number(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i32> {
    dictionary.get(key)?.downcast::<CFNumber>().ok()?.as_i32()
}

fn string(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<String> {
    Some(
        dictionary
            .get(key)?
            .downcast::<CFString>()
            .ok()?
            .to_string(),
    )
}

fn boolean(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<bool> {
    Some(dictionary.get(key)?.downcast::<CFBoolean>().ok()?.as_bool())
}

fn bounds(dictionary: &CFDictionary<CFString, CFType>) -> Option<WindowBounds> {
    let value = dictionary.get(unsafe { kCGWindowBounds })?;
    // SAFETY: kCGWindowBounds is documented as a CGRect dictionary. The destination is
    // initialized by CoreGraphics only when the function returns true.
    let bounds_dictionary: &CFDictionary = unsafe { &*CFRetained::as_ptr(&value).as_ptr().cast() };
    let mut rect = CGRect::ZERO;
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds_dictionary), &mut rect) } {
        return None;
    }
    Some(WindowBounds {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: i32, window_id: u32, title: &str) -> WindowSnapshot {
        window_at(pid, window_id, title, 10.0, 20.0)
    }

    fn window_at(pid: i32, window_id: u32, title: &str, x: f64, y: f64) -> WindowSnapshot {
        WindowSnapshot {
            pid,
            window_id,
            owner: "Fixture".to_owned(),
            title: Some(title.to_owned()),
            bounds: WindowBounds {
                x,
                y,
                width: 300.0,
                height: 200.0,
            },
            is_on_screen: true,
        }
    }

    fn ax_of(window: &WindowSnapshot) -> HashMap<i32, Vec<WindowBounds>> {
        HashMap::from([(window.pid, vec![window.bounds])])
    }

    fn ws(window: &WindowSnapshot) -> &[WindowSnapshot] {
        std::slice::from_ref(window)
    }

    #[test]
    fn stable_window_keeps_generation_but_reappearance_gets_a_new_one() {
        let mut state = DiscoveryState::new();
        let first = state.apply(vec![window(10, 20, "first")]);
        let stable = state.apply(vec![window(10, 20, "changed")]);
        assert_eq!(first[0].generation, stable[0].generation);

        assert!(state.apply(Vec::new()).is_empty());
        let reappeared = state.apply(vec![window(10, 20, "third")]);
        assert!(reappeared[0].generation > stable[0].generation);
    }

    #[test]
    fn identity_includes_both_process_and_window_number() {
        let mut state = DiscoveryState::new();
        let targets = state.apply(vec![window(10, 20, "a"), window(11, 20, "b")]);
        assert_eq!(targets.len(), 2);
        assert_ne!(targets[0].target_id, targets[1].target_id);
    }

    #[test]
    fn known_window_keeps_generation_when_ax_bounds_temporarily_disagree() {
        // Living pid+window_id stays present while AX still reports the
        // pre-move frame. Snapshot bounds come from current WindowServer.
        let mut state = DiscoveryState::new();
        let original = window_at(10, 20, "first", 10.0, 20.0);
        let first = state.apply_discovered(ws(&original), &ax_of(&original));
        let target_id = first[0].target_id.clone();
        let generation = first[0].generation;

        let moved = window_at(10, 20, "first", 1727.0, 64.0);
        let during_mismatch = state.apply_discovered(ws(&moved), &ax_of(&original));
        assert_eq!(during_mismatch.len(), 1);
        assert_eq!(during_mismatch[0].target_id, target_id);
        assert_eq!(during_mismatch[0].generation, generation);
        let record = state
            .record(&target_id, generation)
            .expect("living WindowServer window must stay present");
        assert_eq!(record.snapshot.bounds.x, 1727.0);
        assert_eq!(record.snapshot.bounds.y, 64.0);

        let recovered = state.apply_discovered(ws(&moved), &ax_of(&moved));
        assert_eq!(recovered[0].target_id, target_id);
        assert_eq!(recovered[0].generation, generation);
    }

    #[test]
    fn unknown_window_without_ax_agreement_is_not_listed() {
        let mut state = DiscoveryState::new();
        let snapshot = window(10, 20, "first");
        assert!(
            state
                .apply_discovered(ws(&snapshot), &HashMap::new())
                .is_empty()
        );
    }

    #[test]
    fn window_server_absence_then_reappearance_is_replacement() {
        let mut state = DiscoveryState::new();
        let snapshot = window(10, 20, "first");
        let first = state.apply_discovered(ws(&snapshot), &ax_of(&snapshot));
        assert!(state.apply_discovered(&[], &HashMap::new()).is_empty());
        assert!(
            state
                .apply_discovered(ws(&snapshot), &HashMap::new())
                .is_empty()
        );
        let again = state.apply_discovered(ws(&snapshot), &ax_of(&snapshot));
        assert!(again[0].generation > first[0].generation);
    }

    #[test]
    fn ax_only_window_keeps_generation_when_absent_from_window_server() {
        let mut state = DiscoveryState::new();
        let snapshot = window(10, 20, "first");
        let first = state.apply_discovered(ws(&snapshot), &ax_of(&snapshot));
        let hidden = state.apply_discovered(&[], &ax_of(&snapshot));
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].target_id, first[0].target_id);
        assert_eq!(hidden[0].generation, first[0].generation);
        let record = state
            .record(&first[0].target_id, first[0].generation)
            .expect("unique AX match must keep the window");
        assert!(!record.snapshot.is_on_screen);
    }

    #[test]
    fn different_window_id_is_a_new_target_not_a_kept_move() {
        let mut state = DiscoveryState::new();
        let original = window(10, 20, "first");
        let first = state.apply_discovered(ws(&original), &ax_of(&original));
        let replacement = window(10, 21, "second");
        let after = state.apply_discovered(ws(&replacement), &ax_of(&replacement));
        assert_eq!(after.len(), 1);
        assert_ne!(after[0].target_id, first[0].target_id);
        assert!(
            state
                .record(&first[0].target_id, first[0].generation)
                .is_none()
        );
    }
}
