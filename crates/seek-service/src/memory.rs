//! What the service's memory is made of, for `seek memory`: a counting allocator (so the heap can be compared
//! with what Windows reports) and a log of which code last used each index.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use seek_core::DeviceId;
use seek_ipc::{DeviceMemory, MemoryReport};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::state::State;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(n: usize) {
    let now = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// The system allocator, counting the bytes handed out.
pub struct CountingAlloc;

// SAFETY: every call goes straight to `System`; the counters do not touch the memory.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            grew(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            grew(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            grew(new);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

/// Who used which index, and what idle unloading did about it.
#[derive(Default)]
pub struct UsageLog {
    pub last_by: HashMap<DeviceId, &'static str>,
    pub loads: HashMap<DeviceId, u32>,
    pub unloads: HashMap<DeviceId, u32>,
    /// Why the last attempt to unload a device failed; gone once one works.
    pub refused: HashMap<DeviceId, String>,
    /// How often each piece of code asked for indexes, since the service started.
    pub touches: HashMap<&'static str, u64>,
    pub idle_checks: u64,
}

/// (working set, private, peak working set) of this process, in bytes.
fn process_memory() -> (u64, u64, u64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `c` is a valid buffer of the size given, and the pseudo handle needs no closing.
    let ok = unsafe {
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb)
    };
    if ok == 0 {
        return (0, 0, 0);
    }
    (c.WorkingSetSize as u64, c.PrivateUsage as u64, c.PeakWorkingSetSize as u64)
}

impl State {
    pub fn memory_report(&self) -> MemoryReport {
        let now = Instant::now();
        let (working_set, private_bytes, peak_working_set) = process_memory();
        let names: Vec<(DeviceId, String)> = {
            let reg = self.registry.read().unwrap();
            reg.devices.iter().map(|d| (d.id, d.display_name.clone())).collect()
        };
        let used = self.last_used.lock().unwrap().clone();
        let usage = self.usage.lock().unwrap();
        let indexes: HashMap<DeviceId, _> = self.indexes.read().unwrap().clone();
        let mut devices = Vec::new();
        for (id, name) in names {
            let memory = indexes.get(&id).map(|ix| {
                let ix = ix.read().unwrap();
                (ix.len() as u64, ix.heap_columns())
            });
            let (rows, columns) = memory.clone().unwrap_or_default();
            devices.push(DeviceMemory {
                name,
                in_memory: memory.is_some(),
                on_disk_only: self.parked.lock().unwrap().contains_key(&id),
                rows,
                bytes: columns.iter().map(|(_, b)| *b as u64).sum(),
                columns: columns.into_iter().map(|(n, b)| (n.to_string(), b as u64)).collect(),
                idle_secs: used.get(&id).map(|t| now.duration_since(*t).as_secs()),
                last_used_by: usage.last_by.get(&id).map(|s| s.to_string()),
                loads: usage.loads.get(&id).copied().unwrap_or(0),
                unloads: usage.unloads.get(&id).copied().unwrap_or(0),
                refused: usage.refused.get(&id).cloned(),
            });
        }
        let mut touches: Vec<(String, u64)> = usage.touches.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        touches.sort_by_key(|t| std::cmp::Reverse(t.1));
        MemoryReport {
            working_set,
            private_bytes,
            peak_working_set,
            heap_live: LIVE.load(Ordering::Relaxed) as u64,
            heap_peak: PEAK.load(Ordering::Relaxed) as u64,
            idle_unload_secs: self.cfg.idle_unload.map(|d| d.as_secs()),
            idle_checks: usage.idle_checks,
            devices,
            touches,
            journal_bytes: self.journals.lock().unwrap().values().map(|j| j.heap_bytes() as u64).sum(),
            mask_bytes: self
                .hidden_masks
                .lock()
                .unwrap()
                .values()
                .map(|m| m.mask.as_ref().map_or(0, |v| v.len() as u64))
                .sum(),
            totals_bytes: self.space_totals.lock().unwrap().iter().map(|(_, t)| t.heap_bytes() as u64).sum(),
            stored_result_bytes: crate::server::stored_result_bytes(),
        }
    }
}
