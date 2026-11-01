//! Track A (Ingest): per (hour, endpoint) request count, p50/p95/p99 latency and 5xx rate.
//!
//! The log is mmapped and read twice, and almost nothing is kept in memory:
//!   pass 1  (one chunk per thread) reads just the hour of every line and records, per 16 KiB
//!           block of the file, a 72-bit mask of the hours present in it;
//!   pass 2  goes hour by hour with all threads together: each parses a share of the blocks
//!           holding the hour, the per-endpoint counts give every (thread, endpoint) pair a
//!           range of one shared hour buffer, then each thread selects and formats a share
//!           of the endpoints and writes it at its place in the file.
//! Only one hour of latencies (~170 KB) is ever held. Disorder in the log costs speed only.
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};
use std::sync::Mutex;
use std::thread;

const HOURS: usize = 72; // 2026-10-30T18 .. 2026-11-02T17, fixed by the spec
const THREADS: usize = 12; // the scoring box gives us exactly 12 hardware threads
const BLOCK_SHIFT: u32 = 14; // 16 KiB blocks, about 200 lines each

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).expect("cannot open requests.log");
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // Reserve the file's size plus a spare page of zeros (anonymous, PROT_READ), then map the
    // file over the front (MAP_PRIVATE | MAP_FIXED). Reads straight from the page cache, no
    // copy into our heap, and SIMD loads may run a few bytes past the last line.
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 4096, 1, 0x22, -1, 0);
        assert!(p as isize != -1, "mmap failed");
        let q = mmap(p, len, 1, 0x12, f.as_raw_fd(), 0);
        assert!(q == p, "mmap failed");
        std::slice::from_raw_parts(p, len)
    }
}

/// The first line start at or after `p`.
fn line_start(data: &[u8], mut p: usize) -> usize {
    while p > 0 && p < data.len() && data[p - 1] != b'\n' {
        p += 1;
    }
    p.min(data.len())
}

