use super::*;

/// A run of words that differ at rest.
struct Differs {
    shadow: std::sync::Arc<Shadow>,
    at: usize,
    len: usize,
    words: u32,
}

/// Nothing is in the air, and nothing is sent meanwhile (the submission
/// lock is held): compares every copy in the card's memory with its CPU
/// side, every few seconds.
pub(in crate::backend) fn at_rest(host: &Host) {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    if now < LAST.load(Ordering::Relaxed) + 4000 {
        return;
    }
    let began = std::time::Instant::now();
    if look(host).is_none() {
        tracing::warn!("up-copy check: the look at rest could not be made");
    }
    look_for_stale();
    tracing::info!(ms = began.elapsed().as_millis(), "up-copy check: the look at rest is done");
    LAST.store(now_ms(), Ordering::Relaxed);
}

/// At rest: copies of title memory, whatever reads them (meshes too), that
/// are not what title memory holds although nothing told that it changed: a
/// refresh will not bring them up to date.
fn look_for_stale() {
    const MOST: usize = 1 << 30;
    let mut by: BTreeMap<(u64, u64), (bool, u64, usize, Vec<String>)> = BTreeMap::new();
    let compared = crate::memory::shadow::stale_pages(MOST, &mut |shadow, page, words, (at, copy, title)| {
        let e = by.entry(shadow.span()).or_insert((shadow.carded(), 0, 0, Vec::new()));
        e.1 += words as u64;
        e.2 += 1;
        if e.3.len() < 4 {
            e.3.push(format!("{words} words in the 64 KiB at {page:#x} (the first at {at:#x}: the copy {copy:#x}, title memory {title:#x})"));
        }
    });
    tracing::info!(
        "up-copy check at rest: {} MB of copies held for unchanged compared with title memory, {} copies are stale",
        compared >> 20,
        by.len()
    );
    let mut by: Vec<_> = by.into_iter().collect();
    by.sort_by_key(|(_, e)| std::cmp::Reverse(e.1));
    for ((start, len), (carded, words, pages, some)) in by.into_iter().take(16) {
        tracing::warn!(
            "up-copy check at rest, stale: {start:#x}+{len:#x} ({}): {words} words in {pages} pages are not title memory's, and nothing \
             told of it: {}",
            if carded { "on the card" } else { "in PC memory" },
            some.join("; ")
        );
    }
}

/// Sends `list` (recorded by `f`) and waits for it.
pub(super) fn run(host: &Host, f: impl FnOnce(&ID3D12GraphicsCommandList)) -> Option<()> {
    let (allocator, list) = host.make_prelude()?;
    // SAFETY: objects of our own, waited for before they go.
    unsafe {
        allocator.Reset().ok()?;
        list.Reset(&allocator, None).ok()?;
        f(&list);
        list.Close().ok()?;
        let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
        let event = CreateEventW(None, false, false, None).ok()?;
        host.queue.ExecuteCommandLists(&[Some(list.cast().ok()?)]);
        let done = host.queue.Signal(&fence, 1).and_then(|()| fence.SetEventOnCompletion(1, event));
        if done.is_ok() {
            WaitForSingleObject(event, INFINITE);
        }
        let _ = CloseHandle(event);
        done.ok()
    }
}

