//! Track A (Ingest): per (hour, endpoint) request count, p50/p95/p99 latency and 5xx rate.
//!
//! The log is mmapped and read once:
//!   parse   one chunk per thread; every line becomes a 6-byte record (endpoint id with the
//!           5xx flag, latency) in that thread's bucket for the line's hour, inside one
//!           reservation sized for the chunk;
//!   names   all threads' endpoints are deduplicated and sorted once, which maps every local
//!           id to its rank, the output order inside an hour;
//!   finish  hours are handed out in order; a worker counting-sorts the hour's records from
//!           all threads by rank, frees them, picks the percentiles and the hour's exact text
//!           size; then the output file is mapped at its final size and every hour is
//!           formatted straight into its place.
//! Disorder in the log never matters: a line simply goes to its hour's bucket.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::thread;

const HOURS: usize = 72; // 2026-10-30T18 .. 2026-11-02T17, fixed by the spec
const THREADS: usize = 12; // the scoring box gives us exactly 12 hardware threads

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).expect("cannot open requests.log");
    let len = f.metadata().unwrap().len() as usize;
    // Reserve the file's size plus a spare page of zeros (anonymous, PROT_READ), then map the
    // file over the front (MAP_PRIVATE | MAP_FIXED). Reads straight from the page cache, no
    // copy into our heap, and SIMD loads may run a few bytes past the last line.
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 4096, 1, 0x22, -1, 0);
        assert!(p as isize != -1, "mmap failed");
        let q = mmap(p, len.max(1), 1, 0x12, f.as_raw_fd(), 0); // an empty log: one unread byte
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
        // SAFETY: 29 <= ep and the status's 3 digits and a space lie between ep and sp.
        let is_err = *l.get_unchecked(sp - 3) == b'5';
        (l.get_unchecked(ep..sp - 4), is_err, lat)
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

