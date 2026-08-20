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
        match native_windows() {
            Ok(mut windows) => {
                self.last_error = None;
                self.retain_ax_only_windows(&mut windows);
                self.apply(windows)
            }
            Err(error) => {
                self.last_error = Some(error);
                Vec::new()
            }
        }
    }

    fn retain_ax_only_windows(&self, windows: &mut Vec<WindowSnapshot>) {
        let listed = windows
            .iter()
            .map(WindowSnapshot::target_id)
            .collect::<HashSet<_>>();
        for record in self
            .records
            .values()
            .filter(|record| record.present && !listed.contains(&record.descriptor.target_id))
        {
            let matches = crate::ax::application_window_bounds(record.snapshot.pid)
                .map(|bounds| {
                    bounds
                        .iter()
                        .filter(|bounds| crate::ax::same_bounds(**bounds, record.snapshot.bounds))
                        .count()
                })
                .unwrap_or(0);
            if matches == 1 {
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

fn native_windows() -> Result<Vec<WindowSnapshot>, String> {
    let windows = window_server_windows()?;
    let mut ax_bounds = HashMap::<i32, Vec<WindowBounds>>::new();
    for pid in windows
        .iter()
        .map(|window| window.pid)
        .collect::<HashSet<_>>()
    {
        if let Ok(bounds) = crate::ax::application_window_bounds(pid) {
            ax_bounds.insert(pid, bounds);
        }
    }
    let exact: Vec<WindowSnapshot> = windows
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
        .collect();
    Ok(exact)
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
        WindowSnapshot {
            pid,
            window_id,
            owner: "Fixture".to_owned(),
            title: Some(title.to_owned()),
            bounds: WindowBounds {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0,
            },
            is_on_screen: true,
        }
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
}
