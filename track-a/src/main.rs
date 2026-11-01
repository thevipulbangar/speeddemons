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
use std::sync::{Barrier, Condvar, Mutex};
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
    while pos + 32 <= end {
        let mut mask = unsafe {
            let v = _mm256_loadu_si256(data.as_ptr().add(pos) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32
        };
        while mask != 0 {
            let lf = pos + mask.trailing_zeros() as usize;
            f(line, &data[line..lf]);
            line = lf + 1;
            mask &= mask - 1;
        }
        pos += 32;
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

const HOUR_LABEL_LEN: u64 = 21; // the length of an hour label below

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
/// arena offset << 32 | (id + 1) << 8 | length (0 = empty; endpoints are at most 125 bytes).
struct Table {
    slots: Vec<u64>,
    keys: Vec<(&'static [u8], u32)>, // id -> (the name in the mapping, its arena offset)
    arena: Vec<u8>,                  // key copies, each followed by 32 zero bytes of padding
}

impl Table {
    fn new() -> Self {
        Table { slots: vec![0; 4096], keys: Vec::new(), arena: Vec::new() }
    }

    fn key(&self, id: usize) -> &'static [u8] {
        self.keys[id].0
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
    fn id(&mut self, k: &'static [u8]) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = Self::hash(k) & mask;
        loop {
            let s = self.slots[i];
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
        self.keys.push((k, start as u32));
        self.slots[i] = Self::slot(start, k.len(), self.keys.len());
        if self.keys.len() * 2 > self.slots.len() {
            self.grow();
        }
        self.keys.len() - 1
    }

    fn slot(start: usize, len: usize, id1: usize) -> u64 {
        (start as u64) << 32 | (id1 as u64) << 8 | len as u64
    }

    fn grow(&mut self) {
        self.slots = vec![0; self.slots.len() * 2];
        let mask = self.slots.len() - 1;
        for (id, &(k, start)) in self.keys.iter().enumerate() {
            let mut i = Self::hash(k) & mask;
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = Self::slot(start as usize, k.len(), id + 1);
        }
    }
}

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

/// One output line before formatting: (hour, endpoint) group statistics.
struct Row {
    name: &'static [u8],
    n: u32,
    p: [u32; 3], // p50, p95, p99
    errors: u32,
}

impl Row {
    fn ppm(&self) -> u64 {
        self.errors as u64 * 1_000_000 / self.n as u64
    }

    /// The formatted line's length, known without formatting it.
    fn len(&self) -> u64 {
        let digits = |v: u64| v.checked_ilog10().map_or(1, |d| d as u64 + 1);
        let nums = [self.n, self.p[0], self.p[1], self.p[2], self.errors];
        let total: u64 = nums.iter().map(|&v| digits(v as u64)).sum::<u64>() + digits(self.ppm());
        HOUR_LABEL_LEN + self.name.len() as u64 + 1 + total + 6
    }
}

/// Formats an hour's rows and writes them at `off`, through a small reused buffer.
fn write_hour(file: &File, mut off: u64, h: usize, rows: &[Row], buf: &mut Vec<u8>) {
    let prefix = hour_label(h);
    for r in rows {
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend_from_slice(r.name);
        buf.push(b' ');
        for v in [r.n, r.p[0], r.p[1], r.p[2], r.errors] {
            put_num(buf, v as u64, b' ');
        }
        put_num(buf, r.ppm(), b'\n');
        if buf.len() >= 1 << 15 {
            file.write_all_at(buf, off).unwrap();
            off += buf.len() as u64;
            buf.clear();
        }
    }
    file.write_all_at(buf, off).unwrap();
    buf.clear();
}

/// Per-thread state for pass 2, reused from one hour to the next.
#[derive(Default)]
struct HourWork {
    table: Option<Table>,
    order: Vec<usize>, // endpoint ids sorted by name, i.e. output order
    recs: Vec<u64>,    // this hour's lines: endpoint id << 33 | latency << 1 | is_5xx
    start: Vec<usize>, // per id: where its group begins in `recs` once grouped
    next: Vec<usize>,  // per id: next slot of its group to fill while grouping
    end: Vec<usize>,   // per id: where its group ends
}

impl HourWork {
    /// Parses every line of hour `h` in `data[start..end]`.
    fn collect(&mut self, data: &'static [u8], start: usize, end: usize, h: usize) {
        let want = hour_key(hour_label(h).as_bytes());
        let table = self.table.get_or_insert_with(Table::new);
        for_each_line(data, start, end, |_, l| {
            if hour_key(l) == want {
                let (ep, is_err, latency) = parse_rest(l);
                let id = table.id(ep) as u64;
                self.recs.push(id << 33 | (latency as u64) << 1 | is_err as u64);
            }
        });
    }

    /// Groups the collected lines by endpoint into `rows`, in output order; returns their
    /// formatted size in bytes.
    fn finish(&mut self, rows: &mut Vec<Row>) -> u64 {
        // A thread that has not parsed a line yet has no table and nothing to write.
        let Some(table) = self.table.as_ref() else { return 0 };
        let n = table.keys.len();
        // Ids are handed out in first-seen order; keep `order` sorted as new names show up.
        // The old part is already sorted, so this is a cheap merge after the first hour.
        if self.order.len() < n {
            self.order.extend(self.order.len()..n);
            self.order.sort_by(|&a, &b| table.key(a).cmp(table.key(b)));
        }
        let (recs, start, next, end) = (&mut self.recs, &mut self.start, &mut self.next, &mut self.end);
        for v in [&mut *start, &mut *next, &mut *end] {
            v.clear();
            v.resize(n, 0);
        }
        for &r in recs.iter() {
            end[(r >> 33) as usize] += 1;
        }
        let mut pos = 0;
        for &i in &self.order {
            (start[i], next[i], pos) = (pos, pos, pos + end[i]);
            end[i] = pos;
        }
        // In-place counting sort (American flag): take the record at a group's next free
        // slot and swap it towards where it belongs until one for this group comes back.
        // Groups end up contiguous, in name order, with no second buffer.
        for &i in &self.order {
            while next[i] < end[i] {
                let mut r = recs[next[i]];
                loop {
                    let g = (r >> 33) as usize;
                    if g == i {
                        break;
                    }
                    std::mem::swap(&mut r, &mut recs[next[g]]);
                    next[g] += 1;
                }
                recs[next[i]] = r;
                next[i] += 1;
            }
        }
        let mut bytes = 0;
        for &i in &self.order {
            let v = &mut recs[start[i]..end[i]];
            let cnt = v.len();
            if cnt == 0 {
                continue;
            }
            // Within a group the id bits are equal, so records order by latency. Nearest
            // rank: 0-based index ceil(p*n/100) - 1. Each select leaves everything from that
            // index on in v[lo..], so the next search is smaller.
            let (mut lo, mut p) = (0, [0; 3]);
            for (q, pct) in p.iter_mut().zip([50, 95, 99]) {
                let k = (pct * cnt + 99) / 100 - 1;
                *q = (*v[lo..].select_nth_unstable(k - lo).1 >> 1) as u32;
                lo = k;
            }
            let errors = v.iter().map(|&r| r as u32 & 1).sum();
            let row = Row { name: table.key(i), n: cnt as u32, p, errors };
            bytes += row.len();
            rows.push(row);
        }
        recs.clear();
        bytes
    }
}

/// Hours finish out of order. A finished hour waits here as compact rows until every earlier
/// hour has claimed its place in the file; from then on any thread may format and write it.
struct Output {
    next: usize,                           // first hour without a file offset
    offset: u64,                           // where hour `next` starts
    pending: Vec<Option<(Vec<Row>, u64)>>, // finished hours: rows and formatted size
    ready: Vec<(usize, u64, Vec<Row>)>,    // hours with an offset, not written yet
}

struct Writer {
    out: Mutex<Output>,
    wake: Condvar,
}

impl Writer {
    fn finished(&self, h: usize, rows: Vec<Row>, bytes: u64) {
        let mut o = self.out.lock().unwrap();
        o.pending[h] = Some((rows, bytes));
        while o.next < HOURS {
            let i = o.next;
            let Some((rows, bytes)) = o.pending[i].take() else { return };
            let item = (i, o.offset, rows);
            o.ready.push(item);
            o.offset += bytes;
            o.next += 1;
            self.wake.notify_all();
        }
    }

    /// An hour that is ready to write. With `wait`, blocks until there is one, and returns
    /// None only once every hour has been handed out.
    fn take(&self, wait: bool) -> Option<(usize, u64, Vec<Row>)> {
        let mut o = self.out.lock().unwrap();
        loop {
            if let Some(item) = o.ready.pop() {
                return Some(item);
            }
            if !wait || o.next == HOURS {
                return None;
            }
            o = self.wake.wait(o).unwrap();
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
    let next_hour = AtomicUsize::new(0);
    let out = Output { next: 0, offset: 0, pending: (0..HOURS).map(|_| None).collect(), ready: Vec::new() };
    let writer = Writer { out: Mutex::new(out), wake: Condvar::new() };
    let barrier = Barrier::new(THREADS);

    // One set of threads for both passes; the barrier separates them.
    thread::scope(|s| {
        for t in 0..THREADS {
            let (masks, has, next_hour, writer, barrier, file) = (&masks, &has, &next_hour, &writer, &barrier, &file);
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
                let (mut work, mut buf) = (HourWork::default(), Vec::new());
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
                    // An hour's size is known before it is formatted, so only the file offset
                    // is claimed in order; formatting and writing run unlocked, through a
                    // small buffer, by whichever thread is free.
                    let mut rows = Vec::new();
                    let bytes = work.finish(&mut rows);
                    writer.finished(h, rows, bytes);
                    while let Some((h, off, rows)) = writer.take(false) {
                        write_hour(file, off, h, &rows, &mut buf);
                    }
                }
                while let Some((h, off, rows)) = writer.take(true) {
                    write_hour(file, off, h, &rows, &mut buf);
                }
            });
        }
    });
}
