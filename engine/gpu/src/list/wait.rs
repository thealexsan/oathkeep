use super::This;
#[cfg(windows)]
use super::{List, Part, detach::cut, fresh_list, host_list};
use crate::queue::Written;

/// A GPU wait for a memory value: the list's commands after it run when
/// the 32 or 64 bits at `address` compare as asked with `reference`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ValueWait {
    address: u64,
    comparison: u32,
    reference: u64,
    wide: bool,
}

impl ValueWait {
    /// Whether the memory holds what is waited for now.
    /// shortcut: the comparison codes are taken to be the comparison
    /// functions' (1 never .. 8 always, value against reference); one not
    /// known counts as met, and so does memory that is not mapped.
    ///
    /// The value is read through the kernel, which reports memory that is
    /// not there instead of faulting (as the copies of title memory are
    /// read, `crate::host::copy_mapped`). Asking Windows first whether the
    /// memory is committed (`VirtualQuery`) walks the page tables to the
    /// end of the region: the title keeps these values in a 1.1 GB
    /// allocation (0xd00000000), where one question took about 8 ms, and a
    /// queue asks at every submission and every signal, for each queue that
    /// stands at a wait (the title has five queues). That was
    /// 1.0-2.2 s of every cutscene frame on the thread that submits (runs
    /// 36-49: "between its submissions ... kernel"; 47 of 110 looks at
    /// that thread in run 49 found it in this question).
    /// `OATHKEEP_WAIT_ASK=1` asks first, as before.
    pub(crate) fn met(&self) -> bool {
        #[cfg(all(windows, feature = "diagnostics"))]
        let started = std::time::Instant::now();
        let met = self.holds();
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            use crate::copystat::{C, add};
            add(C::WaitChecks, 1);
            add(C::WaitCheckNs, started.elapsed().as_nanos() as u64);
        }
        met
    }

    fn holds(&self) -> bool {
        let bytes = if self.wide { 8 } else { 4 };
        if self.address == 0 {
            return true;
        }
        static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ASK.get_or_init(|| std::env::var_os("OATHKEEP_WAIT_ASK").is_some_and(|v| v == "1"))
            && !crate::texture::committed(self.address as *const u8, bytes)
        {
            return true;
        }
        // The low `bytes` bytes; the rest stays zero.
        let mut raw = [0u8; 8];
        if !crate::host::read_into(self.address as *const u8, &mut raw[..bytes]) {
            return true;
        }
        self.holds_for(u64::from_le_bytes(raw))
    }

    /// Where the value waited for is: address and bytes.
    #[cfg(windows)]
    pub(crate) fn place(&self) -> (usize, usize) {
        (self.address as usize, if self.wide { 8 } else { 4 })
    }

    /// Whether the wait is met with `value` at its address (its low half
    /// for a 32-bit wait).
    pub(crate) fn holds_for(&self, value: u64) -> bool {
        let (value, reference) = if self.wide {
            (value, self.reference)
        } else {
            (value & 0xFFFF_FFFF, self.reference & 0xFFFF_FFFF)
        };
        match self.comparison {
            1 => false,
            2 => value < reference,
            3 => value == reference,
            4 => value <= reference,
            5 => value > reference,
            6 => value != reference,
            7 => value >= reference,
            _ => true,
        }
    }
}

/// The waits lists recorded since their last reset, by list.
static VALUE_WAITS: std::sync::Mutex<Vec<(usize, ValueWait)>> = std::sync::Mutex::new(Vec::new());

/// Drops any unsubmitted value waits recorded for `this`.
pub(super) fn clear_for(this: This) {
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(list, _)| list != this as usize);
}

/// The waits of the submitted `lists` that are not met yet (taken from
/// them): what the console's GPU would stop at. The title orders work
/// between its queues this way (the title: the compute queue's
/// lighting passes wait for a value the graphics queue's depth pass writes
/// at the bottom of the pipe; run at once, they read a cleared depth
/// buffer and nothing was lit).
/// shortcut: a wait holds its whole list (and submission) back, not only
/// the commands after it.
pub(crate) fn unmet_waits(lists: &[This]) -> Vec<ValueWait> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut unmet = Vec::new();
    waits.retain(|&(list, wait)| {
        if !lists.iter().any(|&l| l as usize == list) {
            return true;
        }
        if !wait.met() {
            unmet.push(wait);
        }
        false
    });
    unmet
}