/// The words that differ.
pub(super) fn look(host: &Host) -> Option<u64> {
    let pass = pass(host)?;
    let copies = crate::memory::shadow::carded_copies();
    // (copy, offset, bytes)
    let mut pieces: Vec<(std::sync::Arc<Shadow>, usize, usize)> = Vec::new();
    for shadow in &copies {
        // What has not gone up yet differs rightly.
        let dirty = shadow.card_dirty_now();
        let len = shadow.span().1 as usize & !3;
        for at in (0..len).step_by(PIECE) {
            let n = PIECE.min(len - at);
            if !dirty.iter().any(|&(a, b)| a < at + n && b > at) {
                pieces.push((shadow.clone(), at, n));
            }
        }
    }
    if pieces.is_empty() {
        return Some(0);
    }
    let size = (pieces.len() as u64 * ANSWER).next_multiple_of(256);
    let (found, answers) = host.shared_buffer(size)?;
    // SAFETY: a mapping of that size.
    unsafe { std::ptr::write_bytes(answers, 0, size as usize) };
    run(host, |list| {
        // SAFETY: an open list; live buffers (held by `pieces`).
        unsafe {
            list.SetComputeRootSignature(&pass.root);
            list.SetPipelineState(&pass.pso);
            list.SetComputeRootUnorderedAccessView(3, found.GetGPUVirtualAddress());
            let mut bound = 0;
            for (i, (shadow, at, n)) in pieces.iter().enumerate() {
                let Some((staging, ..)) = shadow.card_sides() else {
                    continue;
                };
                if bound != shadow.id() {
                    bound = shadow.id();
                    list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                    list.SetComputeRootShaderResourceView(2, staging.GetGPUVirtualAddress());
                }
                let at = *at as u32;
                let constants = [at, (*n / 4) as u32, at, at, (i as u64 * ANSWER) as u32];
                list.SetComputeRoot32BitConstants(0, 5, constants.as_ptr().cast(), 0);
                list.Dispatch((*n / 4).div_ceil(64) as u32, 1, 1);
            }
        }
    })?;
    // SAFETY: the mapping holds an answer for each; the card has run.
    let answers = unsafe { std::slice::from_raw_parts(answers as *const [u32; 4], pieces.len()) };
    let differs: Vec<Differs> = pieces
        .iter()
        .zip(answers)
        .filter(|(_, a)| a[0] > 0)
        .map(|((shadow, at, len), a)| Differs {
            shadow: shadow.clone(),
            at: *at,
            len: *len,
            words: a[0],
        })
        .collect();
    let bytes: u64 = pieces.iter().map(|p| p.2 as u64).sum();
    let words: u64 = differs.iter().map(|d| u64::from(d.words)).sum();
    tracing::info!(
        frame = crate::d3d12::frame::current(),
        "up-copy check at rest: {} copies, {} MB; {words} words in {} pieces of them differ from the CPU's side",
        copies.len(),
        bytes >> 20,
        differs.len()
    );
    if differs.is_empty() {
        return Some(0);
    }
    // Per copy, what stands where, of a few pieces.
    const SHOWN: usize = 64;
    let shown = &differs[..differs.len().min(SHOWN)];
    let (held, data) = host.shared_buffer((SHOWN * PIECE) as u64)?;
    run(host, |list| {
        for (i, d) in shown.iter().enumerate() {
            // SAFETY: an open list; live buffers, the ranges in them.
            unsafe { list.CopyBufferRegion(&held, (i * PIECE) as u64, &d.shadow.buffer, d.at as u64, d.len as u64) };
        }
    })?;
    let mut by: BTreeMap<(u64, u64), (u64, usize, Vec<String>)> = BTreeMap::new();
    for d in &differs {
        let e = by.entry(d.shadow.span()).or_default();
        e.0 += u64::from(d.words);
        e.1 += 1;
    }
    for (i, d) in shown.iter().enumerate() {
        let (start, _) = d.shadow.span();
        // SAFETY: the mapping; the copy into it has run.
        let card = unsafe { std::slice::from_raw_parts(data.add(i * PIECE).cast_const(), d.len) };
        let cpu = d.shadow.card_held(d.at, d.len);
        let mut title = vec![0u8; d.len];
        let mapped = read_into((start as usize + d.at) as *const u8, &mut title);
        // Of the words that differ: title memory holds the CPU side's
        // value, the card's, another one.
        let (mut as_cpu, mut as_card, mut other) = (0, 0, 0);
        let mut first = None;
        let (mut low, mut high) = (usize::MAX, 0);
        for at in (0..d.len & !3).step_by(4) {
            let word = |b: &[u8]| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
            let (c, h, t) = (word(card), word(&cpu), word(&title));
            if c == h {
                continue;
            }
            (low, high) = (low.min(at), at);
            first.get_or_insert((at, c, h, t));
            if t == h {
                as_cpu += 1;
            } else if t == c {
                as_card += 1;
            } else {
                other += 1;
            }
        }
        let Some((at, c, h, t)) = first else {
            continue;
        };
        if let Some(e) = by.get_mut(&d.shadow.span())
            && e.2.len() < 6
        {
            e.2.push(format!(
                "{} words in +{:#x}..{:#x} (title memory: {as_cpu} as the CPU's side, {as_card} as the card, {other} neither{}; first: \
                 card {c:#x}, CPU {h:#x}, title {t:#x}; {})",
                d.words,
                d.at + low,
                d.at + high + 4,
                if mapped { "" } else { ", not all mapped" },
                known(d.shadow.id(), d.at + at)
            ));
        }
    }
    let mut by: Vec<_> = by.into_iter().collect();
    by.sort_by_key(|(_, e)| std::cmp::Reverse(e.0));
    for ((start, len), (words, pieces, some)) in by.into_iter().take(16) {
        tracing::warn!(
            "up-copy check at rest: {start:#x}+{len:#x}: {words} words in {pieces} pieces are not the CPU side's: {}",
            some.join("; ")
        );
    }
    Some(words)
}
