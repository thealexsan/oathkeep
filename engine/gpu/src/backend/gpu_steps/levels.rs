use super::*;
use super::forwarding::{Forwarding, Fwd};

/// Bottom-up iterative range-chmax / range-max segment tree over compressed
/// interval endpoints for `O(N log N)` level assignment.
struct LevelSegTree {
    size: usize,
    sub: Vec<u32>,
    lazy: Vec<u32>,
}

impl LevelSegTree {
    fn new(num_leaves: usize) -> Self {
        let size = num_leaves.max(1).next_power_of_two();
        Self {
            size,
            sub: vec![0; 2 * size],
            lazy: vec![0; 2 * size],
        }
    }

    #[inline]
    fn query_max(&self, l0: usize, r0: usize) -> u32 {
        if l0 >= r0 {
            return 0;
        }
        let mut ans = 0u32;
        let (mut l, mut r) = (l0 + self.size, r0 + self.size);
        while l < r {
            if l & 1 == 1 {
                ans = ans.max(self.sub[l]);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                ans = ans.max(self.sub[r]);
            }
            l >>= 1;
            r >>= 1;
        }
        let mut pl = (l0 + self.size) >> 1;
        let mut pr = (r0 - 1 + self.size) >> 1;
        while pl > 0 {
            ans = ans.max(self.lazy[pl]);
            pl >>= 1;
        }
        while pr > 0 {
            ans = ans.max(self.lazy[pr]);
            pr >>= 1;
        }
        ans
    }

    #[inline]
    fn chmax_range(&mut self, l0: usize, r0: usize, val: u32) {
        if l0 >= r0 || val == 0 {
            return;
        }
        let (mut l, mut r) = (l0 + self.size, r0 + self.size);
        while l < r {
            if l & 1 == 1 {
                self.lazy[l] = self.lazy[l].max(val);
                self.sub[l] = self.sub[l].max(val);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                self.lazy[r] = self.lazy[r].max(val);
                self.sub[r] = self.sub[r].max(val);
            }
            l >>= 1;
            r >>= 1;
        }
        let mut pl = (l0 + self.size) >> 1;
        let mut pr = (r0 - 1 + self.size) >> 1;
        while pl > 0 {
            self.sub[pl] = self.sub[pl].max(val);
            pl >>= 1;
        }
        while pr > 0 {
            self.sub[pr] = self.sub[pr].max(val);
            pr >>= 1;
        }
    }
}

pub(super) fn assign_levels_direct(does: &[Does], fwd: &Forwarding, order: &[usize]) -> Vec<(usize, u32)> {
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    let mut copies: Vec<usize> = Vec::new();
    let mut i_buf = [(0usize, 0usize, 0usize); 1];
    let mut j_buf = [(0usize, 0usize, 0usize); 1];
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut i_buf) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0u32;
        if writes {
            for &(j, at) in &out {
                if at < level {
                    continue;
                }
                let Some((j_segs, jw)) = fwd.segments_of(does, j, &mut j_buf) else {
                    continue;
                };
                let dep = (jw.0 < jw.1 && (meet(jw, w) || segs.iter().any(|&(_, a, n)| meet(jw, (a, a + n)))))
                    || j_segs.iter().any(|&(_, a, n)| meet((a, a + n), w));
                if dep {
                    level = at + 1;
                }
            }
            copies.push(out.len());
        } else {
            for &k in &copies {
                let (j, at) = out[k];
                if at < level {
                    continue;
                }
                let Some((_, jw)) = reads_writes(&does[j]) else {
                    continue;
                };
                if jw.0 < jw.1 && segs.iter().any(|&(_, a, n)| meet(jw, (a, a + n))) {
                    level = at + 1;
                }
            }
        }
        out.push((i, level));
    }
    out
}

pub(super) fn assign_levels_segtree(does: &[Does], fwd: &Forwarding, order: &[usize], mut copies_left: usize) -> Vec<(usize, u32)> {
    let mut coords: Vec<usize> = Vec::with_capacity(order.len() * 2 + copies_left * 2);
    let mut single_buf = [(0usize, 0usize, 0usize); 1];
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) else {
            continue;
        };
        for &(_, a, n) in segs {
            if n > 0 {
                coords.push(a);
                coords.push(a + n);
            }
        }
        if w.0 < w.1 {
            coords.push(w.0);
            coords.push(w.1);
        }
    }
    coords.sort_unstable();
    coords.dedup();
    if coords.len() < 2 {
        return order.iter().map(|&i| (i, 0)).collect();
    }
    let idx_of = |addr: usize| coords.binary_search(&addr).unwrap_or(0);
    let num_leaves = coords.len() - 1;
    let mut write_tree = LevelSegTree::new(num_leaves);
    let mut read_tree = LevelSegTree::new(num_leaves);
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0u32;
        if segs.len() == 1 {
            let (_, a, n) = segs[0];
            let (rl, rr) = if n > 0 { (idx_of(a), idx_of(a + n)) } else { (0, 0) };
            if rl < rr {
                level = level.max(write_tree.query_max(rl, rr));
            }
            if writes {
                let (wl, wr) = (idx_of(w.0), idx_of(w.1));
                level = level.max(write_tree.query_max(wl, wr));
                level = level.max(read_tree.query_max(wl, wr));
                copies_left -= 1;
                let next = level + 1;
                if copies_left > 0 && rl < rr {
                    read_tree.chmax_range(rl, rr, next);
                }
                write_tree.chmax_range(wl, wr, next);
            } else if copies_left > 0 && rl < rr {
                read_tree.chmax_range(rl, rr, level + 1);
            }
        } else {
            for &(_, a, n) in segs {
                if n > 0 {
                    let (rl, rr) = (idx_of(a), idx_of(a + n));
                    level = level.max(write_tree.query_max(rl, rr));
                }
            }
            let (wl, wr) = if writes { (idx_of(w.0), idx_of(w.1)) } else { (0, 0) };
            if writes {
                level = level.max(write_tree.query_max(wl, wr));
                level = level.max(read_tree.query_max(wl, wr));
                copies_left -= 1;
            }
            let next = level + 1;
            if copies_left > 0 {
                for &(_, a, n) in segs {
                    if n > 0 {
                        let (rl, rr) = (idx_of(a), idx_of(a + n));
                        read_tree.chmax_range(rl, rr, next);
                    }
                }
            }
            if writes {
                write_tree.chmax_range(wl, wr, next);
            }
        }
        out.push((i, level));
    }
    out
}
