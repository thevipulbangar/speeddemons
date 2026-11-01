//! Track D (Traverse): exact s-t shortest paths on a 1.7M-node road graph.
//!
//! 1. graph.txt is mmapped and parsed in parallel into a heads-only CSR. Edges are first
//!    scattered into buckets of 256 consecutive tail ids (per-thread cursors, no atomics),
//!    then each bucket is counting-sorted by tail inside the cache.
//! 2. Node ids in the file are a random relabelling, so every Dijkstra step would be a cache
//!    miss. Twelve threads grow BFS regions from different roots and the concatenated visit
//!    order becomes the new numbering: neighbours on the map get nearby ids.
//! 3. The text is parsed once more, straight into the final weighted CSR under the new ids.
//! 4. Queries run on 12 threads, plain Dijkstra each, with distances kept in a paged
//!    per-thread table so memory follows the size of the search, not the size of the graph.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

const THREADS: usize = 12; // the scoring box gives us exactly 12 hardware threads
const INF: u64 = u64::MAX; // unreachable
const TODO: u64 = u64::MAX - 1; // query not answered yet; real distances are below 2^63
const LOW: usize = 8; // a bucket holds 2^LOW consecutive tail ids
const LOW_MASK: usize = (1 << LOW) - 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).expect("cannot open input");
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // PROT_READ, MAP_PRIVATE: read straight from the page cache, no copy into our heap.
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// Reads the unsigned decimal at d[*p] and steps past the single separator after it.
/// Every number in both files is followed by a space or LF, so no bounds check is needed.
#[inline(always)]
fn num(d: &[u8], p: &mut usize) -> u64 {
    let mut x = 0;
    loop {
        let c = unsafe { *d.get_unchecked(*p) };
        *p += 1;
        if c < b'0' {
            return x;
        }
        x = x * 10 + (c - b'0') as u64;
    }
}

/// Reads a number of up to 7 digits at d[*p] with no branches (SWAR): digits have bit 0x10
/// set and the separators (space, LF) do not, so the first separator is found with one mask
/// and tzcnt; the digits are then combined pairwise with three multiplies. Needs 8 readable
/// bytes at d[*p]; longer numbers (only the rare huge weights) take the byte loop.
#[inline(always)]
fn fast_num(d: &[u8], p: &mut usize) -> u64 {
    let x = u64::from_le_bytes(unsafe { d.as_ptr().add(*p).cast::<[u8; 8]>().read_unaligned() });
    let sep = !x & 0x1010_1010_1010_1010;
    let len = sep.trailing_zeros() as usize / 8;
    if len >= 8 {
        return num(d, p);
    }
    *p += len + 1;
    // Right-align the digits (len >= 1 always), so the empty bytes become leading zeros.
    let mut y = (x & 0x0F0F_0F0F_0F0F_0F0F) << (64 - 8 * len);
    y = (y * 10 + (y >> 8)) & 0x00FF_00FF_00FF_00FF;
    y = (y * 100 + (y >> 16)) & 0x0000_FFFF_0000_FFFF;
    (y * 10000 + (y >> 32)) & 0xFFFF_FFFF
}

