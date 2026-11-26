use super::*;

/// Whether device-removed data is on (`OATHKEEP_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(feature = "diagnostics") && crate::switch::on("OATHKEEP_D3D12_DRED") && !dred_faults() && !dred_light())
}

/// Breadcrumbs and the faulting address, but no markers and no texts
/// (`OATHKEEP_D3D12_DRED=light`): which list a removed device stopped in, at
/// which of its commands, and the pipelines the list binds; formatting a text
/// for every command is what makes the whole of it slow.
pub(crate) fn dred_light() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("OATHKEEP_D3D12_DRED").is_some_and(|v| v == "light")
    })
}

/// Any of the three.
pub(crate) fn dred_any() -> bool {
    dred_on() || dred_light() || dred_faults()
}

/// Whether only the faulting address of a removed device is kept
/// (`OATHKEEP_D3D12_DRED=faults`): no breadcrumbs, no markers and no texts, so
/// it costs next to nothing and can stay on while the title is played.
pub(crate) fn dred_faults() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("OATHKEEP_D3D12_DRED").is_some_and(|v| v == "faults")
    })
}

/// The lists alive while device-removed data is on, by the number their name
/// ends in, with their marker texts and the pipelines they bind: a report made
/// on another thread than the one that submits has no lists of its own.
#[cfg(feature = "diagnostics")]
#[allow(clippy::type_complexity)]
pub(super) static LIST_NOTES: Mutex<
    std::collections::BTreeMap<
        u32,
        (
            std::sync::Weak<Mutex<Vec<Note>>>,
            std::sync::Weak<Mutex<std::collections::BTreeSet<String>>>,
            std::sync::Weak<Mutex<indirects::Indirects>>,
        ),
    >,
> = Mutex::new(std::collections::BTreeMap::new());

/// The number in the name of a list of ours ("victory host list 7", which the
/// debug layer ends with a note of its own in brackets).
#[cfg(feature = "diagnostics")]
pub(super) fn list_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("victory host list ")?;
    let digits = rest.split(' ').next()?;
    (rest.len() == digits.len() || rest[digits.len()..].starts_with(" (")).then(|| digits.parse().ok()).flatten()
}

/// Indirect mesh dispatches since the last frame line: the largest group
/// count a record asked for, and the groups asked in all (from the lists'
/// budget buffers, a run late).
pub(super) static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Indirect records that ran empty for want of their list's budget, and the
/// largest cost of one (told from 2^22 on), likewise.
pub(super) static BUDGET_NOTES: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether a list's indirect-work budget lives in the card's own memory
/// (`OATHKEEP_INDIRECT_CARD_BUDGET=0` keeps it in memory the CPU maps, as
/// before). The clamp charges it with atomics, at least two in a row for
/// every clamp; in mapped memory on a card of its own each of them
/// crosses the bus.
pub(super) fn card_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_INDIRECT_CARD_BUDGET").is_none_or(|v| v != "0"))
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`OATHKEEP_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `OATHKEEP_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
pub(super) fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("OATHKEEP_GPU_TIMING"))
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on), or for
/// the frame recorder (`OATHKEEP_RECORD`).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    #[cfg(feature = "diagnostics")]
    if crate::book::on()
        && let Ok(resource) = object.cast::<ID3D12Resource>()
    {
        let text = text();
        crate::book::made(&resource, || text.clone());
        return name_it(object, || text);
    }
    name_it(object, text);
}

pub(super) fn name_it(object: &impl Interface, text: impl FnOnce() -> String) {
    #[cfg(feature = "diagnostics")]
    if crate::replay::record::on() {
        let text = text();
        crate::replay::record::name(object, &text);
        if dred_any()
            && let Ok(object) = object.cast::<ID3D12Object>()
        {
            // SAFETY: a plain call on a live object.
            let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text)) };
        }
        return;
    }
    if dred_any()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

impl Host {
    pub(in crate::host) fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
                #[cfg(feature = "diagnostics")]
                text.push_str(&crate::book::at_loss(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            if dred_faults() {
                return text;
            }
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    let named = [
                        (!p.pCommandListDebugNameW.is_null()).then(|| p.pCommandListDebugNameW.to_string().unwrap_or_default()),
                        (!p.pCommandListDebugNameA.is_null()).then(|| {
                            std::ffi::CStr::from_ptr(p.pCommandListDebugNameA.cast()).to_string_lossy().into_owned()
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .next()
                    .unwrap_or_default();
                    text.push_str(&format!(" of \"{named}\""));
                    // Not one of the caller's: by the name.
                    #[cfg(feature = "diagnostics")]
                    let by_name = list_number(&named).and_then(|n| {
                        let all = LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner());
                        all.get(&n).map(|(notes, pipelines, _)| (notes.upgrade(), pipelines.upgrade()))
                    });
                    #[cfg(feature = "diagnostics")]
                    let indirects = run.map(|r| r.indirects.clone()).or_else(|| {
                        let n = list_number(&named)?;
                        LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner()).get(&n)?.2.upgrade()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    #[allow(clippy::type_complexity)]
                    let by_name: Option<(Option<()>, Option<std::sync::Arc<Mutex<std::collections::BTreeSet<String>>>>)> = None;
                    #[cfg(feature = "diagnostics")]
                    let noted = run.map(|r| r.notes.clone()).or_else(|| by_name.as_ref().and_then(|b| b.0.clone()));
                    let bound = run.map(|r| r.pipelines.clone()).or_else(|| by_name.as_ref().and_then(|b| b.1.clone()));
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = noted.map_or_else(Vec::new, |notes| {
                        let notes = notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    }
                    #[cfg(feature = "diagnostics")]
                    if let Some(indirects) = indirects {
                        text.push_str(&indirects_around(history, done as usize, &indirects));
                    }
                    // (Without markers a list that stopped midway says what
                    // it may have been at only by what it binds.)
                    if (done == 0 || markers.is_empty())
                        && let Some(bound) = bound
                    {
                        let pipelines = bound.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }
}

pub(super) fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
pub(super) fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
/// For a list that stopped at command `done`: what the indirect executions
/// there ran, from the one before it to the next barrier (four at most), and
/// what the heap slots their user data names hold now.
#[cfg(feature = "diagnostics")]
pub(super) fn indirects_around(history: &[D3D12_AUTO_BREADCRUMB_OP], done: usize, all: &indirects::Shared) -> String {
    let indirect = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_EXECUTEINDIRECT;
    let all = all.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    let mut shown = 0;
    let mesh = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH;
    for (at, op) in history.iter().enumerate().skip(done.saturating_sub(1)) {
        if shown == 4 || (at > done && *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER) {
            break;
        }
        if !indirect(op) && !mesh(op) {
            continue;
        }
        shown += 1;
        let direct = mesh(op);
        let nth = if direct { indirects::before(history, at, mesh) } else { indirects::before(history, at, indirect) };
        let Some((what, words)) = all.nth(nth, direct) else {
            continue;
        };
        text.push_str(&format!("; command {at} ran {what}"));
        for (word, slot, what) in crate::descriptor::host::describe_slots(words) {
            text.push_str(&format!(", word {word} slot {slot}: {what}"));
        }
    }
    text
}

pub(super) fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    // From just before the command the GPU had not finished: the first
    // twelve after the barrier named draws long done when a list of 79
    // mesh draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

