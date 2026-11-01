//! Track A (Ingest): per (hour, endpoint) request count, p50/p95/p99 latency and 5xx rate.
//!
//! The log is mmapped and read twice, but only the second read does real work:
//!   pass 1  (one chunk per thread) reads just the hour of every line and records, per 16 KiB
//!           block of the file, a 72-bit mask of the hours present in it;
//!   pass 2  (one hour at a time, handed out in order) parses only the blocks whose mask has
//!           that hour, groups the hour's latencies by endpoint, picks the percentiles and
//!           appends the formatted lines to the output as soon as all earlier hours are out.
//! Nothing ever holds more than one hour per thread, so the heap stays a few MB. The log is
//! only "mostly" time ordered; the masks make that a speed property, never a correctness one.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Barrier, Mutex};
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
/// line starts. Newlines are found 64 bytes at a time with AVX2.
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
            f(line, &data[line..lf]);
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
    let d = |i: usize| (l[i] - b'0') as usize;
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
        (&l[ep..sp - 4], l[sp - 3] == b'5', lat)
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
/// arena offset << 32 | length << 16 | id + 1 (0 = empty).
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
            let s = self.slots[i];
            if s == 0 {
                break;
            }
            let (start, n) = ((s >> 32) as usize, (s >> 16) as u16 as usize);
            if n == k.len() && unsafe { same(self.arena.as_ptr().add(start), k.as_ptr(), n) } {
                return (s as u16 - 1) as usize;
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
        (start as u64) << 32 | (len as u64) << 16 | id1 as u64
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

/// Per-thread state for pass 2, reused from one hour to the next.
#[derive(Default)]
struct HourWork {
    table: Option<Table>,
    order: Vec<usize>,  // endpoint ids sorted by name, i.e. output order
    ids: Vec<u16>,      // this hour's lines: endpoint id ...
    lines: Vec<u32>,    // ... and latency
    counts: Vec<u32>,   // per id: lines this hour, then end cursor of its latency range
    errors: Vec<u32>,   // per id: 5xx lines this hour
    lat: Vec<u32>,      // this hour's latencies, grouped by endpoint in output order
}

impl HourWork {
    /// Parses every line of hour `h` in `data[start..end]`.
    fn collect(&mut self, data: &[u8], start: usize, end: usize, h: usize) {
        let want = hour_key(hour_label(h).as_bytes());
        let table = self.table.get_or_insert_with(Table::new);
        for_each_line(data, start, end, |_, l| {
            if hour_key(l) == want {
                let (ep, is_err, latency) = parse_rest(l);
                let id = table.id(ep);
                self.ids.push((id << 1 | is_err as usize) as u16);
                self.lines.push(latency);
            }
        });
    }

    /// Groups the collected lines by endpoint and appends the hour's output to `buf`.
    fn finish(&mut self, h: usize, buf: &mut Vec<u8>) {
        // A thread that has not parsed a line yet has no table and nothing to write.
        let Some(table) = self.table.as_ref() else { return };
        let n = table.spans.len();
        // Ids are handed out in first-seen order; keep `order` sorted as new names show up.
        // The old part is already sorted, so this is a cheap merge after the first hour.
        if self.order.len() < n {
            self.order.extend(self.order.len()..n);
            self.order.sort_by(|&a, &b| table.key(a).cmp(table.key(b)));
        }
        self.counts.clear();
        self.counts.resize(n, 0);
        self.errors.clear();
        self.errors.resize(n, 0);
        for &x in &self.ids {
            self.counts[x as usize >> 1] += 1;
            self.errors[x as usize >> 1] += x as u32 & 1;
        }
        // Counting sort: each endpoint gets a contiguous range of `lat`, ranges in name order.
        let mut pos = 0;
        for &i in &self.order {
            let c = self.counts[i];
            self.counts[i] = pos;
            pos += c;
        }
        self.lat.resize(self.lines.len(), 0);
        for (&x, &v) in self.ids.iter().zip(&self.lines) {
            let c = &mut self.counts[x as usize >> 1];
            self.lat[*c as usize] = v;
            *c += 1;
        }
        let prefix: [u8; 21] = hour_label(h).as_bytes().try_into().unwrap();
        let mut a = 0;
        for &i in &self.order {
            let b = self.counts[i] as usize; // after the scatter this is the range's end
            if a == b {
                continue;
            }
            let v = &mut self.lat[a..b];
            let cnt = v.len();
            buf.extend_from_slice(&prefix);
            buf.extend_from_slice(table.key(i));
            buf.push(b' ');
            put_num(buf, cnt as u64, b' ');
            // Nearest rank: 0-based index ceil(p*n/100) - 1. Each select leaves everything
            // from that index on in v[lo..], so the next search is smaller.
            let mut lo = 0;
            for p in [50, 95, 99] {
                let k = (p * cnt + 99) / 100 - 1;
                let x = *v[lo..].select_nth_unstable(k - lo).1;
                put_num(buf, x as u64, b' ');
                lo = k;
            }
            let errs = self.errors[i] as u64;
            put_num(buf, errs, b' ');
            put_num(buf, errs * 1_000_000 / cnt as u64, b'\n');
            a = b;
        }
        self.ids.clear();
        self.lines.clear();
    }
}

/// Hours finish out of order. Each finished hour waits here until every earlier hour has
/// claimed its place in the file, so file offsets are handed out in output order.
struct Output {
    next: usize,
    offset: u64,
    pending: Vec<Option<Vec<u8>>>,
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
    let next_hour = AtomicUsize::new(0);
    let out = Mutex::new(Output { next: 0, offset: 0, pending: vec![None; HOURS] });
    let barrier = Barrier::new(THREADS);

    // One set of threads for both passes; the barrier separates them.
    thread::scope(|s| {
        for t in 0..THREADS {
            let (masks, has, next_hour, out, barrier, file) = (&masks, &has, &next_hour, &out, &barrier, &file);
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
                barrier.wait();

                // Pass 2: hours handed out in increasing order, so the writer rarely has to wait.
                let mut work = HourWork::default();
                loop {
                    let h = next_hour.fetch_add(1, Relaxed);
                    if h >= HOURS {
                        break;
                    }
                    // Parse each run of consecutive blocks that contain hour h.
                    let mut b = 0;
                    while b < blocks {
                        if !has(b, h) {
                            b += 1;
                            continue;
                        }
                        let first = b;
                        while b < blocks && has(b, h) {
                            b += 1;
                        }
                        work.collect(data, line_start(data, first << BLOCK_SHIFT), line_start(data, b << BLOCK_SHIFT), h);
                    }
                    let mut buf = Vec::with_capacity(256 << 10);
                    work.finish(h, &mut buf);
                    // Under the lock only claim file offsets; the writes themselves run unlocked.
                    let mut ready = Vec::new();
                    {
                        let mut o = out.lock().unwrap();
                        o.pending[h] = Some(buf);
                        while o.next < HOURS {
                            let i = o.next;
                            let Some(done) = o.pending[i].take() else { break };
                            ready.push((o.offset, done));
                            o.offset += ready.last().unwrap().1.len() as u64;
                            o.next += 1;
                        }
                    }
                    for (off, done) in ready {
                        file.write_all_at(&done, off).unwrap();
                    }
                }
            });
        }
    });
}
