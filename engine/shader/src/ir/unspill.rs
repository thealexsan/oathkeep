use std::collections::{HashMap, HashSet};

use crate::gfx10::{self, Fields, Flow, Operand};

use super::effects::effects;
use super::{PARKED, Reg};

pub(super) fn unspill(
    list: &mut [gfx10::Instruction],
    flows: &[Flow],
    leaders: &[usize],
    wave64: bool,
    lanes: u8,
    vgprs: u16,
) -> Vec<(usize, u8, u8)> {
    /// `v_writelane_b32 vN, sX, K` / `v_readlane_b32 sX, vN, K`: (save, N, X, K).
    fn parked(inst: &gfx10::Instruction, fields: &Fields) -> Option<(bool, u8, u8, u32)> {
        let Fields::Valu(v) = fields else { return None };
        if v.abs != 0 || v.neg != 0 || v.opsel != 0 || v.clamp || v.omod != 0 || v.sdwa.is_some() || v.dpp.is_some() {
            return None;
        }
        let lane = |op: &Operand| match *op {
            Operand::Const(c @ 128..=191) => Some(u32::from(c) - 128),
            _ => None,
        };
        match (inst.name()?, v.dst?, v.src.as_slice()) {
            ("v_writelane_b32", Operand::Vector { reg, .. }, [Operand::Scalar { code, .. }, k]) if *code < 105 => {
                Some((true, reg, *code, lane(k)?))
            }
            ("v_readlane_b32", Operand::Scalar { code, .. }, [Operand::Vector { reg, .. }, k]) if code < 105 => {
                Some((false, *reg, code, lane(k)?))
            }
            _ => None,
        }
    }
    // VGPRs used by parking alone, the scalar codes the program uses, and
    // per instruction the scalar codes it writes.
    let mut stores = [false; 256];
    let mut other: [Option<usize>; 256] = [None; 256]; // the last such use
    let mut used = [false; 128];
    let mut parks: Vec<Option<(bool, u8, u8, u32)>> = Vec::with_capacity(list.len());
    let mut written: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut read: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    for inst in list.iter() {
        let Some(name) = inst.name() else { return Vec::new() };
        let fields = inst.fields(wave64);
        let park = parked(inst, &fields);
        let Ok(effects) = effects(inst, &fields, name, lanes, vgprs) else {
            return Vec::new();
        };
        let mut codes = Vec::new();
        let mut from = Vec::new();
        if let Some((reads, writes)) = effects {
            for &reg in &writes {
                if let Reg::S(c) = reg {
                    codes.push(c);
                }
            }
            for &reg in &reads {
                if let Reg::S(c) = reg {
                    from.push(c);
                }
            }
            // An M0-relative read (`v_movrels_b32`) names every VGPR from its
            // base up; the compiler indexes a small array with it, never the
            // lanes it parks scalars in, so it does not make those VGPRs
            // ordinary ones. (Relative writes still do.)
            let relative = name == "v_movrels_b32";
            let n_reads = reads.len();
            for (k, reg) in reads.into_iter().chain(writes).enumerate() {
                if relative && k < n_reads && matches!(reg, Reg::V(_)) {
                    continue;
                }
                match reg {
                    Reg::S(c) => used[usize::from(c)] = true,
                    Reg::V(r) if park.is_some_and(|p| p.1 == r) => stores[usize::from(r)] = true,
                    Reg::V(r) => other[usize::from(r)] = Some(parks.len()),
                    Reg::Scc => {}
                }
            }
        }
        parks.push(park);
        written.push(codes);
        read.push(from);
    }
    let leader = |i: usize| leaders.binary_search(&list[i].offset).is_ok();
    for reg in 0..256 {
        let Some(last) = other[reg] else { continue };
        let of = |p: &Option<(bool, u8, u8, u32)>| p.filter(|p| usize::from(p.1) == reg);
        let first = parks.iter().position(|p| of(p).is_some_and(|p| p.0));
        let late = first.is_some_and(|first| {
            let end = parks.iter().rposition(|p| of(p).is_some()).unwrap_or(first);
            let mut saved = Vec::new();
            last < first
                && !(first + 1..=end).any(leader)
                && parks[first..=end].iter().filter_map(of).all(|(save, _, _, lane)| {
                    if save {
                        saved.push(lane);
                    }
                    saved.contains(&lane)
                })
        });
        let from = if late { first.unwrap_or(0) } else { parks.len() };
        for park in &mut parks[..from] {
            if of(park).is_some() {
                *park = None;
            }
        }
    }
    if parks.iter().all(Option::is_none) {
        return Vec::new();
    }
    // Lane pairs (register, lower lane) that always move together: each
    // save of the lower lane is followed, in the same block and before its
    // register changes, by the save of the next register into the next
    // lane; each restore of the lower lane is followed at once by the
    // restore of the next lane into the next register.
    // The second save may come any number of instructions after the first (the
    // compiler puts loads and constants between the two: five instructions later
    // in the ped mesh programs). Only wave64 programs have pairs: a wave32 mask
    // is one dword, and a pair move of it with its neighbour would hide it.
    // instruction -> the instruction that completes its pair
    let partner = |i: usize| -> Option<usize> {
        let (save, reg, code, lane) = parks[i]?;
        if save {
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((true, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        } else {
            // The second restore may come a few instructions later (the
            // compiler schedules other work between the two): the pair is
            // restored at the first, so nothing between may read or write
            // the second register, write the first, or save into the lanes.
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((false, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.0 && p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                    || read[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        }
    };
    // The store each parking instruction uses, and what the stores are lanes
    // of: one per (VGPR, lane), or (`split`) one per life of it.
    let stores_of = |split: bool| -> (Vec<usize>, Vec<(u8, u32)>) {
        let slot = |i: usize| parks[i].map(|p| (p.1, p.3));
        // Instructions of one store, by the first of them.
        let mut first: Vec<usize> = (0..list.len()).collect();
        if split {
            fn root(first: &mut [usize], mut i: usize) -> usize {
                while first[i] != i {
                    first[i] = first[first[i]];
                    i = first[i];
                }
                i
            }
            let at = |offset: usize| list.partition_point(|i| i.offset < offset);
            for save in (0..list.len()).filter(|&i| parks[i].is_some_and(|p| p.0)) {
                let mut seen = HashSet::new();
                let mut work = vec![save + 1];
                while let Some(from) = work.pop() {
                    for i in from..list.len() {
                        if slot(i).is_some() && slot(i) == slot(save) {
                            if parks[i].is_some_and(|p| p.0) {
                                break;
                            }
                            let (a, b) = (root(&mut first, save), root(&mut first, i));
                            first[a.max(b)] = a.min(b);
                        }
                        let mut go = |to: usize| {
                            if seen.insert(to) {
                                work.push(to);
                            }
                        };
                        match flows[i] {
                            Flow::Next if i + 1 < list.len() && leader(i + 1) => go(i + 1),
                            Flow::Next => continue,
                            Flow::Stop => {}
                            Flow::Jump(t) => go(at(t)),
                            Flow::Branch(t) => {
                                go(at(t));
                                go(i + 1);
                            }
                        }
                        break;
                    }
                }
            }
            for i in 0..list.len() {
                first[i] = root(&mut first, i);
            }
        } else {
            let mut of: HashMap<(u8, u32), usize> = HashMap::new();
            for i in 0..list.len() {
                if let Some(s) = slot(i) {
                    first[i] = *of.entry(s).or_insert(i);
                }
            }
        }
        let mut stores: Vec<((u8, u32), usize)> = (0..list.len())
            .filter(|&i| first[i] == i)
            .filter_map(|i| Some((slot(i)?, i)))
            .collect();
        stores.sort_unstable();
        let number = |i: usize| stores.iter().position(|s| s.1 == first[i]);
        let of = (0..list.len())
            .map(|i| parks[i].and_then(|_| number(i)).unwrap_or(usize::MAX))
            .collect();
        (of, stores.into_iter().map(|s| s.0).collect())
    };
    // The stores that move as pairs (lower, upper) and every store's code;
    // none if the spare codes do not suffice.
    let plan = |split: bool| -> Option<(Vec<usize>, Vec<(usize, usize)>, Vec<u8>)> {
        let (mut of, stores) = stores_of(split);
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for (n, &(reg, lane)) in stores.iter().enumerate() {
            if !wave64 || pairs.iter().any(|&(_, upper)| upper == n) {
                continue;
            }
            let lower: Vec<usize> = (0..list.len()).filter(|&i| of[i] == n).collect();
            let partners: Vec<Option<usize>> = lower.iter().map(|&i| partner(i)).collect();
            let mut distinct: Vec<usize> = partners.iter().flatten().copied().collect();
            distinct.sort_unstable();
            distinct.dedup();
            let Some(upper) = distinct.first().map(|&j| of[j]) else {
                continue;
            };
            if stores[upper] == (reg, lane + 1)
                && partners.iter().all(Option::is_some)
                && distinct.len() == lower.len()
                && distinct.iter().all(|&j| of[j] == upper)
                && of.iter().filter(|&&o| o == upper).count() == lower.len()
            {
                pairs.push((n, upper));
            }
        }
        // Lives of one lane that move alike share a code, as they share the lane.
        let role = |n: usize| {
            let (lower, upper) = (pairs.iter().any(|p| p.0 == n), pairs.iter().any(|p| p.1 == n));
            (stores[n], u8::from(lower) + 2 * u8::from(upper))
        };
        let mut shared: Vec<_> = (0..stores.len()).map(role).collect();
        let place: Vec<usize> = {
            let all = shared.clone();
            shared.sort_unstable();
            shared.dedup();
            all.iter().filter_map(|r| shared.binary_search(r).ok()).collect()
        };
        for o in of.iter_mut().filter(|o| **o != usize::MAX) {
            *o = place[*o];
        }
        for p in &mut pairs {
            *p = (place[p.0], place[p.1]);
        }
        pairs.sort_unstable();
        pairs.dedup();
        // Codes for the parked scalars: the trap temporaries (108..=123), then
        // user SGPRs the program never names, from the top, then the codes past
        // the machine's (`PARKED`..=254; 255 stays unused so that the code after a
        // parked one always exists). A material resolve program parks up to 63
        // scalars and leaves about 20 machine codes unused.
        let past = usize::from(PARKED);
        let mut free = [false; 256];
        for c in (0..105).chain(108..=123) {
            free[c] = !used[c];
        }
        free[past..255].fill(true);
        let mut codes: Vec<Option<u8>> = vec![None; shared.len()];
        for &(lower, upper) in &pairs {
            let at = (108..123).chain((0..104).rev()).chain(past..254).find(|&c| free[c] && free[c + 1]);
            // Not enough spare codes: leave the program as it is.
            let c = at?;
            (free[c], free[c + 1]) = (false, false);
            codes[lower] = Some(c as u8);
            codes[upper] = Some(c as u8 + 1);
        }
        for code in codes.iter_mut().filter(|c| c.is_none()) {
            let c = (108..=123).chain((0..105).rev()).chain(past..255).find(|&c| free[c])?;
            free[c] = false;
            *code = Some(c as u8);
        }
        Some((of, pairs, codes.into_iter().flatten().collect()))
    };
    let Some(mut chosen) = plan(false) else { return Vec::new() };
    let lonely = |(of, pairs, _): &(Vec<usize>, Vec<(usize, usize)>, Vec<u8>)| {
        (0..list.len()).any(|i| {
            parks[i].is_some_and(|p| p.0)
                && partner(i).is_some()
                && !pairs.iter().any(|&(lower, upper)| of[i] == lower || of[i] == upper)
        })
    };
    if wave64
        && lonely(&chosen)
        && let Some(split) = plan(true)
        && split.1.len() > chosen.1.len()
    {
        chosen = split;
    }
    let (store, pairs, codes) = chosen;
    let put = |list: &mut [gfx10::Instruction], i: usize, encoding, opcode: u16, word: u32| {
        list[i] = gfx10::Instruction::new(list[i].offset, encoding, opcode, vec![word]);
    };
    // `s_mov_b32` / `s_mov_b64 dst, src` (SOP1, opcodes 3 and 4); `s_nop`. A code
    // from `PARKED` up does not fit the word: s0 stands in for it there.
    let field = |code: u8| if code < PARKED { u32::from(code) } else { 0 };
    let mov = |wide: bool, dst: u8, src: u8| 0xBE80_0000 | field(dst) << 16 | (3 + u32::from(wide)) << 8 | field(src);
    const NOP: u32 = 0xBF80_0000;
    let mut edits: Vec<(usize, Option<(bool, u8, u8)>)> = Vec::new();
    for i in 0..list.len() {
        let Some((save, _, code, _)) = parks[i] else {
            continue;
        };
        let slot = codes[store[i]];
        if pairs.iter().any(|p| p.0 == store[i]) {
            let Some(j) = partner(i) else { return Vec::new() };
            // A save moves the pair where its second half is saved; a
            // restore where its first half is restored.
            let (at, gone) = if save { (j, i) } else { (i, j) };
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((at, Some((true, dst, src))));
            edits.push((gone, None));
        } else if pairs.iter().any(|p| p.1 == store[i]) {
            // the upper half of a pair: moved with the lower
        } else {
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((i, Some((false, dst, src))));
        }
    }
    let mut moves = Vec::new();
    for (i, edit) in edits {
        match edit {
            Some((wide, dst, src)) => {
                put(list, i, gfx10::Encoding::Sop1, 3 + u16::from(wide), mov(wide, dst, src));
                moves.push((i, dst, src));
            }
            None => put(list, i, gfx10::Encoding::Sopp, 0, NOP),
        }
    }
    moves.sort_unstable();
    moves
}

