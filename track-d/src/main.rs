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
static POOLMAX: AtomicUsize = AtomicUsize::new(0);
static POOLSUM: AtomicUsize = AtomicUsize::new(0);
static HEAPSUM: AtomicUsize = AtomicUsize::new(0);

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
        let (d, mut p) = (self.d, self.cuts[k]);
        while p < self.cuts[k + 1] {
            let (u, v, w) = (num(d, &mut p), num(d, &mut p), num(d, &mut p));
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
    w: u16,
}
const ESCAPE: u64 = 0xFFFF;

/// Final forward graph under the new ids.
struct Graph {
    off: Vec<u32>,
    edges: Vec<Edge>,
    heavy: Vec<(u32, u32, u64)>, // (tail, head, weight) for weights >= ESCAPE, sorted
}

impl Graph {
    /// Weight of an escaped edge. Parallel heavy edges share (tail, head); any of them is a
    /// real edge, and the sort puts the lightest first, which is the one a shortest path uses.
    #[cold]
    fn heavy(&self, u: u32, v: u32) -> u64 {
        let i = self.heavy.partition_point(|&(a, b, _)| (a, b) < (u, v));
        self.heavy[i].2
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
    let t = std::time::Instant::now();

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
    eprintln!("pass1 {:?}", t.elapsed());

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
    eprintln!("pass2 {:?}", t.elapsed());
    let mut off1: Vec<u32> = uninit(n + 1);
    sort_buckets(&mut heads, &bstart, &mut off1, |r| r as usize & LOW_MASK, |r| r >> LOW);
    eprintln!("sort2 {:?}", t.elapsed());

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
    eprintln!("bfs {:?}", t.elapsed());

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
    eprintln!("perm {:?}", t.elapsed());

    // Pass 3: parse again, scatter each edge as (new head << LOW | new tail low bits, weight)
    // into the new tail's bucket through shared atomic cursors (a few thousand, so they stay
    // in cache), then sort. Weights of 0xFFFF and up are escaped into a side table.
    let bcur: Vec<AtomicU32> = bstart[..nb].iter().map(|&s| AtomicU32::new(s)).collect();
    let mut edges: Vec<Edge> = uninit(at as usize);
    let out = Shared(edges.as_mut_ptr());
    let heavy = par(|k| {
        let mut heavy = Vec::new();
        g.each(k, |u, v, w| {
            let (u, v) = (perm[u] as usize, perm[v]);
            let slot = bcur[u >> LOW].fetch_add(1, Relaxed) as usize;
            out.put(slot, Edge { head: v << LOW | (u & LOW_MASK) as u32, w: w.min(ESCAPE) as u16 });
            if w >= ESCAPE {
                heavy.push((u as u32, v, w));
            }
        });
        heavy
    });
    eprintln!("pass3 {:?}", t.elapsed());
    let mut off: Vec<u32> = uninit(n + 1);
    sort_buckets(&mut edges, &bstart, &mut off, |e| e.head as usize & LOW_MASK, |e| Edge { head: e.head >> LOW, ..e });
    let mut heavy: Vec<_> = heavy.concat();
    heavy.sort_unstable();
    eprintln!("sort3 {:?}", t.elapsed());
    (Graph { off, edges, heavy }, perm, has_in)
}

#[inline(always)]
unsafe fn prefetch<T>(p: *const T) {
    std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
}

/// Distances live in blocks of BLOCK consecutive node ids, allocated on first touch from a
/// per-thread pool. With the BFS numbering a search touches few blocks, so the pool stays small.
/// The pool's address space is reserved up front; only the pages a search reaches are ever
/// backed by memory, and nothing is copied when it grows.
const BLOCK: usize = 64;
const NONE: u32 = u32::MAX;

/// One thread's Dijkstra state, reused across queries.
///
/// The queue is a radix heap: a node waits in bucket "highest bit where its distance differs
/// from the last settled distance". Distances only grow, so a pop is O(1) except when bucket 0
/// runs dry; then the lowest non-empty bucket is split around its minimum. Buckets hold bare
/// node ids (4 bytes) and read keys from the distance table, so a node whose distance dropped
/// is simply re-filed under its new key; the `done` bits drop the duplicates.
struct Search {
    dir: Vec<u32>,    // block -> index in the pool, or NONE
    dist: Vec<u64>,   // pool: BLOCK distances per allocated block
    done: Vec<u64>,   // pool: one bit per node, set once the node is settled
    blocks: Vec<u32>, // blocks allocated in this query, to reset dir
    last: u64,
    buckets: [Vec<u32>; 65],
}

impl Search {
    fn new(n: usize) -> Self {
        let nb = n.div_ceil(BLOCK);
        Search {
            dir: vec![NONE; nb],
            dist: Vec::with_capacity(nb * BLOCK),
            done: Vec::with_capacity(nb),
            blocks: Vec::new(),
            last: 0,
            buckets: std::array::from_fn(|_| Vec::new()),
        }
    }