/// `bytes` of zeroed memory, backed only where written (MAP_NORESERVE), in 4 KB pages: where
/// transparent huge pages are "always", every touched 2 MB extent of a sparse reservation would
/// otherwise be backed and charged whole (MADV_NOHUGEPAGE; step 15c in PERF.md).
fn anon(bytes: usize) -> *mut u8 {
    let p = unsafe { mmap(std::ptr::null_mut(), bytes, 3, 0x4022, -1, 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { madvise(p, bytes, 15) };
    p
}

/// Endpoint -> dense id, one open-addressing table shared by every parse thread, so ids are
/// global and no merge is needed. Keys are copied once into a shared arena, each preceded by
/// its id as 4 bytes. A slot packs arena offset << 32 | 24-bit hash tag << 8 | length, so a
/// probe that meets another key nearly always fails on one compare, without reading the arena.
/// A new key claims its empty slot with a compare-and-swap to BUSY, is copied, and is then
/// published; a thread meeting BUSY waits for that (once per key at most).
struct Table {
    slots: &'static [AtomicU64],
    arena: *mut u8,
    used: AtomicUsize, // arena bytes taken
    ids: AtomicUsize,  // keys so far
}
unsafe impl Sync for Table {}
const BUSY: u64 = u64::MAX;

impl Table {
    /// Room for `keys` distinct keys (half the slots, a power of two) and `bytes` of them.
    fn new(keys: usize, bytes: usize) -> Self {
        let n = (2 * keys).next_power_of_two();
        assert!(bytes < 1 << 32, "arena offsets are 32 bits");
        let slots = unsafe { std::slice::from_raw_parts(anon(8 * n) as *const AtomicU64, n) };
        // 64 spare bytes: `same` may read up to 31 bytes past the last key.
        Table { slots, arena: anon(bytes + 64), used: AtomicUsize::new(0), ids: AtomicUsize::new(0) }
    }

    /// Hash from the first and last 8 bytes plus the length: no loop over the string.
    fn hash(k: &[u8]) -> u64 {
        let n = k.len();
        let word = |s: &[u8]| {
            let mut b = [0u8; 8];
            b[..s.len()].copy_from_slice(s);
            u64::from_le_bytes(b)
        };
        let (a, b) = if n >= 8 { (word(&k[..8]), word(&k[n - 8..])) } else { (word(k), 0) };
        (a ^ b.rotate_left(29) ^ n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    /// The key and id of a published slot.
    fn entry(&self, s: u64) -> (&'static [u8], usize) {
        unsafe {
            let k = self.arena.add((s >> 32) as usize);
            let id = (k.sub(4) as *const u32).read_unaligned() as usize;
            (std::slice::from_raw_parts(k, s as u8 as usize), id)
        }
    }

    /// The id of `k`, which must be followed by 31 readable bytes (true for any endpoint in the
    /// padded mapping); usize::MAX / 2 if the table is full, which makes the run fall back.
    #[inline(always)]
    fn id(&self, k: &[u8]) -> usize {
        let (h, n) = (Self::hash(k), k.len());
        // Tag (product bits 16..40) and length, as in a slot's low half; the index from bits 40..
        let (want, mask) = ((h >> 8) as u32 & !0xFF | n as u32, self.slots.len() - 1);
        let mut i = (h >> 40) as usize & mask;
        loop {
            // SAFETY: i is masked by slots.len() - 1, and slots.len() is a power of two.
            let slot = unsafe { self.slots.get_unchecked(i) };
            let mut s = slot.load(Acquire);
            if s == 0 {
                if self.ids.load(Relaxed) * 2 >= self.slots.len() {
                    return usize::MAX / 2;
                }
                match slot.compare_exchange(0, BUSY, Acquire, Acquire) {
                    // SAFETY: the arena has room for every key with its id (Table::new).
                    Ok(_) => unsafe {
                        let id = self.ids.fetch_add(1, Relaxed);
                        let at = self.used.fetch_add(4 + n, Relaxed) + 4;
                        (self.arena.add(at - 4) as *mut u32).write_unaligned(id as u32);
                        std::ptr::copy_nonoverlapping(k.as_ptr(), self.arena.add(at), n);
                        slot.store((at as u64) << 32 | want as u64, Release);
                        return id;
                    },
                    Err(v) => s = v,
                }
            }
            while s == BUSY {
                std::hint::spin_loop();
                s = slot.load(Acquire);
            }
            let at = unsafe { self.arena.add((s >> 32) as usize) };
            if s as u32 == want && unsafe { same(at, k.as_ptr(), n) } {
                return self.entry(s).1;
            }
            i = (i + 1) & mask;
        }
    }
}

/// An endpoint id with the 5xx flag in bit 0: u16 (6-byte records) normally, u32 when an
/// input has more than 32,767 distinct endpoints (the spec's catalogue has 1,842).
trait Id: Copy + Default + Send + 'static + TryFrom<usize> + Into<u32> {}
impl Id for u16 {}
impl Id for u32 {}

/// One thread's lines of one hour: `ids[i]` is endpoint id << 1 | is_5xx, `lat[i]` the latency.
/// Both arrays live in one block reserved up front (MAP_NORESERVE: only pages written count),
/// with room for every line of the thread's chunk, so a record is stored without a capacity
/// check, and nothing is ever reallocated (realloc remaps under the process-wide mmap lock).
struct Bucket<T: 'static> {
    ids: &'static mut [T],
    lat: &'static mut [u32],
    n: usize,
}

impl<T: 'static> Bucket<T> {
    /// `HOURS` buckets with room for `lines` records each.
    fn reserve(lines: usize) -> Vec<Self> {
        let cap = (lines + 2048) & !2047; // keeps both arrays on whole pages
        let block = cap * (size_of::<T>() + 4);
        let p = anon(HOURS * block);
        // SAFETY: each bucket's two arrays are its own disjoint part of the reservation.
        let bucket = |h: usize| unsafe {
            let q = p.add(h * block);
            let ids = std::slice::from_raw_parts_mut(q as *mut T, cap);
            let lat = std::slice::from_raw_parts_mut(q.add(cap * size_of::<T>()) as *mut u32, cap);
            Bucket { ids, lat, n: 0 }
        };
        (0..HOURS).map(bucket).collect()
    }

    /// Hands the written pages back (MADV_DONTNEED).
    fn free(self) {
        let page = |n: usize| (n + 4095) & !4095;
        let n = self.n;
        unsafe { madvise(self.ids.as_mut_ptr().cast(), page(n * size_of::<T>()), 4) };
        unsafe { madvise(self.lat.as_mut_ptr().cast(), page(n * 4), 4) };
    }
}

/// Decimal digits of `v`.
fn digits(v: u32) -> usize {
    v.checked_ilog10().map_or(1, |d| d as usize + 1)
}

/// Writes `v` in decimal and then `end` at `*o`, exactly `digits(v) + 1` bytes, and advances `*o`.
/// SAFETY: the caller sized the destination for it.
unsafe fn put_num(o: &mut *mut u8, mut v: u32, end: u8) {
    let n = digits(v);
    for i in (0..n).rev() {
        *o.add(i) = b'0' + (v % 10) as u8;
        v /= 10;
    }
    *o.add(n) = end;
    *o = o.add(n + 1);
}

/// One output line before formatting: rank, count, p50, p95, p99, errors.
type Row = [u32; 6];
/// An hour's rows and their size as text.
type Rows = (Vec<Row>, usize);

/// The rows of one hour `hr` from its buckets (one per parse thread), and their size as text;
/// `rank` maps endpoint ids to positions in `names`. `lat` is scratch space reused per hour.
fn tally<T: Id>(mut hr: Vec<Bucket<T>>, rank: &[u32], names: &[&[u8]], lat: &mut Vec<u32>) -> Rows {
    let e = names.len();
    let mut count = vec![0u32; 2 * e]; // [lines per rank | errors per rank]
    let mut total = 0;
    for b in &mut hr {
        for x in &mut b.ids[..b.n] {
            let v: u32 = (*x).into();
            // SAFETY: every id came from the table, which `rank` covers, and every rank is below
            // the number of ids, which all fit in `T` (else `run` would have stopped).
            unsafe {
                let r = *rank.get_unchecked(v as usize >> 1) as usize;
                *count.get_unchecked_mut(r) += 1;
                *count.get_unchecked_mut(e + r) += v & 1;
                *x = T::try_from(r).unwrap_unchecked(); // remap to the rank once, for the scatter
            }
        }
        total += b.n;
    }
    // Counting sort: each endpoint gets a contiguous range of `lat`, ranges in name order.
    let mut pos = 0;
    for c in &mut count[..e] {
        (*c, pos) = (pos, pos + *c);
    }
    lat.resize(total, 0);
    for b in hr {
        for (&r, &v) in b.ids[..b.n].iter().zip(&b.lat[..b.n]) {
            let c = &mut count[Into::<u32>::into(r) as usize];
            // SAFETY: the cursors of rank r run over r's own range, which lies inside `total`.
            unsafe { *lat.get_unchecked_mut(*c as usize) = v };
            *c += 1;
        }
        b.free();
    }
    let (mut rows, mut size) = (Vec::new(), 0);
    let mut a = 0;
    for r in 0..e {
        let b = count[r] as usize; // after the scatter this is the end of rank r's range
        if a == b {
            continue;
        }
        let v = &mut lat[a..b];
        let n = v.len();
        // Nearest rank: 0-based index ceil(p*n/100) - 1. Each select leaves everything from
        // that index on in v[lo..], so the next search is smaller.
        // Small groups (most of them, in the Zipf tail): one insertion sort beats three selects.
        if n <= 32 {
            v.sort_unstable();
        }
        let mut row = [r as u32, n as u32, 0, 0, 0, count[e + r]];
        let mut lo = 0;
        for (i, p) in [50, 95, 99].into_iter().enumerate() {
            let k = (p * n + 99) / 100 - 1;
            if n > 32 {
                v[lo..].select_nth_unstable(k - lo); // puts the k-th smallest at v[k]
            }
            (row[2 + i], lo) = (v[k], k);
        }
        // The line: label (21), name, a space, then six numbers each followed by one byte.
        let widths: usize = row[1..].iter().map(|&x| digits(x)).sum();
        size += 28 + names[r].len() + widths + digits(ppm(&row));
        rows.push(row);
        a = b;
    }
    (rows, size)
}

fn ppm(row: &Row) -> u32 {
    (row[5] as u64 * 1_000_000 / row[1] as u64) as u32
}

/// Formats hour `h`'s rows into `o`, which has exactly their size.
unsafe fn write_hour(h: usize, rows: &[Row], names: &[&[u8]], mut o: *mut u8) {
    let prefix: [u8; 21] = hour_label(h).as_bytes().try_into().unwrap(); // fixed size: no memcpy call
    for row in rows {
        let name = names[row[0] as usize];
        (o as *mut [u8; 21]).write_unaligned(prefix);
        std::ptr::copy_nonoverlapping(name.as_ptr(), o.add(21), name.len());
        *o.add(21 + name.len()) = b' ';
        o = o.add(22 + name.len());
        for &x in &row[1..] {
            put_num(&mut o, x, b' ');
        }
        put_num(&mut o, ppm(row), b'\n');
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let data = map_file(&format!("{data_dir}/requests.log"));
    let mut options = File::options();
    options.read(true).write(true).create(true); // read too: the output is mapped
    let file = options.open(format!("{out_dir}/summary.txt")).expect("cannot create summary.txt");
    // Compact ids first; only an input with too many endpoints for them pays for a second run.
    let _ = run::<u16>(data, &file) || run::<u32>(data, &file);
}

/// The whole job with ids of type `T`. Returns false, having written nothing, if some id
/// does not fit in `T`.
fn run<T: Id>(data: &'static [u8], file: &File) -> bool {
    let len = data.len();

    // Parse: one shared endpoint table, and per thread one bucket per hour.
    let keys = (len / 40 + 1).min(1 << (8 * size_of::<T>() - 1)); // lines, and what ids can number
    let table = &Table::new(keys, (len + len / 8 + 4).min(keys * 130)); // every key with its id
    // Each thread files its buckets under their hours when its chunk is done.
    let by_hour: Vec<Mutex<Vec<Bucket<T>>>> = (0..HOURS).map(|_| Mutex::default()).collect();
    thread::scope(|s| {
        for t in 0..THREADS {
            let by_hour = &by_hour;
            s.spawn(move || {
                let a = line_start(data, t * len / THREADS);
                let b = line_start(data, (t + 1) * len / THREADS);
                // A line is at least 40 bytes with its LF, so no hour gets more than this.
                let mut hours = Bucket::<T>::reserve((b - a) / 40 + 1);
                // Hour of the previous line: bytes 5..13 (`MM-DDTHH`) nearly always repeat.
                let (mut key, mut h) = (0u64, 0);
                for_each_line(data, a, b, |_, l| {
                    if l[5..13] != key.to_le_bytes() {
                        (key, h) = (u64::from_le_bytes(l[5..13].try_into().unwrap()), hour_of(l));
                    }
                    let (ep, is_err, latency) = parse_rest(l);
                    // Fits in T unless the table filled up, and then the run is discarded.
                    let id = T::try_from(table.id(ep) << 1 | is_err as usize).unwrap_or_default();
                    // SAFETY: the spec puts every timestamp inside the 72 hours, so h < HOURS,
                    // and a bucket has room for every line of the chunk.
                    unsafe {
                        let b = hours.get_unchecked_mut(h);
                        *b.ids.get_unchecked_mut(b.n) = id;
                        *b.lat.get_unchecked_mut(b.n) = latency;
                        b.n += 1;
                    }
                });
                for (h, b) in hours.into_iter().enumerate() {
                    by_hour[h].lock().unwrap().push(b);
                }
            });
        }
    });

    // Names: sort the distinct keys once; an id's rank is its place in the output order.
    // A full table (more keys than this run's ids can number) means some ids were lost.
    if table.ids.load(Relaxed) * 2 >= table.slots.len() {
        by_hour.into_iter().flat_map(|b| b.into_inner().unwrap()).for_each(Bucket::free);
        return false;
    }
    let mut keys = vec![&[][..]; table.ids.load(Relaxed)];
    for s in table.slots.iter().map(|s| s.load(Relaxed)) {
        if s != 0 {
            let (k, id) = table.entry(s);
            keys[id] = k;
        }
    }
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_unstable_by_key(|&i| keys[i]);
    let mut rank = vec![0u32; order.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r as u32;
    }
    let names: Vec<&[u8]> = order.iter().map(|&i| keys[i]).collect();

    // Finish: hours handed out in increasing order. Each hour's rows and text size come first;
    // then the file gets its final size and is mapped, and every hour is formatted straight into
    // its place. No write() calls: concurrent writes to one file serialize on its inode lock,
    // page faults on a shared mapping do not.
    let (next_hour, next_text) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let rows: Vec<Mutex<Rows>> = (0..HOURS).map(|_| Mutex::default()).collect();
    let at = std::sync::OnceLock::new(); // (mapping address, offset of every hour and the end)
    let barrier = std::sync::Barrier::new(THREADS);
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut lat = Vec::new();
                while let h @ 0..HOURS = next_hour.fetch_add(1, Relaxed) {
                    let buckets = std::mem::take(&mut *by_hour[h].lock().unwrap());
                    *rows[h].lock().unwrap() = tally(buckets, &rank, &names, &mut lat);
                }
                drop(lat);
                if barrier.wait().is_leader() {
                    let mut off = vec![0];
                    for r in &rows {
                        off.push(off[off.len() - 1] + r.lock().unwrap().1);
                    }
                    let total = off[HOURS];
                    file.set_len(total as u64).expect("cannot size summary.txt");
                    // (an empty output still gets a mapping, which is never touched)
                    let fd = file.as_raw_fd();
                    let p = unsafe { mmap(std::ptr::null_mut(), total.max(1), 3, 1, fd, 0) };
                    assert!(p as isize != -1, "mmap failed");
                    at.set((p as usize, off)).unwrap();
                }
                barrier.wait();
                let (p, off) = at.get().unwrap();
                while let h @ 0..HOURS = next_text.fetch_add(1, Relaxed) {
                    let (r, _) = std::mem::take(&mut *rows[h].lock().unwrap());
                    // SAFETY: hour h owns bytes off[h]..off[h + 1] of the mapping, its exact size.
                    unsafe { write_hour(h, &r, &names, (*p as *mut u8).add(off[h])) };
                }
            });
        }
    });
    true
}