/// Runs f(k) for k in 0..12 on 12 threads and collects the results in order.
fn par<R: Send>(f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let f = &f;
    thread::scope(|s| {
        let hs: Vec<_> = (0..THREADS).map(|k| s.spawn(move || f(k))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Lets threads write disjoint slots of one buffer.
#[derive(Clone, Copy)]
struct Shared<T>(*mut T);
unsafe impl<T> Sync for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}
impl<T> Shared<T> {
    #[inline(always)]
    fn get(self, i: usize) -> T {
        unsafe { self.0.add(i).read() }
    }
    #[inline(always)]
    fn put(self, i: usize, x: T) {
        unsafe { self.0.add(i).write(x) }
    }
}

/// A buffer of n slots that are all written before they are read.
fn uninit<T>(n: usize) -> Vec<T> {
    let mut v = Vec::with_capacity(n);
    unsafe { v.set_len(n) };
    v
}

/// The edge lines of graph.txt, split into one chunk per thread on line boundaries.
struct Edges {
    d: &'static [u8],
    cuts: Vec<usize>,
}

impl Edges {
    /// Calls f(u, v, w) for every edge in chunk k. Self-loops are skipped: they never
    /// shorten a path.
    #[inline(always)]
    fn each(&self, k: usize, mut f: impl FnMut(usize, usize, u64)) {
        let (d, mut p, end) = (self.d, self.cuts[k], self.cuts[k + 1]);
        // A line is at most 3 * 11 bytes, so lines starting before `safe` have 8 bytes of
        // slack after every number and can use fast_num.
        let safe = end.min(d.len().saturating_sub(40));
        while p < end {
            let rd = if p < safe { fast_num } else { num };
            let (u, v, w) = (rd(d, &mut p), rd(d, &mut p), rd(d, &mut p));
            if u != v {
                f(u as usize, v as usize, w);
            }
        }
    }
}

/// Second half of the bucket sort. recs[bstart[b]..bstart[b + 1]] holds bucket b's records
/// in any order; this counting-sorts each bucket by the tail's low bits (`low`), stores
/// `strip(rec)` in place and writes the CSR start of every node to `off`.
fn sort_buckets<T: Copy + Send + Sync>(
    recs: &mut [T],
    bstart: &[u32],
    off: &mut [u32],
    low: impl Fn(T) -> usize + Sync,
    strip: impl Fn(T) -> T + Sync,
) {
    let (n, nb) = (off.len() - 1, bstart.len() - 1);
    off[n] = bstart[nb];
    let (out, offp) = (Shared(recs.as_mut_ptr()), Shared(off.as_mut_ptr()));
    par(|k| {
        let mut tmp: Vec<T> = Vec::new();
        for b in nb * k / THREADS..nb * (k + 1) / THREADS {
            let (lo, hi) = (bstart[b] as usize, bstart[b + 1] as usize);
            tmp.clear();
            tmp.extend_from_slice(unsafe { std::slice::from_raw_parts(out.0.add(lo), hi - lo) });
            let mut cur = [0u32; 1 << LOW];
            for &r in &tmp {
                cur[low(r)] += 1;
            }
            let mut at = lo as u32;
            for (l, c) in cur.iter_mut().enumerate().take(n - (b << LOW)) {
                offp.put((b << LOW) + l, at);
                at += std::mem::replace(c, at);
            }
            for &r in &tmp {
                let c = &mut cur[low(r)];
                out.put(*c as usize, strip(r));
                *c += 1;
            }
        }
    });
}

/// One edge in 6 bytes. Road weights are far below 2^16, so the few heavier ones store
/// ESCAPE here and their true weight in `Graph::heavy`.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Edge {
    head: u32,
    w: u16,   // weight of this edge
    win: u16, // weight of the reverse edge head -> tail, or ABSENT
}
const ESCAPE: u64 = 0xFFFE; // weight too big for 16 bits: look it up in `heavy`
const ABSENT: u16 = 0xFFFF; // no reverse edge

/// Final forward graph under the new ids.
struct Graph {
    off: Vec<u32>,
    edges: Vec<Edge>,
    heavy: Vec<(u32, u32, u64)>, // (tail, head, weight) for weights >= ESCAPE, sorted
    // In-edges y -> x that no row-x entry carries (x -> y does not exist): (x, y, weight),
    // sorted, with one bit per x saying whether it has any.
    side: Vec<(u32, u32, u64)>,
    has_side: Vec<u64>,
}

impl Graph {
    /// Weight of an escaped edge. Parallel heavy edges share (tail, head); any of them is a
    /// real edge, and the sort puts the lightest first, which is the one a shortest path uses.
    #[cold]
    fn heavy(&self, u: u32, v: u32) -> u64 {
        let i = self.heavy.partition_point(|&(a, b, _)| (a, b) < (u, v));
        self.heavy[i].2
    }

    #[inline(always)]
    fn row(&self, u: usize) -> &[Edge] {
        &self.edges[self.off[u] as usize..self.off[u + 1] as usize]
    }

    /// Weight of tail -> head, stored as `w16` (ESCAPE means look it up).
    #[inline(always)]
    fn weight(&self, tail: u32, head: u32, w16: u16) -> u64 {
        if w16 as u64 == ESCAPE { self.heavy(tail, head) } else { w16 as u64 }
    }

    /// Fills every entry's reverse weight, so a backward search reads the same rows as a
    /// forward one, and lists the in-edges no row can carry. Rows are short, so finding
    /// the reverse edge is a scan of the head's row.
    fn link(&mut self) {
        let n = self.off.len() - 1;
        let out = Shared(self.edges.as_mut_ptr());
        let g = &*self;
        let side = par(|k| {
            let mut side = Vec::new();
            for u in n * k / THREADS..n * (k + 1) / THREADS {
                for i in g.off[u] as usize..g.off[u + 1] as usize {
                    let e = out.get(i);
                    // Parallel reverse edges: only the lightest matters.
                    let back = g.row(e.head as usize).iter().filter(|r| r.head == u as u32);
                    let win = back.map(|r| g.weight(e.head, u as u32, r.w)).min();
                    let win = match win {
                        None => {
                            side.push((e.head, u as u32, g.weight(u as u32, e.head, e.w)));
                            ABSENT
                        }
                        Some(w) => w.min(ESCAPE) as u16,
                    };
                    out.put(i, Edge { win, ..e });
                }
            }
            side
        });
        let mut side = side.concat();
        side.sort_unstable();
        let mut has_side = vec![0u64; n.div_ceil(64)];
        for &(x, _, _) in &side {
            has_side[x as usize / 64] |= 1 << (x % 64);
        }
        (self.side, self.has_side) = (side, has_side);
    }

    /// Calls f(neighbour, weight) for every edge into x.
    #[inline(always)]
    fn each_in(&self, x: usize, mut f: impl FnMut(u32, u64)) {
        for e in self.row(x) {
            if e.win != ABSENT {
                f(e.head, self.weight(e.head, x as u32, e.win));
            }
        }
        if self.has_side[x / 64] >> (x % 64) & 1 != 0 {
            let i = self.side.partition_point(|&(a, _, _)| (a as usize) < x);
            for &(_, y, w) in self.side[i..].iter().take_while(|s| s.0 as usize == x) {
                f(y, w);
            }
        }
    }
}

/// Builds the graph and returns it with `perm` (old id -> new id) and the in-edge bitset (old ids).
fn build(d: &'static [u8]) -> (Graph, Vec<u32>, Vec<u64>) {
    let mut p = 0;
    let n = num(d, &mut p) as usize;
    let _m = num(d, &mut p);
    // Records pack (head << LOW | tail low bits) into 32 bits.
    assert!(n < 1 << (32 - LOW), "node ids above 2^24 are not supported");
    let mut cuts = vec![p];
    for i in 1..THREADS {
        let mut c = (p + (d.len() - p) * i / THREADS).max(cuts[i - 1]);
        while c < d.len() && d[c - 1] != b'\n' {
            c += 1;
        }
        cuts.push(c);
    }
    cuts.push(d.len());
    let g = Edges { d, cuts };
    let nb = n.div_ceil(1 << LOW);

    // Pass 1: per-thread bucket counts and in-edge bitsets, all thread-local.
    let local = par(|k| {
        let (mut cnt, mut has_in) = (vec![0u32; nb], vec![0u64; n.div_ceil(64)]);
        g.each(k, |u, v, _| {
            cnt[u >> LOW] += 1;
            has_in[v / 64] |= 1 << (v % 64);
        });
        (cnt, has_in)
    });
    // Bucket starts, and each thread's first slot in every bucket.
    let mut has_in = vec![0u64; n.div_ceil(64)];
    let mut bstart = vec![0u32; nb + 1];
    let mut cur = vec![vec![0u32; nb]; THREADS];
    let mut at = 0;
    for b in 0..nb {
        bstart[b] = at;
        for k in 0..THREADS {
            cur[k][b] = at;
            at += local[k].0[b];
        }
    }
    bstart[nb] = at;
    for (_, hi) in &local {
        for (h, x) in has_in.iter_mut().zip(hi) {
            *h |= x;
        }
    }
    drop(local);

    // Pass 2: scatter (head << LOW | tail low bits) into the tail's bucket, then sort buckets.
    let mut heads: Vec<u32> = uninit(at as usize);
    let out = Shared(heads.as_mut_ptr());
    let cur: Vec<_> = cur.into_iter().map(std::sync::Mutex::new).collect();
    par(|k| {
        let mut cur = cur[k].lock().unwrap();
        g.each(k, |u, v, _| {
            let c = &mut cur[u >> LOW];
            out.put(*c as usize, (v << LOW | u & LOW_MASK) as u32);
            *c += 1;
        })
    });
    drop(cur);
    let mut off1: Vec<u32> = uninit(n + 1);
    sort_buckets(&mut heads, &bstart, &mut off1, |r| r as usize & LOW_MASK, |r| r >> LOW);

    // Region-growing BFS: each thread repeatedly takes the next unclaimed node of its slice of
    // ids as a root and runs BFS through unclaimed nodes. Nodes are claimed in an atomic bitset,
    // so the regions tile the graph; the order is timing-dependent, the answers are not.
    let claimed: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
    let claim = |v: usize| {
        let bit = 1 << (v % 64);
        claimed[v / 64].load(Relaxed) & bit == 0 && claimed[v / 64].fetch_or(bit, Relaxed) & bit == 0
    };
    let regions = par(|k| {
        let mut q = Vec::with_capacity(n / THREADS * 2);
        for root in n * k / THREADS..n * (k + 1) / THREADS {
            if !claim(root) {
                continue;
            }
            let mut i = q.len();
            q.push(root as u32);
            while i < q.len() {
                // The queue says which rows come next: fetch them ahead of time.
                if let Some(&a) = q.get(i + 8) {
                    unsafe { prefetch(heads.as_ptr().add(off1[a as usize] as usize)) };
                }
                let u = q[i] as usize;
                for &v in &heads[off1[u] as usize..off1[u + 1] as usize] {
                    if claim(v as usize) {
                        q.push(v);
                    }
                }
                i += 1;
            }
        }
        q
    });
    drop(heads);

    // perm: old -> new; new bucket sizes come from the old degrees.
    let mut perm: Vec<u32> = uninit(n);
    let pp = Shared(perm.as_mut_ptr());
    let base: Vec<usize> = regions.iter().scan(0, |at, r| Some(std::mem::replace(at, *at + r.len()))).collect();
    let deg = |old: u32| off1[old as usize + 1] - off1[old as usize];
    let bsize: Vec<Vec<u32>> = par(|k| {
        let mut bs = vec![0u32; nb];
        for (i, &old) in regions[k].iter().enumerate() {
            pp.put(old as usize, (base[k] + i) as u32);
            bs[(base[k] + i) >> LOW] += deg(old);
        }
        bs
    });
    drop((regions, off1));
    let mut at = 0;
    for b in 0..nb {
        bstart[b] = at;
        at += bsize.iter().map(|bs| bs[b]).sum::<u32>();
    }

    // Pass 3: parse again, scatter each edge as (new head << LOW | new tail low bits, weight)
    // into the new tail's bucket through shared atomic cursors (a few thousand, so they stay
    // in cache), then sort. Weights of 0xFFFF and up are escaped into a side table.
    let bcur: Vec<AtomicU32> = bstart[..nb].iter().map(|&s| AtomicU32::new(s)).collect();
    let mut edges: Vec<Edge> = uninit(at as usize);
    let out = Shared(edges.as_mut_ptr());
    let heavy = par(|k| {
        let mut heavy = Vec::new();
        // Lines go through in batches: the perm lookups of a whole batch are prefetched before
        // the first locked add, which would otherwise stall each lookup behind it.
        let mut batch = Vec::with_capacity(32);
        let mut flush = |batch: &mut Vec<(usize, usize, u64)>| {
            for &(u, v, w) in batch.iter() {
                let (u, v) = (perm[u] as usize, perm[v]);
                let slot = bcur[u >> LOW].fetch_add(1, Relaxed) as usize;
                out.put(slot, Edge { head: v << LOW | (u & LOW_MASK) as u32, w: w.min(ESCAPE) as u16, win: 0 });
                if w >= ESCAPE {
                    heavy.push((u as u32, v, w));
                }
            }
            batch.clear();
        };
        g.each(k, |u, v, w| {
            unsafe { prefetch(perm.as_ptr().add(u)) };
            unsafe { prefetch(perm.as_ptr().add(v)) };
            batch.push((u, v, w));
            if batch.len() == 32 {
                flush(&mut batch);
            }
        });
        flush(&mut batch);
        heavy
    });
    let mut off: Vec<u32> = uninit(n + 1);
    sort_buckets(&mut edges, &bstart, &mut off, |e| e.head as usize & LOW_MASK, |e| Edge { head: e.head >> LOW, ..e });
    let mut heavy: Vec<_> = heavy.concat();
    heavy.sort_unstable();
    let mut g = Graph { off, edges, heavy, side: Vec::new(), has_side: Vec::new() };
    g.link();
    (g, perm, has_in)
}

#[inline(always)]
unsafe fn prefetch<T>(p: *const T) {
    std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
}

/// Radix heap for Dijkstra's monotone keys: an entry sits in bucket "highest bit where it
/// differs from the last popped key". Pops are O(1) except when bucket 0 runs dry; then the
/// lowest non-empty bucket is split around its minimum.
///
/// An entry is 8 bytes: the low 32 bits of its key, then the node id. Every queued key is a
/// settled distance plus one u32 weight, so it lies in [last, last + 2^32) and its low half
/// is enough to rebuild it exactly.
struct RadixHeap {
    last: u64,
    len: usize, // entries queued, stale ones included
    buckets: [Vec<u64>; 65],
}

impl RadixHeap {
    fn clear(&mut self) {
        self.last = 0;
        self.len = 0;
        self.buckets.iter_mut().for_each(Vec::clear);
    }

    #[inline(always)]
    fn key(&self, e: u64) -> u64 {
        self.last + ((e >> 32) as u32).wrapping_sub(self.last as u32) as u64
    }

    #[inline(always)]
    fn file(&mut self, key: u64, e: u64) {
        self.buckets[(64 - (key ^ self.last).leading_zeros()) as usize].push(e);
    }

    #[inline(always)]
    fn push(&mut self, key: u64, v: u32) {
        self.len += 1;
        self.file(key, key << 32 | v as u64);
    }

    /// Smallest queued key (a lower bound: it may belong to a stale entry).
    #[inline(always)]
    fn peek(&mut self) -> Option<u64> {
        self.pop().map(|(k, v)| {
            self.buckets[0].push(k << 32 | v as u64);
            self.len += 1;
            k
        })
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.buckets[0].is_empty() {
            let i = (1..65).find(|&i| !self.buckets[i].is_empty())?;
            let mut b = std::mem::take(&mut self.buckets[i]);
            let old = self.last;
            self.last = b.iter().map(|&e| self.key(e)).min().unwrap();
            for &e in &b {
                // Rebuild against the old `last`, then file against the new one.
                let key = old + ((e >> 32) as u32).wrapping_sub(old as u32) as u64;
                self.file(key, e);
            }
            b.clear();
            self.buckets[i] = b; // keep its capacity
        }
        let e = self.buckets[0].pop()?;
        self.len -= 1;
        Some((self.last, e as u32))
    }
}

/// Distances live in blocks of BLOCK consecutive node ids, allocated on first touch from a
/// per-thread pool. With the BFS numbering a search touches few blocks, so the pool stays small.
const BLOCK: usize = 64;
const NONE: u32 = u32::MAX;

/// A distance slot: u32 for almost every search, u64 for the few that go past 2^32 - 2.
trait Dist: Copy {
    const INF: Self;
    fn wide(self) -> u64;
    fn narrow(x: u64) -> Self;
}

impl Dist for u32 {
    const INF: u32 = u32::MAX;
    fn wide(self) -> u64 {
        self as u64
    }
    fn narrow(x: u64) -> u32 {
        x as u32
    }
}

impl Dist for u64 {
    const INF: u64 = u64::MAX;
    fn wide(self) -> u64 {
        self
    }
    fn narrow(x: u64) -> u64 {
        x
    }
}

struct Search<D> {
    dir: Vec<u32>,    // block -> index in pool, or NONE
    pool: Vec<D>,     // BLOCK distances per allocated block
    blocks: Vec<u32>, // blocks allocated in this query, to reset dir
    heap: RadixHeap,
}

impl<D: Dist> Search<D> {
    fn new(n: usize) -> Self {
        Search {
            dir: vec![NONE; n.div_ceil(BLOCK)],
            pool: Vec::with_capacity(n.div_ceil(BLOCK) * BLOCK), // address space only
            blocks: Vec::new(),
            heap: RadixHeap { last: 0, len: 0, buckets: std::array::from_fn(|_| Vec::new()) },
        }
    }

    #[inline(always)]
    fn dist(&mut self, v: usize) -> &mut D {
        let b = v / BLOCK;
        let mut i = self.dir[b];
        if i == NONE {
            i = (self.pool.len() / BLOCK) as u32;
            self.pool.extend_from_slice(&[D::INF; BLOCK]);
            self.dir[b] = i;
            self.blocks.push(b as u32);
        }
        unsafe { self.pool.get_unchecked_mut(i as usize * BLOCK + v % BLOCK) }
    }

    /// Distance of v if this search has reached it.
    #[inline(always)]
    fn get(&self, v: usize) -> u64 {
        match self.dir[v / BLOCK] {
            NONE => INF,
            i => match unsafe { *self.pool.get_unchecked(i as usize * BLOCK + v % BLOCK) }.wide() {
                x if x == D::INF.wide() => INF, // block allocated, node not reached
                x => x,
            },
        }
    }

    fn reset(&mut self, s: usize) {
        for &b in &self.blocks {
            self.dir[b as usize] = NONE;
        }
        self.blocks.clear();
        self.pool.clear();
        self.heap.clear();
        *self.dist(s) = D::narrow(0);
        self.heap.push(0, s as u32);
    }

    /// One step of a bidirectional search: settles the closest queued node and relaxes its
    /// out-edges (forward) or in-edges (backward). `best` is the shortest s-t path seen,
    /// through a node both searches have reached. Returns false if a distance overflows D.
    #[inline(always)]
    fn step(&mut self, other: &Self, g: &Graph, forward: bool, best: &mut u64) -> bool {
        let (du, u) = self.heap.pop().unwrap();
        if du > self.dist(u as usize).wide() {
            return true; // stale entry
        }
        let mut ok = true;
        let mut relax = |v: u32, w: u64| {
            let nd = du + w;
            if nd >= D::INF.wide() {
                ok = false;
                return;
            }
            let dv = self.dist(v as usize);
            if nd < dv.wide() {
                *dv = D::narrow(nd);
                self.heap.push(nd, v);
                unsafe { prefetch(g.edges.as_ptr().add(*g.off.get_unchecked(v as usize) as usize)) };
                let o = other.get(v as usize);
                if o != INF {
                    *best = (*best).min(nd + o);
                }
            }
        };
        if forward {
            for e in g.row(u as usize) {
                relax(e.head, g.weight(u, e.head, e.w));
            }
        } else {
            g.each_in(u as usize, relax);
        }
        ok
    }
}

/// Bidirectional Dijkstra. Each step expands the side with fewer queued entries (a near
/// 2^32 first edge would make key-based alternation flood one side); it stops once the two
/// smallest keys add up to the best meeting distance. None if a distance overflows D.
fn bidi<D: Dist>(f: &mut Search<D>, b: &mut Search<D>, g: &Graph, s: usize, t: usize) -> Option<u64> {
    f.reset(s);
    b.reset(t);
    let mut best = INF;
    while let (Some(kf), Some(kb)) = (f.heap.peek(), b.heap.peek()) {
        if kf + kb >= best {
            break;
        }
        let ok = if f.heap.len <= b.heap.len { f.step(b, g, true, &mut best) } else { b.step(f, g, false, &mut best) };
        if !ok {
            return None;
        }
    }
    Some(best)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = args.get(1).map_or("/data", |s| s.as_str());
    let out = args.get(2).map_or("/out", |s| s.as_str());

    let (g, perm, has_in) = build(map_file(&format!("{data}/graph.txt")));
    let n = g.off.len() - 1;

    // Queries in new ids. Trivial ones are answered here: s == t is 0; nothing reaches a node
    // without in-edges and a node without out-edges reaches nothing, so both are -1.
    let qd = map_file(&format!("{data}/queries.txt"));
    let mut queries = Vec::new();
    let mut p = 0;
    while p < qd.len() {
        let (s, t) = (num(qd, &mut p) as usize, num(qd, &mut p) as usize);
        let (ns, nt) = (perm[s] as usize, perm[t] as usize);
        let ans = if s == t {
            0
        } else if has_in[t / 64] >> (t % 64) & 1 == 0 || g.off[ns] == g.off[ns + 1] {
            INF
        } else {
            TODO
        };
        queries.push((ns, nt, AtomicU64::new(ans)));
    }
    drop((perm, has_in));

    // Work-stealing over queries: each thread grabs the next index until none are left.
    let next = AtomicUsize::new(0);
    par(|_| {
        let (mut fw, mut bw) = (Search::<u32>::new(n), Search::<u32>::new(n));
        let mut wide: Option<(Search<u64>, Search<u64>)> = None;
        while let Some((a, b, ans)) = queries.get(next.fetch_add(1, Relaxed)) {
            if ans.load(Relaxed) == TODO {
                let d = bidi(&mut fw, &mut bw, &g, *a, *b).unwrap_or_else(|| {
                    let (f, w) = wide.get_or_insert_with(|| (Search::new(n), Search::new(n)));
                    bidi(f, w, &g, *a, *b).unwrap()
                });
                ans.store(d, Relaxed);
            }
        }
    });

    let mut text = String::with_capacity(queries.len() * 12);
    for (_, _, a) in &queries {
        match a.load(Relaxed) {
            INF => text.push_str("-1\n"),
            d => {
                text.push_str(&d.to_string());
                text.push('\n');
            }
        }
    }
    std::fs::write(format!("{out}/distances.txt"), text).expect("cannot write distances.txt");
}
