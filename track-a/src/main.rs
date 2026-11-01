//! Track A (Ingest): per (hour, endpoint) request count, p50/p95/p99 latency and 5xx rate.
//!
//! The log is mmapped and read once:
//!   parse   one chunk per thread; every line becomes a 6-byte record (endpoint id with the
//!           5xx flag, latency) in that thread's bucket for the line's hour;
//!   names   all threads' endpoints are deduplicated and sorted once, which maps every local
//!           id to its rank, the output order inside an hour;
//!   finish  hours are handed out in order; a worker counting-sorts the hour's records from
//!           all threads by rank, frees them, picks the percentiles and writes the hour at
//!           its file offset once every earlier hour has claimed its place.
//! Disorder in the log never matters: a line simply goes to its hour's bucket.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
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
        let d2 = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A)); // byte pairs: 10 * a + b
        let d4 = _mm_madd_epi16(d2, _mm_set1_epi32(0x0001_0064)); // pairs: 100 * a + b
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
        let x = _mm256_loadu_si256(a.add(i) as *const __m256i);
        let y = _mm256_loadu_si256(b.add(i) as *const __m256i);
        let equal = _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32;
        let wanted = u32::MAX >> 32usize.saturating_sub(n - i); // the bytes before n
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
/// An endpoint id with the 5xx flag in bit 0: u16 (6-byte records) normally, u32 when an
/// input has more than 32,767 distinct endpoints (the spec's catalogue has 1,842).
trait Id: Copy + Default + Send + TryFrom<usize> + Into<u32> {}
impl Id for u16 {}
impl Id for u32 {}