    /// Pool index of v's distance, allocating v's block on first touch.
    #[inline(always)]
    fn slot(&mut self, v: usize) -> usize {
        let b = v / BLOCK;
        let mut i = self.dir[b];
        if i == NONE {
            i = self.done.len() as u32;
            self.dist.extend_from_slice(&[INF; BLOCK]);
            self.done.push(0);
            self.dir[b] = i;
            self.blocks.push(b as u32);
        }
        i as usize * BLOCK + v % BLOCK
    }

    /// Pool index of a node that is already in the pool.
    #[inline(always)]
    fn at(&self, v: u32) -> usize {
        self.dir[v as usize / BLOCK] as usize * BLOCK + v as usize % BLOCK
    }

    #[inline(always)]
    fn settled(&self, v: u32) -> bool {
        let i = self.at(v);
        self.done[i / BLOCK] >> (i % BLOCK) & 1 != 0
    }

    #[inline(always)]
    fn push(&mut self, v: u32, key: u64) {
        self.buckets[(64 - (key ^ self.last).leading_zeros()) as usize].push(v);
    }

    /// Settles and returns the unsettled node with the smallest distance.
    fn pop(&mut self) -> Option<u32> {
        loop {
            while let Some(v) = self.buckets[0].pop() {
                if !self.settled(v) {
                    let i = self.at(v);
                    self.done[i / BLOCK] |= 1 << (i % BLOCK);
                    return Some(v);
                }
            }
            let i = (1..65).find(|&i| !self.buckets[i].is_empty())?;
            let mut b = std::mem::take(&mut self.buckets[i]);
            b.retain(|&v| !self.settled(v));
            if let Some(min) = b.iter().map(|&v| self.dist[self.at(v)]).min() {
                self.last = min;
                for &v in &b {
                    self.push(v, self.dist[self.at(v)]);
                }
            }
            b.clear();
            self.buckets[i] = b; // keep its capacity
        }
    }

    fn run(&mut self, g: &Graph, s: usize, t: usize) -> u64 {
        for &b in &self.blocks {
            self.dir[b as usize] = NONE;
        }
        self.blocks.clear();
        self.dist.clear();
        self.done.clear();
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = 0;
        let i = self.slot(s);
        self.dist[i] = 0;
        self.push(s as u32, 0);
        while let Some(u) = self.pop() {
            let du = self.last;
            if u as usize == t {
                return du;
            }
            for &e in &g.edges[g.off[u as usize] as usize..g.off[u as usize + 1] as usize] {
                let v = e.head;
                let w = if e.w as u64 == ESCAPE { g.heavy(u, v) } else { e.w as u64 };
                let nd = du + w;
                let i = self.slot(v as usize);
                if nd < self.dist[i] {
                    self.dist[i] = nd;
                    self.push(v, nd);
                    // v will be popped soon: start fetching its edge row now.
                    unsafe { prefetch(g.edges.as_ptr().add(*g.off.get_unchecked(v as usize) as usize)) };
                }
            }
        }
        INF
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = args.get(1).map_or("/data", |s| s.as_str());
    let out = args.get(2).map_or("/out", |s| s.as_str());

    let t0 = std::time::Instant::now();
    let (g, perm, has_in) = build(map_file(&format!("{data}/graph.txt")));
    eprintln!("build {:?}", t0.elapsed());
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
    let qt: usize = std::env::var("QT").map_or(THREADS, |v| v.parse().unwrap());
    let tq = std::time::Instant::now();
    par(|k| { if k >= qt { return; }
        let mut s = Search::new(n);
        while let Some((a, b, ans)) = queries.get(next.fetch_add(1, Relaxed)) {
            if ans.load(Relaxed) == TODO {
                ans.store(s.run(&g, *a, *b), Relaxed);
                POOLMAX.fetch_max(s.dist.len(), Relaxed);
            }
        }
        POOLSUM.fetch_add(s.dist.len()*8, Relaxed);
        HEAPSUM.fetch_add(s.buckets.iter().map(|b| b.capacity()*4).sum::<usize>(), Relaxed);
    });
    eprintln!("poolmax {} poolsum {}MB heapsum {}MB", POOLMAX.load(Relaxed), POOLSUM.load(Relaxed)>>20, HEAPSUM.load(Relaxed)>>20);
    eprintln!("queries {:?} (phase {:?}) pool {}", t0.elapsed(), tq.elapsed(), 0);

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