/// Calls `f(offset, line)` on every line (without its LF) in data[start..end]; both bounds are
/// line starts. Newlines are found 32 bytes at a time with AVX2.
#[inline(always)]
fn for_each_line<'a>(data: &'a [u8], start: usize, end: usize, mut f: impl FnMut(usize, &'a [u8])) {
    let (mut line, mut pos) = (start, start);
    while pos + 64 <= end {
        // 64 bytes per step: two compares merged into one 64-bit mask halve the loop overhead.
        let lf = |i: usize| unsafe {
            let v = _mm256_loadu_si256(data.as_ptr().add(pos + i) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32 as u64
        };
        let mut mask = lf(0) | lf(32) << 32;
        while mask != 0 {
            let lf = pos + mask.trailing_zeros() as usize;
            // SAFETY: start <= line <= lf < end <= data.len().
            f(line, unsafe { data.get_unchecked(line..lf) });
            line = lf + 1;
            mask &= mask - 1;
        }
        pos += 64;
    }
    for i in pos..end {
        if data[i] == b'\n' {
            f(line, &data[line..i]);
            line = i + 1;
        }
    }
}

/// Hour index 0..72 of a line; the timestamp is always the first 27 bytes.
/// Only four days occur (Oct 30, Oct 31, Nov 1, Nov 2), so no date library is needed.
#[inline(always)]
fn hour_of(l: &[u8]) -> usize {
    // SAFETY: a line is at least 39 bytes, so bytes 6..13 exist.
    let d = |i: usize| (unsafe { *l.get_unchecked(i) } - b'0') as usize;
    let day = d(8) * 10 + d(9);
    let day_idx = if l[6] == b'0' { day - 30 } else { day + 1 };
    day_idx * 24 + d(11) * 10 + d(12) - 18
}

/// Bytes 5..13 of a timestamp, `MM-DDTHH`, as one word: equal exactly when the hour is.
#[inline(always)]
fn hour_key(l: &[u8]) -> u64 {
    u64::from_le_bytes(l[5..13].try_into().unwrap())
}

/// `YYYY-MM-DDTHH:00:00Z ` for hour index h, the start of every output line of that hour.
fn hour_label(h: usize) -> String {
    let abs = 18 + h; // hours since 2026-10-30T00
    let days = ["2026-10-30T", "2026-10-31T", "2026-11-01T", "2026-11-02T"];
    format!("{}{:02}:00:00Z ", days[abs / 24], abs % 24)
}

/// The rest of a line -> (endpoint, is 5xx, latency), without a byte loop.
/// Layout: `TIMESTAMP METHOD ENDPOINT SSS LATENCY`; a line is at least 39 bytes.
#[inline(always)]
fn parse_rest(l: &[u8]) -> (&[u8], bool, u32) {
    // The method (3..7 letters) starts at byte 28, so the space after it is in l[31..36].
    // SWAR: xor with spaces turns it into the lowest zero byte of an 8-byte word.
    let w = u64::from_le_bytes(l[28..36].try_into().unwrap()) ^ 0x2020_2020_2020_2020;
    let zero = w.wrapping_sub(0x0101_0101_0101_0101) & !w & 0x8080_8080_8080_8080;
    let ep = 29 + (zero.trailing_zeros() / 8) as usize;
    // The latency (1..10 digits) and the space before it lie in the last 16 bytes.
    let n = l.len();
    unsafe {
        let tail = _mm_loadu_si128(l.as_ptr().add(n - 16) as *const __m128i);
        let spaces = _mm_movemask_epi8(_mm_cmpeq_epi8(tail, _mm_set1_epi8(b' ' as i8)));
        let last = 31 - spaces.leading_zeros() as i32; // lane of the space before the latency
        let sp = n - 16 + last as usize;
        // Digits as 0..9, everything up to that space zeroed, then multiply-add pairs of
        // digits into 2-digit and then 4-digit groups: lanes 4..16 hold the last 12 digits.
        let lanes = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let keep = _mm_cmpgt_epi8(lanes, _mm_set1_epi8(last as i8));
        let d = _mm_and_si128(_mm_sub_epi8(tail, _mm_set1_epi8(b'0' as i8)), keep);
        let d2 = _mm_maddubs_epi16(d, _mm_set_epi8(1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10));
        let d4 = _mm_madd_epi16(d2, _mm_set_epi16(1, 100, 1, 100, 1, 100, 1, 100));
        let g: [u32; 4] = std::mem::transmute(d4);
        let lat = (g[1] as u64 * 100_000_000 + g[2] as u64 * 10_000 + g[3] as u64) as u32;
        // SAFETY: 29 <= ep and the status's 3 digits and a space lie between ep and sp.
        (l.get_unchecked(ep..sp - 4), *l.get_unchecked(sp - 3) == b'5', lat)
    }
}

/// Equality of two strings of length `n`, 32 bytes per step. Either side may be read up to 31
/// bytes past its end: the table's arena and the file mapping are padded for this.
#[inline(always)]
unsafe fn same(a: *const u8, b: *const u8, n: usize) -> bool {
    let mut i = 0;
    while i < n {
        let (x, y) = (_mm256_loadu_si256(a.add(i) as *const __m256i), _mm256_loadu_si256(b.add(i) as *const __m256i));
        let equal = _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32;
        let wanted = if n - i >= 32 { u32::MAX } else { (1 << (n - i)) - 1 };
        if !equal & wanted != 0 {
            return false;
        }
        i += 32;
    }
    true
}

/// Open-addressing endpoint -> dense id table, private to one thread. Key bytes are copied
/// into one small arena, so probing touches a few hot cache lines instead of ~1,800 scattered
/// places in the 239 MB mapping. A slot packs everything a lookup needs into one word:
/// arena offset << 32 | (id + 1) << 8 | length (0 = empty; endpoints are at most 125 bytes).
struct Table {
    slots: Vec<u64>,
    spans: Vec<(u32, u32)>, // id -> (start, end) in `arena`
    arena: Vec<u8>,         // keys, each followed by 32 zero bytes of padding
}

impl Table {
    fn new() -> Self {
        Table { slots: vec![0; 4096], spans: Vec::new(), arena: Vec::new() }
    }

    fn key(&self, id: usize) -> &[u8] {
        let (a, b) = self.spans[id];
        &self.arena[a as usize..b as usize]
    }

    /// Hash from the first and last 8 bytes plus the length: no loop over the string.
    fn hash(k: &[u8]) -> usize {
        let n = k.len();
        let word = |s: &[u8]| {
            let mut b = [0u8; 8];
            b[..s.len()].copy_from_slice(s);
            u64::from_le_bytes(b)
        };
        let (a, b) = if n >= 8 { (word(&k[..8]), word(&k[n - 8..])) } else { (word(k), 0) };
        ((a ^ b.rotate_left(29) ^ n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize
    }

    /// The id of `k`, which must be followed by 31 readable bytes (true for any endpoint
    /// in the padded mapping).
    #[inline(always)]
    fn id(&mut self, k: &[u8]) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = Self::hash(k) & mask;
        loop {
            // SAFETY: i is masked by slots.len() - 1, and slots.len() is a power of two.
            let s = unsafe { *self.slots.get_unchecked(i) };
            if s == 0 {
                break;
            }
            let (start, n) = ((s >> 32) as usize, s as u8 as usize);
            if n == k.len() && unsafe { same(self.arena.as_ptr().add(start), k.as_ptr(), n) } {
                return (s as u32 >> 8) as usize - 1;
            }
            i = (i + 1) & mask;
        }
        let start = self.arena.len();
        self.arena.extend_from_slice(k);
        self.arena.extend_from_slice(&[0; 32]);
        self.spans.push((start as u32, (start + k.len()) as u32));
        self.slots[i] = Self::slot(start, k.len(), self.spans.len());
        if self.spans.len() * 2 > self.slots.len() {
            self.grow();
        }
        self.spans.len() - 1
    }

    fn slot(start: usize, len: usize, id1: usize) -> u64 {
        (start as u64) << 32 | (id1 as u64) << 8 | len as u64
    }

    fn grow(&mut self) {
        self.slots = vec![0; self.slots.len() * 2];
        let mask = self.slots.len() - 1;
        for id in 0..self.spans.len() {
            let mut i = Self::hash(self.key(id)) & mask;
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            let (a, b) = self.spans[id];
            self.slots[i] = Self::slot(a as usize, (b - a) as usize, id + 1);
        }
    }
}

/// Appends `v` in decimal and then `end`. The digits go into a fixed 24-byte block that is
/// appended whole and trimmed: a fixed-size copy is two vector moves, a variable one a call.
fn put_num(buf: &mut Vec<u8>, mut v: u64, end: u8) {
    let n = v.checked_ilog10().map_or(1, |d| d as usize + 1);
    let mut tmp = [end; 24];
    for i in (0..n).rev() {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let len = buf.len();
    buf.extend_from_slice(&tmp);
    buf.truncate(len + n + 1);
}


/// Data the threads share with no lock. The phases of an hour are separated by barriers, and
/// within a phase each thread writes only its own part (every use says which).
struct Shared<T>(UnsafeCell<T>);
unsafe impl<T: Send> Sync for Shared<T> {}
impl<T> Shared<T> {
    #[allow(clippy::mut_from_ref)]
    unsafe fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

/// A barrier that spins: the scoring box is ours alone, and an hour has five phases, so a
/// futex sleep and wake per phase would cost more than the work between them.
struct Spin {
    arrived: AtomicUsize,
    round: AtomicUsize,
}

impl Spin {
    fn wait(&self) {
        let round = self.round.load(Acquire);
        if self.arrived.fetch_add(1, AcqRel) + 1 == THREADS {
            self.arrived.store(0, Relaxed);
            self.round.store(round + 1, Release);
        } else {
            while self.round.load(Acquire) == round {
                std::hint::spin_loop();
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let data = map_file(&format!("{data_dir}/requests.log"));
    let len = data.len();
    let file = File::create(format!("{out_dir}/summary.txt")).expect("cannot create summary.txt");

    // Hours present in each block, as two 64-bit halves of a 72-bit mask.
    let blocks = (len >> BLOCK_SHIFT) + 1;
    let masks: Vec<[AtomicU64; 2]> = (0..blocks).map(|_| Default::default()).collect();
    let has = |b: usize, h: usize| masks[b][h / 64].load(Relaxed) >> (h % 64) & 1 != 0;
    let spin = Spin { arrived: AtomicUsize::new(0), round: AtomicUsize::new(0) };
    let global = Mutex::new(Table::new()); // every endpoint seen so far; id = global id
    let names = Shared(UnsafeCell::new(Table::new())); // a copy, read while nobody inserts
    let order = Shared(UnsafeCell::new(Vec::<usize>::new())); // rank -> global id, by name
    // Per thread and global id: lines this hour (then the thread's write cursor), 5xx lines.
    let counts: Vec<_> = (0..THREADS).map(|_| Shared(UnsafeCell::new((vec![0u32; 0], vec![0u32; 0])))).collect();
    let ranks = Shared(UnsafeCell::new((vec![0u32; 0], vec![0u32; 0]))); // per rank: start, 5xx
    let lat = Shared(UnsafeCell::new(vec![0u32; 0])); // the hour's latencies, grouped by rank
    let parts: Vec<AtomicUsize> = (0..THREADS).map(|_| AtomicUsize::new(0)).collect();

    thread::scope(|s| {
        for t in 0..THREADS {
            let (masks, has, spin, global, names, order) = (&masks, &has, &spin, &global, &names, &order);
            let (counts, ranks, lat, parts, file) = (&counts, &ranks, &lat, &parts, &file);
            s.spawn(move || {
                // Pass 1: this thread's chunk. A line belongs to the block its first byte is in.
                let (a, b) = (line_start(data, t * len / THREADS), line_start(data, (t + 1) * len / THREADS));
                let (mut cur, mut m) = (a >> BLOCK_SHIFT, 0u128);
                let (mut key, mut bit) = (0, 0); // hour of the previous line, usually the same
                let flush = |b: usize, m: u128| {
                    masks[b][0].fetch_or(m as u64, Relaxed);
                    masks[b][1].fetch_or((m >> 64) as u64, Relaxed);
                };
                for_each_line(data, a, b, |off, l| {
                    if off >> BLOCK_SHIFT != cur {
                        flush(cur, m);
                        (cur, m) = (off >> BLOCK_SHIFT, 0);
                    }
                    if hour_key(l) != key {
                        (key, bit) = (hour_key(l), 1 << hour_of(l));
                    }
                    m |= bit;
                });
                flush(cur, m);
                spin.wait();

                // Pass 2: all threads work on one hour at a time, in five phases.
                let (mut table, mut to_global) = (Table::new(), Vec::new()); // local id cache
                let (mut recs, mut buf, mut hour_off) = (Vec::new(), Vec::new(), 0u64);
                for h in 0..HOURS {
                    // 1. Parse every THREADS-th block holding hour h, counting per endpoint.
                    // SAFETY: until phase 3, counts[t] is touched by thread t alone.
                    let (cnt, err) = unsafe { counts[t].get() };
                    cnt.fill(0);
                    err.fill(0);
                    let want = hour_key(hour_label(h).as_bytes());
                    for b in (0..blocks).filter(|&b| has(b, h)).skip(t).step_by(THREADS) {
                        let (from, to) = (line_start(data, b << BLOCK_SHIFT), line_start(data, (b + 1) << BLOCK_SHIFT));
                        for_each_line(data, from, to, |_, l| {
                            if hour_key(l) == want {
                                let (ep, is_err, latency) = parse_rest(l);
                                let id = table.id(ep);
                                if id == to_global.len() {
                                    to_global.push(global.lock().unwrap().id(ep));
                                }
                                let g = to_global[id];
                                if g >= cnt.len() {
                                    cnt.resize(g + 1, 0);
                                    err.resize(g + 1, 0);
                                }
                                cnt[g] += 1;
                                err[g] += is_err as u32;
                                recs.push((g as u32, latency));
                            }
                        });
                    }
                    spin.wait();
                    // 2. One thread: copy new names, sort them into the rank order, size arrays.
                    if t == 0 {
                        // SAFETY: every other thread waits at the next barrier.
                        let (names, order, g) = unsafe { (names.get(), order.get(), global.lock().unwrap()) };
                        while names.spans.len() < g.spans.len() {
                            names.id(g.key(names.spans.len()));
                        }
                        // A few new names per hour after the first: insert each where it belongs.
                        let e = names.spans.len();
                        if e - order.len() > 64 {
                            order.extend(order.len()..e);
                            order.sort_unstable_by(|&a, &b| names.key(a).cmp(names.key(b)));
                        }
                        for g in order.len()..e {
                            order.insert(order.partition_point(|&x| names.key(x) < names.key(g)), g);
                        }
                        for c in counts.iter().chain([ranks]) {
                            let (x, y) = unsafe { c.get() };
                            x.resize(e, 0);
                            y.resize(e, 0);
                        }
                    }
                    spin.wait();
                    // 3. Thread t owns the ranks lo..hi: the total of its range, then offsets.
                    // SAFETY: from here to the end of the hour, `order` and `names` are only read.
                    let (order, names) = unsafe { (&*order.get(), &*names.get()) };
                    let e = order.len();
                    let (lo, hi) = (t * e / THREADS, (t + 1) * e / THREADS);
                    let all = |g: usize, f: fn(&(Vec<u32>, Vec<u32>), usize) -> u32| -> u32 {
                        counts.iter().map(|c| f(unsafe { c.get() }, g)).sum()
                    };
                    let mine: u32 = order[lo..hi].iter().map(|&g| all(g, |c, g| c.0[g])).sum();
                    parts[t].store(mine as usize, Relaxed);
                    spin.wait();
                    let total: usize = parts.iter().map(|p| p.load(Relaxed)).sum();
                    let mut pos = parts[..t].iter().map(|p| p.load(Relaxed)).sum::<usize>() as u32;
                    // SAFETY: thread t writes only the entries of its own ranks; lat is sized by
                    // thread 0 while the others are still here, before anyone writes into it.
                    let (start, errs) = unsafe { ranks.get() };
                    if t == 0 {
                        unsafe { lat.get().resize(total, 0) };
                    }
                    for r in lo..hi {
                        let g = order[r];
                        start[r] = pos;
                        errs[r] = all(g, |c, g| c.1[g]);
                        for c in counts {
                            let c = unsafe { &mut c.get().0[g] };
                            (*c, pos) = (pos, pos + *c);
                        }
                    }
                    spin.wait();
                    // 4. Every thread scatters its own lines through its own cursors.
                    // SAFETY: the cursors of different (thread, endpoint) pairs cover disjoint
                    // ranges of the hour buffer, which holds exactly `total` lines.
                    let out = unsafe { lat.get() };
                    for &(g, v) in &recs {
                        unsafe {
                            let c = counts[t].get().0.get_unchecked_mut(g as usize);
                            *out.get_unchecked_mut(*c as usize) = v;
                            *c += 1;
                        }
                    }
                    recs.clear();
                    spin.wait();
                    // 5. Format this thread's ranks, then write them after the earlier ranges.
                    let prefix: [u8; 21] = hour_label(h).as_bytes().try_into().unwrap();
                    for r in lo..hi {
                        let (a, b) = (start[r] as usize, if r + 1 < e { start[r + 1] as usize } else { total });
                        if a == b {
                            continue;
                        }
                        // SAFETY: rank ranges are disjoint, and this thread owns ranks lo..hi.
                        let v = unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr().add(a), b - a) };
                        let n = v.len();
                        buf.extend_from_slice(&prefix);
                        buf.extend_from_slice(names.key(order[r]));
                        buf.push(b' ');
                        put_num(&mut buf, n as u64, b' ');
                        // Small groups (most of them, in the Zipf tail): one sort beats three selects.
                        if n <= 32 {
                            v.sort_unstable();
                        }
                        let mut lo = 0;
                        for p in [50, 95, 99] {
                            let k = (p * n + 99) / 100 - 1;
                            let x = if n <= 32 { v[k] } else { *v[lo..].select_nth_unstable(k - lo).1 };
                            put_num(&mut buf, x as u64, b' ');
                            lo = k;
                        }
                        let errs = errs[r] as u64;
                        put_num(&mut buf, errs, b' ');
                        put_num(&mut buf, errs * 1_000_000 / n as u64, b'\n');
                    }
                    parts[t].store(buf.len(), Relaxed);
                    spin.wait();
                    let before = parts[..t].iter().map(|p| p.load(Relaxed) as u64).sum::<u64>();
                    file.write_all_at(&buf, hour_off + before).unwrap();
                    hour_off += parts.iter().map(|p| p.load(Relaxed) as u64).sum::<u64>();
                    buf.clear();
                    spin.wait(); // `parts` is reused in the next hour
                }
            });
        }
    });
}