/// One thread's lines of one hour: `ids[i]` is endpoint id << 1 | is_5xx, `lat[i]` the latency.
#[derive(Default)]
struct Bucket<T> {
    ids: Vec<T>,
    lat: Vec<u32>,
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

/// Formats hour `h` from its buckets (one per parse thread); `rank[t]` maps thread t's local
/// endpoint ids to positions in `names`. `count` and `lat` are scratch space reused per hour.
fn format_hour<T: Id>(h: usize, mut buckets: Vec<Bucket<T>>, rank: &[Vec<u32>], names: &[&[u8]], count: &mut Vec<u32>, lat: &mut Vec<u32>) -> Vec<u8> {
    let e = names.len();
    count.clear();
    count.resize(2 * e, 0); // [lines per rank | errors per rank]
    let mut total = 0;
    for (b, rank) in buckets.iter_mut().zip(rank) {
        for x in b.ids.iter_mut() {
            let v: u32 = (*x).into();
            let r = rank[v as usize >> 1] as usize;
            count[r] += 1;
            count[e + r] += v & 1;
            *x = T::try_from(r).ok().unwrap(); // remap to the global rank once, for the scatter
        }
        total += b.ids.len();
    }
    // Counting sort: each endpoint gets a contiguous range of `lat`, ranges in name order.
    let mut pos = 0;
    for c in &mut count[..e] {
        (*c, pos) = (pos, pos + *c);
    }
    lat.resize(total, 0);
    for b in buckets {
        for (&r, &v) in b.ids.iter().zip(&b.lat) {
            let r: u32 = r.into();
            lat[count[r as usize] as usize] = v;
            count[r as usize] += 1;
        }
    } // the records of this hour are freed here
    let prefix = hour_label(h);
    let mut buf = Vec::with_capacity(total * 40);
    let mut a = 0;
    for r in 0..e {
        let b = count[r] as usize; // after the scatter this is the end of rank r's range
        if a == b {
            continue;
        }
        let v = &mut lat[a..b];
        let n = v.len();
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend_from_slice(names[r]);
        buf.push(b' ');
        put_num(&mut buf, n as u64, b' ');
        // Nearest rank: 0-based index ceil(p*n/100) - 1. Each select leaves everything from
        // that index on in v[lo..], so the next search is smaller.
        let mut lo = 0;
        for p in [50, 95, 99] {
            let k = (p * n + 99) / 100 - 1;
            let x = *v[lo..].select_nth_unstable(k - lo).1;
            put_num(&mut buf, x as u64, b' ');
            lo = k;
        }
        let errs = count[e + r] as u64;
        put_num(&mut buf, errs, b' ');
        put_num(&mut buf, errs * 1_000_000 / n as u64, b'\n');
        a = b;
    }
    buf
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let data = map_file(&format!("{data_dir}/requests.log"));
    let file = File::create(format!("{out_dir}/summary.txt")).expect("cannot create summary.txt");
    // Compact ids first; only an input with too many endpoints for them pays for a second run.
    let _ = run::<u16>(data, &file) || run::<u32>(data, &file);
}

/// The whole job with ids of type `T`. Returns false, having written nothing, if some id
/// does not fit in `T`.
fn run<T: Id>(data: &'static [u8], file: &File) -> bool {
    let len = data.len();

    // Parse: per thread, an endpoint table and one bucket per hour.
    let parsed: Vec<(Table, Vec<Bucket<T>>, bool)> = thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                s.spawn(move || {
                    let a = line_start(data, t * len / THREADS);
                    let b = line_start(data, (t + 1) * len / THREADS);
                    let mut table = Table::new();
                    let mut hours: Vec<Bucket<T>> = (0..HOURS).map(|_| Bucket::default()).collect();
                    let mut overflow = false;
                    // Hour of the previous line: bytes 5..13 (`MM-DDTHH`) nearly always repeat.
                    let (mut key, mut h) = (0u64, 0);
                    for_each_line(data, a, b, |_, l| {
                        if l[5..13] != key.to_le_bytes() {
                            (key, h) = (u64::from_le_bytes(l[5..13].try_into().unwrap()), hour_of(l));
                        }
                        let (ep, is_err, latency) = parse_rest(l);
                        let Ok(id) = T::try_from(table.id(ep) << 1 | is_err as usize) else {
                            overflow = true;
                            return;
                        };
                        hours[h].ids.push(id);
                        hours[h].lat.push(latency);
                    });
                    (table, hours, overflow)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Names: dedupe all threads' endpoints through one table, sort the distinct ones, and map
    // every thread's local ids to the sorted position.
    if parsed.iter().any(|p| p.2) {
        return false;
    }
    let mut all = Table::new();
    for (table, _, _) in &parsed {
        for k in (0..table.spans.len()).map(|i| table.key(i)) {
            all.id(k);
        }
    }
    if T::try_from(all.spans.len() << 1).is_err() {
        return false;
    }
    let mut order: Vec<usize> = (0..all.spans.len()).collect();
    order.sort_unstable_by_key(|&i| all.key(i));
    let mut global_rank = vec![0u32; order.len()];
    for (r, &i) in order.iter().enumerate() {
        global_rank[i] = r as u32;
    }
    let mut rank = Vec::new();
    let mut by_hour: Vec<Mutex<Vec<Bucket<T>>>> = (0..HOURS).map(|_| Mutex::default()).collect();
    for (table, hours, _) in parsed {
        let local = (0..table.spans.len()).map(|i| global_rank[all.id(table.key(i))]);
        rank.push(local.collect::<Vec<u32>>());
        for (h, b) in hours.into_iter().enumerate() {
            by_hour[h].get_mut().unwrap().push(b);
        }
    }
    let names: Vec<&[u8]> = order.iter().map(|&i| all.key(i)).collect();

    // Finish: hours handed out in increasing order, so the writer rarely has to wait.
    let next_hour = AtomicUsize::new(0);
    // Hours finish out of order. Each finished hour waits in `pending` until every earlier hour
    // has claimed its place, so file offsets go out in output order: (next hour, its offset).
    let out = Mutex::new((0, 0, vec![None; HOURS]));
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let (mut count, mut lat) = (Vec::new(), Vec::new());
                loop {
                    let h = next_hour.fetch_add(1, Relaxed);
                    if h >= HOURS {
                        break;
                    }
                    let buckets = std::mem::take(&mut *by_hour[h].lock().unwrap());
                    let buf = format_hour(h, buckets, &rank, &names, &mut count, &mut lat);
                    // Under the lock only claim file offsets; the writes themselves run unlocked.
                    let mut ready = Vec::new();
                    {
                        let (next, offset, pending) = &mut *out.lock().unwrap();
                        pending[h] = Some(buf);
                        while let Some(done) = pending.get_mut(*next).and_then(Option::take) {
                            *offset += done.len() as u64;
                            ready.push((*offset - done.len() as u64, done));
                            *next += 1;
                        }
                    }
                    for (off, done) in ready {
                        file.write_all_at(&done, off).unwrap();
                    }
                }
            });
        }
    });
    true
}