/// The waits each of the submitted `lists` recorded (taken from them),
/// met or not, at the list's first place in the submission.
#[cfg(windows)]
pub(crate) fn waits_by_list(lists: &[This]) -> Vec<Vec<ValueWait>> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut by = vec![Vec::new(); lists.len()];
    waits.retain(|&(list, wait)| {
        match lists.iter().position(|&l| l as usize == list) {
            Some(i) => {
                by[i].push(wait);
                false
            }
            None => true,
        }
    });
    by
}

fn wait_for(this: This, address: u64, comparison: u32, reference: u64, wide: bool) {
    let wait = ValueWait {
        address,
        comparison,
        reference,
        wide,
    };
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
        tracing::info!(
            address = format_args!("{address:#x}"),
            comparison,
            reference,
            wide,
            met = wait.met(),
            "a list waits for a memory value"
        );
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(host) = host_list(this) {
        crate::capture::op(std::ptr::from_ref(host) as usize, || {
            format!(
                "WAIT VALUE at {address:#x} comparison {comparison} reference {reference:#x} wide {wide} met {}",
                wait.met()
            )
        });
    }
    // The list is cut here, and the wait stands before what follows.
    #[cfg(windows)]
    if crate::queue::cuts() {
        cut(this, wait);
        return;
    }
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((this as usize, wait));
}
pub(super) extern "system" fn wait_value(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, false);
}
pub(super) extern "system" fn wait_value64(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, true);
}

/// The bottom-of-pipe value writes `(address, value, flush, flags)`, 32 and
/// 64 bits: done when the list has run, as the GPU would. Flag 1 writes the
/// GPU timestamp instead (64-bit form), flag 2 the system timestamp.
pub(super) extern "system" fn write32(this: This, address: u64, value: u32, _flush: u32, _flags: u32) {
    defer_write(this, address, Written::Value(u64::from(value)), 4);
    value_written(this, address, u64::from(value));
}
pub(super) extern "system" fn write64(this: This, address: u64, value: u64, _flush: u32, flags: u32) {
    const GPU_TIMESTAMP: u32 = 1;
    const SYSTEM_TIMESTAMP: u32 = 2;
    let what = match flags {
        GPU_TIMESTAMP => Written::GpuTimestamp,
        SYSTEM_TIMESTAMP => Written::SystemTimestamp,
        _ => Written::Value(value),
    };
    defer_write(this, address, what, 8);
    if let Written::Value(value) = what {
        value_written(this, address, value);
    }
}

/// The title wrote a value at the bottom of the pipe (not a timestamp, not
/// a query result): listed (diagnostics), and with `OATHKEEP_CUT_WRITES=1`
/// the host list is cut right behind it, so that the value is stored when
/// the card has run the list up to there, as on the console, and not when
/// the whole list has run. Lists of other queues wait for these values
/// (the title: the compute list that builds the depth pyramid
/// waits for one, and the meshlet culling of the same frame reads that
/// pyramid). Off by default: every such write then ends a host list, and
/// each part is a submission of its own.
fn value_written(this: This, address: u64, value: u64) {
    #[cfg(windows)]
    {
        if address == 0 || this.is_null() {
            return;
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = value;
        #[cfg(feature = "diagnostics")]
        if let Some(host) = host_list(this) {
            crate::capture::op(std::ptr::from_ref(host) as usize, || {
                format!("WRITE VALUE at {address:#x} value {value:#x}")
            });
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_CUT_WRITES").is_some_and(|v| v == "1"))
            || !crate::queue::cuts()
        {
            return;
        }
        // SAFETY: the title passes one of our lists; a list is recorded by
        // one thread at a time and is not submitted while it records.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(current) = list.host.as_ref() else {
            return;
        };
        let Some(fresh) = fresh_list() else {
            return;
        };
        current.close();
        if let Some(done) = list.host.replace(fresh) {
            // The part so far keeps its waits; nothing stands before the
            // next one but this part itself.
            let waits = std::mem::take(&mut list.gate);
            list.earlier.push(Part { waits, host: done });
        }
        if let Some(host) = list.host.as_ref() {
            list.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replay(host);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, address, value);
}

pub(super) fn defer_write(this: This, address: u64, what: Written, bytes: usize) {
    if address == 0 {
        return;
    }
    #[cfg(windows)]
    if let Some(list) = host_list(this) {
        list.defer_write(address as *mut u8, what, bytes);
        return;
    }
    let _ = this;
    // Without a host GPU nothing runs later: write now.
    what.write(address as *mut u8, bytes);
}
