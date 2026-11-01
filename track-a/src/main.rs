//! Track A (Ingest): per (hour, endpoint) request count, p50/p95/p99 latency and 5xx rate.
//!
//! The log is mmapped and split into one chunk per thread on line boundaries, then read twice:
//!   pass 1  counts lines per (endpoint, hour) using a thread-local endpoint table;
//!   merge   sorts the distinct endpoints once, which fixes every group's slot range in one flat
//!           latency array, laid out hour-major then endpoint, which is exactly the output order;
//!   pass 2  re-parses and writes each latency straight into its slot.
//! Latencies are stored once, 4 bytes each, never moved. Percentiles use select_nth_unstable
//! per group instead of a full sort, and hours are formatted in parallel.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

const HOURS: usize = 72; // 2026-10-30T18 .. 2026-11-02T17, fixed by the spec
const THREADS: usize = 12; // the scoring box gives us exactly 12 hardware threads

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).expect("cannot open requests.log");
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // PROT_READ, MAP_PRIVATE: read straight from the page cache, no copy into our heap.
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// Calls `f` on every line (without its LF) in data[start..end]; both bounds are line starts.
/// Newlines are found 32 bytes at a time with AVX2.
#[inline(always)]
fn for_each_line(data: &'static [u8], start: usize, end: usize, mut f: impl FnMut(&'static [u8])) {
    let (mut line, mut pos) = (start, start);
    while pos + 32 <= end {
        let mut mask = unsafe {
            let v = _mm256_loadu_si256(data.as_ptr().add(pos) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32
        };
        while mask != 0 {
            let lf = pos + mask.trailing_zeros() as usize;
            f(&data[line..lf]);
            line = lf + 1;
            mask &= mask - 1;
        }
        pos += 32;
    }
    for i in pos..end {
        if data[i] == b'\n' {
            f(&data[line..i]);
            line = i + 1;
        }
    }
}

/// One line -> (hour index 0..72, endpoint, is 5xx, latency).
/// Layout: `YYYY-MM-DDTHH:MM:SS.ffffffZ METHOD ENDPOINT SSS LATENCY`, the timestamp always 27 bytes.
#[inline(always)]
fn parse(l: &[u8]) -> (usize, &[u8], bool, u32) {
    let d = |i: usize| (l[i] - b'0') as usize;
    // Only four days occur: Oct 30, Oct 31, Nov 1, Nov 2. No date library needed.
    let day = d(8) * 10 + d(9);
    let day_idx = if l[6] == b'0' { day - 30 } else { day + 1 };
    let hour = day_idx * 24 + d(11) * 10 + d(12) - 18;
    // The method (3..7 letters) starts at byte 28; the endpoint starts after the next space.
    let mut ep = 31;
    while l[ep] != b' ' {
        ep += 1;
    }
    // Walk back from the end over the latency digits to the space before them.
    let mut sp = l.len() - 1;
    while l[sp] != b' ' {
        sp -= 1;
    }
    let lat = l[sp + 1..].iter().fold(0u32, |a, &c| a * 10 + (c - b'0') as u32);
    (hour, &l[ep + 1..sp - 4], l[sp - 3] == b'5', lat)
}

/// Open-addressing endpoint -> dense id table, private to one thread.
struct Table {
    slots: Vec<u32>, // id + 1, 0 = empty
    keys: Vec<&'static [u8]>,
}

impl Table {
    fn new() -> Self {
        Table { slots: vec![0; 4096], keys: Vec::new() }
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

    #[inline(always)]
    fn id(&mut self, k: &'static [u8]) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = Self::hash(k) & mask;
        loop {
            match self.slots[i] {
                0 => break,
                s if self.keys[s as usize - 1] == k => return s as usize - 1,
                _ => i = (i + 1) & mask,
            }
        }
        self.keys.push(k);
        self.slots[i] = self.keys.len() as u32;
        if self.keys.len() * 2 > self.slots.len() {
            self.grow();
        }
        self.keys.len() - 1
    }

    fn grow(&mut self) {
        self.slots = vec![0; self.slots.len() * 2];
        let mask = self.slots.len() - 1;
        for (id, k) in self.keys.iter().enumerate() {
            let mut i = Self::hash(k) & mask;
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = id as u32 + 1;
        }
    }
}

/// A raw pointer that worker threads may share; every user writes a disjoint range.
#[derive(Clone, Copy)]
struct Shared(*mut u32);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn put_num(buf: &mut Vec<u8>, mut v: u64, end: u8) {
    let mut tmp = [0u8; 21];
    let mut i = 20;
    tmp[20] = end;
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let data = map_file(&format!("{data_dir}/requests.log"));
    let len = data.len();

    // Chunk bounds, each moved forward to the start of a line.
    let bounds: Vec<usize> = (0..=THREADS)
        .map(|i| {
            let mut p = i * len / THREADS;
            while p > 0 && p < len && data[p - 1] != b'\n' {
                p += 1;
            }
            p
        })
        .collect();

    // Pass 1: per thread, an endpoint table and counts[local_id * HOURS + hour].
    let mut locals: Vec<(Table, Vec<u32>)> = thread::scope(|s| {
        let handles: Vec<_> = bounds
            .windows(2)
            .map(|w| {
                s.spawn(move || {
                    let mut table = Table::new();
                    let mut counts: Vec<u32> = Vec::new();
                    for_each_line(data, w[0], w[1], |l| {
                        let (h, ep, _, _) = parse(l);
                        let id = table.id(ep);
                        if id * HOURS >= counts.len() {
                            counts.resize(counts.len() + HOURS, 0);
                        }
                        counts[id * HOURS + h] += 1;
                    });
                    (table, counts)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge: sorted endpoint names give each (hour, endpoint) group its output position.
    let mut names: Vec<&[u8]> = locals.iter().flat_map(|(t, _)| t.keys.iter().copied()).collect();
    names.sort_unstable();
    names.dedup();
    let e = names.len();
    let ranks: Vec<Vec<usize>> = locals
        .iter()
        .map(|(t, _)| t.keys.iter().map(|k| names.binary_search(k).unwrap()).collect())
        .collect();
    let groups = HOURS * e;
    let mut start = vec![0usize; groups + 1];
    for ((_, counts), rank) in locals.iter().zip(&ranks) {
        for (id, &r) in rank.iter().enumerate() {
            for h in 0..HOURS {
                start[h * e + r + 1] += counts[id * HOURS + h] as usize;
            }
        }
    }
    for g in 0..groups {
        start[g + 1] += start[g];
    }
    // Turn each thread's counts into its own write cursors: thread t's latencies for a group
    // go right after those of threads 0..t.
    let mut cursor = start.clone();
    for ((_, counts), rank) in locals.iter_mut().zip(&ranks) {
        for (id, &r) in rank.iter().enumerate() {
            for h in 0..HOURS {
                let c = &mut counts[id * HOURS + h];
                let g = h * e + r;
                let n = *c as usize;
                *c = cursor[g] as u32;
                cursor[g] += n;
            }
        }
    }

    // Pass 2: scatter latencies into place and count 5xx per group.
    let mut lat = vec![0u32; start[groups]];
    let errors: Vec<AtomicU32> = (0..groups).map(|_| AtomicU32::new(0)).collect();
    let out = Shared(lat.as_mut_ptr());
    thread::scope(|s| {
        for ((w, (table, cursors)), rank) in bounds.windows(2).zip(locals.iter_mut()).zip(&ranks) {
            let errors = &errors;
            s.spawn(move || {
                let out = out;
                for_each_line(data, w[0], w[1], |l| {
                    let (h, ep, is_err, latency) = parse(l);
                    let id = table.id(ep);
                    let c = &mut cursors[id * HOURS + h];
                    unsafe { *out.0.add(*c as usize) = latency };
                    *c += 1;
                    if is_err {
                        errors[h * e + rank[id]].fetch_add(1, Relaxed);
                    }
                });
            });
        }
    });

    // Percentiles and formatting, hours spread across threads.
    let mut per_hour: Vec<Vec<u8>> = vec![Vec::new(); HOURS];
    thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let (start, names, errors) = (&start, &names, &errors);
                s.spawn(move || {
                    let out = out; // capture the whole wrapper, not its raw field
                    let mut done = Vec::new();
                    for h in (t..HOURS).step_by(THREADS) {
                        let abs = 18 + h; // hours since 2026-10-30T00
                        let days = ["2026-10-30T", "2026-10-31T", "2026-11-01T", "2026-11-02T"];
                        let prefix = format!("{}{:02}:00:00Z ", days[abs / 24], abs % 24);
                        let mut buf = Vec::new();
                        for r in 0..e {
                            let g = h * e + r;
                            let (a, b) = (start[g], start[g + 1]);
                            if a == b {
                                continue;
                            }
                            // Groups of different hours are disjoint ranges of `lat`.
                            let v = unsafe { std::slice::from_raw_parts_mut(out.0.add(a), b - a) };
                            let n = v.len();
                            buf.extend_from_slice(prefix.as_bytes());
                            buf.extend_from_slice(names[r]);
                            buf.push(b' ');
                            put_num(&mut buf, n as u64, b' ');
                            // Nearest rank: 0-based index ceil(p*n/100) - 1. Each select leaves
                            // everything from that index on in v[lo..], so the next search is smaller.
                            let mut lo = 0;
                            for p in [50, 95, 99] {
                                let i = (p * n + 99) / 100 - 1;
                                let x = *v[lo..].select_nth_unstable(i - lo).1;
                                put_num(&mut buf, x as u64, b' ');
                                lo = i;
                            }
                            let errs = errors[g].load(Relaxed) as u64;
                            put_num(&mut buf, errs, b' ');
                            put_num(&mut buf, errs * 1_000_000 / n as u64, b'\n');
                        }
                        done.push((h, buf));
                    }
                    done
                })
            })
            .collect();
        for h in handles {
            for (hour, buf) in h.join().unwrap() {
                per_hour[hour] = buf;
            }
        }
    });

    let mut f = File::create(format!("{out_dir}/summary.txt")).expect("cannot create summary.txt");
    for buf in &per_hour {
        f.write_all(buf).unwrap();
    }
}
