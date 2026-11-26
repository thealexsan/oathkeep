//! Periodic per-frame diagnostic log output.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::memory::log_memory;
use super::timing::{
    COMMANDS_BY_WORK, COMPARED, LONGEST, RANGES, RECORDERS, SUBMITTERS, WAITS_BY_WORK, WHOLE,
    page_faults, process_times,
};
use super::{C, COUNTERS, barrier_test, taken};

/// Logs the counters per frame, and the ranges that took most bytes. The
/// counters run from line to line, so they are divided by the frames
/// presented since the last line, not by `frames` (the frames of the last
/// second or more: a 4 s frame after an unlogged 4 s frame showed two
/// frames' counts as one's).
pub(crate) fn log(frames: f64) {
    let counted = taken(C::Frames as usize);
    let frames = if counted > 0 { counted as f64 } else { frames };
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = (0..COUNTERS).map(|i| taken(i) as f64 / frames).collect();
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed, {:.1} changes the guard did not see",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
        c[C::TexHot as usize],
    );
    tracing::info!(
        "recording per frame ({frames:.0} frames): {:.0} list calls {:.1} ms (all recording \
         threads); {:.1} ms waiting for pipelines; dynamic slots: {:.0} tables asked, {:.0} \
         refused, {:.1} ms searching; notes {:.1} ms; shader-written ranges: {:.0} steps in \
         {:.1} submissions, snapshots {:.1} ms, {:.1} MB compared back; sparse reads {:.1} ms; \
         guards {:.1} ms; {:.1} MB copied whole (memory not watched); {:.1} frame waits \
         sleeping {:.1} ms (all threads); ranges of shaders that cannot store to buffers: \
         {:.0} steps {:.1} MB left out",
        c[C::RecordCalls as usize],
        c[C::RecordNs as usize] / 1e6,
        ms(c[C::PipelineWaitUs as usize]),
        c[C::DynamicAsked as usize],
        c[C::DynamicRefused as usize],
        c[C::DynamicNs as usize] / 1e6,
        c[C::NoteNs as usize] / 1e6,
        c[C::GpuWrites as usize],
        c[C::BackSubmissions as usize],
        ms(c[C::SnapshotUs as usize]),
        mb(c[C::BackCompared as usize]),
        ms(c[C::SparseUs as usize]),
        ms(c[C::GuardUs as usize]),
        mb(c[C::WholeBytes as usize]),
        c[C::PaceWaits as usize],
        ms(c[C::PaceUs as usize]),
        c[C::GpuWritesLeftOut as usize],
        mb(c[C::GpuWritesLeftOutBytes as usize]),
    );
    let [together, written, mapped] =
        [0, 1, 2].map(|n| crate::memory::watch::ALIASES[n].load(Relaxed));
    tracing::info!(
        "guards over several views of a page, since the start: {together} blocks guarded with their other views, \
         {written} views opened by a write through another, {mapped} ranges opened because a view of them was mapped"
    );
    tracing::info!(
        "waits for the write trackers' locks per frame, all threads (number, time): {}",
        crate::memory::watch::lock_waits(frames)
    );
    let [copies, blocks, opened, stores, unknown, full, stayed, copied_same, records] =
        [0, 1, 2, 3, 4, 5, 6, 7, 8].map(|n| crate::memory::title_write::DONE[n].load(Relaxed));
    tracing::info!(
        "the title's writes beside the guards, since the start: {copies} copies (OATHKEEP_COPY_BESIDE=1), {blocks} blocks, {opened} opened; \
         {stores} stores (OATHKEEP_STORE_BESIDE=1), {unknown} not understood, {full} blocks opened for too many; left out for changing nothing: {stayed} stores, {copied_same} parts of copies, {records} copies of descriptors; \
         the busiest stores (left out/changed): {}",
        crate::memory::title_write::busiest()
    );
    // Where the wall clock of a frame goes: the phases of a frame, the
    // processor time of the whole process, and what each submitting and
    // recording thread did.
    static CPU: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let (user, kernel) = process_times();
    let (user, kernel) = {
        let mut last = CPU.lock().unwrap_or_else(|e| e.into_inner());
        let spent = (user.saturating_sub(last.0), kernel.saturating_sub(last.1));
        *last = (user, kernel);
        (spent.0 as f64 / 1e3 / frames, spent.1 as f64 / 1e3 / frames)
    };
    tracing::info!(
        "frame account per frame: after a present {:.1} ms until the first list call and {:.1} ms \
         until the first submission; {:.1} ms from the last submission to the present call; \
         lists reset in {:.1} ms; the process ran {user:.0} ms user + {kernel:.0} ms kernel on \
         all threads; what shaders wrote: {:.1} ms finding {:.0} pieces that differ, {:.1} ms \
         putting them back; texture uploads: {:.0} recorded, {:.0} left out; {:.1} held \
         submissions run at the present with their waits not met; waits for memory values: \
         {:.0} looks {:.1} ms; {:.0} host lists made {:.1} ms; {:.0} parts of held submissions \
         ran alone that the next part could have run with",
        ms(c[C::UntilRecordUs as usize]),
        ms(c[C::UntilSubmitUs as usize]),
        ms(c[C::TailUs as usize]),
        c[C::ResetNs as usize] / 1e6,
        ms(c[C::BackScanUs as usize]),
        c[C::BackDiffer as usize],
        ms(c[C::BackSyncUs as usize]),
        c[C::TexUploads as usize],
        c[C::TexSkipped as usize],
        c[C::Forced as usize],
        c[C::WaitChecks as usize],
        c[C::WaitCheckNs as usize] / 1e6,
        c[C::HostLists as usize],
        c[C::HostListNs as usize] / 1e6,
        c[C::PartsTogether as usize],
    );
    // What is copied whole, largest first: memory nobody tells us about.
    let whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut whole: Vec<_> = whole.into_iter().flatten().collect();
    whole.sort_by_key(|&((_, len), (n, _))| std::cmp::Reverse(len as u64 * n));
    tracing::info!(
        "  copied whole (memory not watched): {:.1} ms per frame in {} ranges; the largest follow",
        ms(c[C::WholeUs as usize]),
        whole.len()
    );
    for ((src, len), (n, us)) in whole.into_iter().take(6) {
        tracing::info!(
            "  copied whole from {src:#x}+{len:#x}: {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            (len as u64 * n) as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
    // The waiting for the card: many short waits or a few long ones, how
    // much of it is for submissions the title gets nothing back from, and
    // the longest one.
    let longest = std::mem::take(&mut *LONGEST.lock().unwrap_or_else(|e| e.into_inner()));
    tracing::info!(
        "  waiting for the card per frame: {:.1} submissions under 2 ms ({:.1} ms), {:.1} of 2-8 ms \
         ({:.1} ms), {:.1} of 8-32 ms ({:.1} ms), {:.1} longer ({:.1} ms); {:.1} submissions hand \
         nothing back to the title ({:.1} ms of the waiting); the longest since the last line: \
         {:.1} ms, {}",
        c[C::CardUnder2 as usize],
        ms(c[C::CardUnder2Us as usize]),
        c[C::Card2To8 as usize],
        ms(c[C::Card2To8Us as usize]),
        c[C::Card8To32 as usize],
        ms(c[C::Card8To32Us as usize]),
        c[C::CardOver32 as usize],
        ms(c[C::CardOver32Us as usize]),
        c[C::NothingBack as usize],
        ms(c[C::NothingBackUs as usize]),
        longest.0 as f64 / 1e3,
        longest.1
    );
    let by = WAITS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut by: Vec<_> = by.into_iter().flatten().collect();
    by.sort_by_key(|&(_, (_, us))| std::cmp::Reverse(us));
    for (what, (n, us)) in by.into_iter().take(12) {
        tracing::info!(
            "  the card's time by what ran: {:.1} ms a frame in {:.1} submissions a frame: {}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            what.chars().take(700).collect::<String>()
        );
    }
    let by = COMMANDS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut by: Vec<_> = by.into_iter().flatten().collect();
    by.sort_by_key(|&(_, (_, us, _))| std::cmp::Reverse(us));
    // All of them together, and by their first word: the forty lines below
    // are the heaviest only (100 of 226 ms waited in run 102), and what the
    // commands do not account for is the layer's own lists (the card
    // copies' passes, preludes) and the waits themselves.
    if !by.is_empty() {
        let mut kinds: Vec<(String, u64, u64)> = Vec::new();
        for (what, (n, us, _)) in &by {
            let word = what.split(' ').next().unwrap_or("").to_owned();
            match kinds.iter_mut().find(|k| k.0 == word) {
                Some(k) => (k.1, k.2) = (k.1 + n, k.2 + us),
                None => kinds.push((word, *n, *us)),
            }
        }
        kinds.sort_by_key(|k| std::cmp::Reverse(k.2));
        let all: u64 = kinds.iter().map(|k| k.2).sum();
        let text: Vec<String> =
            kinds.iter().map(|(word, n, us)| format!("{word} {:.1} ms in {:.0}", *us as f64 / 1e3 / frames, *n as f64 / frames)).collect();
        tracing::info!(
            "  the card's time by command (timing run), all {} kinds together: {:.1} ms a frame; by first word: {}",
            by.len(),
            all as f64 / 1e3 / frames,
            text.join(", ")
        );
    }
    for (what, (n, us, longest)) in by.into_iter().take(40) {
        tracing::info!(
            "  the card's time by command (timing run): {:.2} ms a frame in {:.1} commands a frame, the longest {:.2} ms: {what}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            longest as f64 / 1e3
        );
    }
    tracing::info!(
        "  waits for every earlier shader write, per frame: {:.0} for the title's barrier groups, {:.0} \
         for its barriers on buffers; {:.0} indirect commands bounded (a dispatch of ours between two \
         such waits each); barrier experiment {}",
        c[C::WaitsForGroups as usize],
        c[C::WaitsForBuffers as usize],
        c[C::IndirectBounded as usize],
        barrier_test()
    );
    // What is compared after submissions, largest first, and how much of
    // it shaders had written.
    let compared = COMPARED.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut compared: Vec<_> = compared.into_iter().flatten().collect();
    compared.sort_by_key(|&((_, len), (n, _))| std::cmp::Reverse(len as u64 * n));
    tracing::info!("  compared after submissions: {} ranges; the largest follow", compared.len());
    for ((src, len), (n, written)) in compared.into_iter().take(8) {
        tracing::info!(
            "  compared {src:#x}+{len:#x}: {:.1}x/frame, {:.2} MB/frame, {:.3} MB/frame of it written back",
            n as f64 / frames,
            (len as u64 * n) as f64 / 1e6 / frames,
            written as f64 / 1e6 / frames
        );
    }
    let mut submitters = std::mem::take(&mut *SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner()));
    submitters.sort_by_key(|s| std::cmp::Reverse(s.1.submissions));
    for (id, s) in submitters.into_iter().take(4) {
        let per = |v: u64| v as f64 / 1e3 / frames;
        tracing::info!(
            "  thread {id} submitted {:.1}x/frame: between its submissions {:.1} ms (running \
             {:.1} ms user + {:.1} ms kernel; {:.1} ms in list calls), in them running {:.1} ms \
             user + {:.1} ms kernel, {:.1} ms waiting for the submission lock",
            s.submissions as f64 / frames,
            per(s.between),
            per(s.between_user),
            per(s.between_kernel),
            s.between_ours as f64 / 1e6 / frames,
            per(s.inside_user),
            per(s.inside_kernel),
            per(s.lock),
        );
    }
    let mut recorders = std::mem::take(&mut *RECORDERS.lock().unwrap_or_else(|e| e.into_inner()));
    recorders.sort_by_key(|r| std::cmp::Reverse(r.1));
    let busiest: Vec<String> = recorders
        .iter()
        .take(6)
        .map(|r| format!("thread {} {:.0} ms in {:.0} calls", r.0, r.1 as f64 / 1e6 / frames, r.2 as f64 / frames))
        .collect();
    tracing::info!("  list calls by thread: {} threads; {}", recorders.len(), busiest.join(", "));
    log_memory();
    let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut ranges: Vec<_> = ranges.into_iter().flatten().collect();
    ranges.sort_by_key(|&(_, (_, bytes, _, _))| std::cmp::Reverse(bytes));
    for ((src, len), (n, bytes, us, _)) in ranges.into_iter().take(10) {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        tracing::info!(
            "  copied from {src:#x}+{len:#x} (memory {kind:#x}): {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
}
