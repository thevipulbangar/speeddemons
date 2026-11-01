//! Track C (Join): parse a hostile CSV, join every record to its country, aggregate per country.
//!
//! One thread per hardware thread, all phases inside one thread scope, separated by barriers,
//! nothing copied out of the mmap:
//!
//! 1. regions.csv: every thread parses a slice of the lines twice, first to see which country
//!    codes occur, then to fill a flat `region_id -> group` byte table. Codes are `[A-Z]{3}`,
//!    so ranking the codes that occur gives group numbers in bytewise order. Group 0 is
//!    `_UNMATCHED`, so the zero-filled table already sends unknown region ids there.
//! 2. records.csv is cut into one chunk per thread. A cut can land inside a quoted note, so it
//!    is moved to the byte after the next LF that is followed by a digit or a BOM: the generator
//!    never puts digits or non-ASCII inside quotes (spec 2.2), so that LF ends a record. The
//!    guess is then checked, not trusted: the parser is strict, and the chunk before every cut
//!    must end exactly on it (by induction from the header, a known record start). If a check
//!    fails, the cuts are recomputed from quote parity (a byte is inside quotes iff an odd
//!    number of `"` precede it, which follows from the grammar in 2.1) and the parse reruns.
//! 3. names_distinct is exact. A lock-free hash table shared by all threads holds every name
//!    once; a slot is 16 bytes, the length and up to 15 name bytes, so a lookup is one 16-byte
//!    compare and the slot index is the name's id. Longer names keep their bytes in an arena.
//!    Each id has a shared row of 256 bits, one per group, set with a load and, only if the bit
//!    is still clear, a `fetch_or`; the row is prefetched for writing along with the slot.
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::Barrier;
use std::thread;

const CODES: usize = 26 * 26 * 26; // [A-Z]{3}
const BATCH: usize = 16; // records parsed ahead of their table lookups
const PENDING: u64 = 0x80; // in a slot's first byte while its second word is being written
const LONG: u64 = 0x7F; // first byte of a slot whose name lives in the arena

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *const u8, len: usize, advice: i32) -> i32;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap_or_else(|e| panic!("cannot open {path}: {e}"));
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // PROT_READ, MAP_PRIVATE: read straight from the page cache, no copy into our heap.
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// Zeroed anonymous memory for n atomics. Pages are only charged once written, so tables
/// can be sized for the worst case.
fn anon<T>(n: usize) -> &'static [T] {
    let bytes = (n * std::mem::size_of::<T>()).max(1);
    // PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE
    let p = unsafe { mmap(std::ptr::null_mut(), bytes, 3, 0x4022, -1, 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(p as *const T, n) }
}

#[derive(Clone, Copy)]
struct Agg {
    count: u64,
    id_sum: u64,
    sum: i64,
    min: i64,
    max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

const EMPTY: Agg = Agg { count: 0, id_sum: 0, sum: 0, min: i64::MAX, max: i64::MIN, name_bytes: 0, note_bytes: 0 };

/// The shared name table: open addressing, two words per slot. A name of up to 15 bytes is
/// stored as `[len + 1, bytes..., zero padding]`; a longer one as `[LONG, 56 bits of hash]`
/// plus its arena offset. A slot is claimed by a CAS of its first word to the key with
/// PENDING set, then the second word is written and the first word is published.
struct Names {
    slots: &'static [AtomicU64],
    bits: u32,
    arena: &'static [AtomicU8],
    arena_len: AtomicUsize,
    used: AtomicU32,
    max: u32,
    full: AtomicBool,
}

/// A name ready for the table: its two key words and its hash.
#[derive(Clone, Copy)]
struct Key {
    k1: u64,
    k2: u64,
    h: u64,
}

impl Names {
    fn new(bits: u32, arena_bytes: usize) -> Names {
        Names {
            slots: anon(2 << bits),
            bits,
            arena: anon(arena_bytes),
            arena_len: AtomicUsize::new(0),
            used: AtomicU32::new(0),
            max: (1 << bits) / 8 * 7,
            full: AtomicBool::new(false),
        }
    }

    #[inline(always)]
    fn home(&self, h: u64) -> usize {
        (h >> (64 - self.bits)) as usize
    }

    #[inline(always)]
    fn prefetch(&self, k: &Key) {
        unsafe { _mm_prefetch(&self.slots[2 * self.home(k.h)] as *const _ as *const i8, _MM_HINT_T0) };
    }

    fn arena_eq(&self, off: usize, name: &[u8]) -> bool {
        let len = (0..4).fold(0, |v, i| v | (self.arena[off + i].load(Relaxed) as usize) << (8 * i));
        len == name.len() && name.iter().zip(&self.arena[off + 4..]).all(|(&a, b)| b.load(Relaxed) == a)
    }

    /// The id (slot index) of `name`, or None once the table is too full.
    #[inline(always)]
    fn id(&self, name: &[u8], k: &Key) -> Option<u32> {
        let long = k.k1 & 0xFF == LONG;
        let mask = (1 << self.bits) - 1;
        let mut i = self.home(k.h);
        loop {
            let (s1, s2) = (&self.slots[2 * i], &self.slots[2 * i + 1]);
            let mut v = s1.load(Acquire);
            if v == 0 {
                if self.used.fetch_add(1, Relaxed) >= self.max {
                    self.full.store(true, Relaxed);
                    return None;
                }
                match s1.compare_exchange(0, k.k1 | PENDING, Acquire, Acquire) {
                    Ok(_) => {
                        s2.store(if long { self.store_long(name)? } else { k.k2 }, Relaxed);
                        s1.store(k.k1, Release);
                        return Some(i as u32);
                    }
                    Err(cur) => v = cur,
                }
            }
            while v & PENDING != 0 {
                std::hint::spin_loop();
                v = s1.load(Acquire);
            }
            if v == k.k1 {
                let v2 = s2.load(Relaxed);
                if if long { self.arena_eq(v2 as usize, name) } else { v2 == k.k2 } {
                    return Some(i as u32);
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn store_long(&self, name: &[u8]) -> Option<u64> {
        let off = self.arena_len.fetch_add(4 + name.len(), Relaxed);
        if off + 4 + name.len() > self.arena.len() {
            self.full.store(true, Relaxed);
            return None;
        }
        let bytes = (name.len() as u32).to_le_bytes().into_iter().chain(name.iter().copied());
        bytes.zip(&self.arena[off..]).for_each(|(a, b)| b.store(a, Relaxed));
        Some(off as u64)
    }
}

/// The table key of `name`, which lies in `d`. Short names are read as two words straight
/// from `d` when that stays inside it, and masked to their length.
#[inline(always)]
fn key(d: &[u8], name: &[u8]) -> Key {
    let n = name.len();
    if n > 15 {
        let h = name.iter().fold(n as u64, |h, &b| (h ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        return Key { k1: LONG | h << 8, k2: 0, h: h.wrapping_mul(0xFF51_AFD7_ED55_8CCD) };
    }
    let mut w = [0u64; 2];
    if name.as_ptr() as usize - d.as_ptr() as usize + 16 <= d.len() {
        for (i, w) in w.iter_mut().enumerate() {
            let word = unsafe { (name.as_ptr().add(8 * i) as *const u64).read_unaligned() };
            let keep = n.saturating_sub(8 * i);
            *w = if keep >= 8 { word } else { word & ((1u64 << (8 * keep)) - 1) };
        }
    } else {
        unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), w.as_mut_ptr() as *mut u8, n) };
    }
    let k1 = (n as u64 + 1) | w[0] << 8;
    let k2 = w[0] >> 56 | w[1] << 8;
    let h = (k1.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ k2).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    Key { k1, k2, h }
}

/// Position of the first `"` at or after p.
#[inline(always)]
fn find_quote(d: &[u8], mut p: usize) -> Option<usize> {
    while p + 32 <= d.len() {
        let m = unsafe {
            let v = _mm256_loadu_si256(d.as_ptr().add(p) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8))) as u32
        };
        if m != 0 {
            return Some(p + m.trailing_zeros() as usize);
        }
        p += 32;
    }
    d[p..].iter().position(|&b| b == b'"').map(|i| p + i)
}

/// A text field at p and its comma: (raw bytes between the quotes, value length, position
/// after the comma).
#[inline(always)]
fn text(d: &[u8], p: usize) -> Option<(&[u8], u64, usize)> {
    if d.get(p) == Some(&b'"') {
        let (mut q, mut esc) = (p + 1, 0);
        loop {
            q = find_quote(d, q)?;
            if d.get(q + 1) != Some(&b'"') {
                break;
            }
            esc += 1;
            q += 2;
        }
        (d.get(q + 1) == Some(&b',')).then(|| (&d[p + 1..q], (q - p - 1 - esc) as u64, q + 2))
    } else {
        let mut e = p;
        while *d.get(e)? != b',' {
            if matches!(d[e], b'"' | b'\r' | b'\n') {
                return None;
            }
            e += 1;
        }
        Some((&d[p..e], (e - p) as u64, e + 1))
    }
}

/// 1*DIGIT followed by `stop`: (value, position after `stop`). Saturates instead of wrapping.
#[inline(always)]
fn number(d: &[u8], mut p: usize, stop: u8) -> Option<(u64, usize)> {
    // Fast path: 1 to 7 digits and the stop byte within the next 8 bytes, converted with SWAR.
    if let Some(b) = d.get(p..p + 8) {
        let x = u64::from_le_bytes(b.try_into().unwrap()) ^ 0x3030_3030_3030_3030;
        let nondigit = (x.wrapping_add(0x7676_7676_7676_7676) | x) & 0x8080_8080_8080_8080;
        let n = (nondigit.trailing_zeros() / 8) as usize;
        if n > 0 && n < 8 {
            if b[n] != stop {
                return None;
            }
            let mut v = x << (64 - 8 * n); // the digits, behind leading zeros
            v = (v & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
            v = (v & 0x00FF_00FF_00FF_00FF).wrapping_mul(6553601) >> 16;
            v = (v & 0x0000_FFFF_0000_FFFF).wrapping_mul(42949672960001) >> 32;
            return Some((v, p + n + 1));
        }
    }
    let s = p;
    let mut v = 0u64;
    while let Some(c) = d.get(p).filter(|c| c.is_ascii_digit()) {
        v = v.saturating_mul(10).saturating_add((c - b'0') as u64);
        p += 1;
    }
    (p > s && d.get(p) == Some(&stop)).then_some((v, p + 1))
}

/// One parsed record, waiting in a batch for its table lines to arrive.
struct Rec<'a> {
    id: u64,
    rid: usize,
    cents: i64,
    name: &'a [u8],
    key: Key,
    name_len: u64,
    note_len: u64,
}

/// The record at p, strictly by the grammar in spec 2.1, and the position after it.
#[inline(always)]
fn record(d: &[u8], mut p: usize) -> Option<(Rec<'_>, usize)> {
    if d[p] == 0xEF {
        if d.get(p + 1..p + 3) != Some(b"\xBB\xBF") {
            return None;
        }
        p += 3;
    }
    let (id, q) = number(d, p, b',')?;
    let (name, name_len, q) = text(d, q)?;
    let (_, note_len, q) = text(d, q)?;
    let (rid, q) = number(d, q, b',')?;
    let neg = d.get(q) == Some(&b'-');
    let (whole, q) = number(d, q + neg as usize, b'.')?;
    let frac = d.get(q..q + 2).filter(|f| f.iter().all(u8::is_ascii_digit))?;
    let cents = whole as i64 * 100 + ((frac[0] - b'0') * 10 + (frac[1] - b'0')) as i64;
    let next = match d.get(q + 2..q + 4) {
        None if q + 2 == d.len() => q + 2,
        _ if d[q + 2] == b'\n' => q + 3,
        Some(b"\r\n") => q + 4,
        _ => return None,
    };
    let cents = if neg { -cents } else { cents };
    Some((Rec { id, rid: rid as usize, cents, name, key: key(d, name), name_len, note_len }, next))
}

/// Everything the workers share.
struct Ctx<'a> {
    d: &'a [u8],
    r: &'a [u8],
    rcuts: &'a [usize],
    present: &'a [AtomicBool],
    region: &'a [AtomicU8],
    groups: usize,
    threads: usize,
    names: Names,
    seen: &'a [AtomicU64], // 4 words per name id: one bit per group that had the name
    barrier: Barrier,
}

/// What one worker returns.
struct Part {
    end: Option<usize>, // where its parse stopped, None on a grammar error or a full table
    aggs: Vec<Agg>,
    distinct: Vec<u64>, // per group, over the names this worker owns
}

/// Calls f(id, code) for every line of regions.csv in this worker's slice.
fn regions(c: &Ctx, k: usize, mut f: impl FnMut(usize, usize)) {
    for line in c.r[c.rcuts[k]..c.rcuts[k + 1]].split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let (id, q) = number(line, 0, b',').expect("bad regions.csv line");
        let code = &line[q..];
        assert!(code.len() == 3 && code.iter().all(u8::is_ascii_uppercase), "bad country code");
        f(id as usize, code.iter().fold(0, |v, &b| v * 26 + (b - b'A') as usize));
    }
}

/// Group number per country code: 1, 2, ... in code order (0 is _UNMATCHED).
fn ranks(present: &[AtomicBool]) -> Vec<u8> {
    let mut next = 0;
    present
        .iter()
        .map(|p| {
            next += p.load(Relaxed) as usize;
            next as u8
        })
        .collect()
}

fn worker(c: &Ctx, k: usize, start: usize, end: usize, load_regions: bool) -> Part {
    if load_regions {
        regions(c, k, |_, code| c.present[code].store(true, Relaxed));
        c.barrier.wait();
        let rank = ranks(c.present);
        regions(c, k, |id, code| c.region[id].store(rank[code], Relaxed));
        c.barrier.wait();
    }
    let d = c.d;
    // Map our part of the file up front (MADV_POPULATE_READ, Linux 5.14+): one call instead of a
    // page fault every few pages. Older kernels refuse it, which only costs the faults.
    if end > start {
        let a = start & !4095;
        unsafe { madvise(d.as_ptr().add(a), end - a, 22) };
    }
    let mut aggs = vec![EMPTY; c.groups];
    let mut batch = Vec::with_capacity(BATCH);
    let mut p = start;
    let mut ok = true;
    'parse: while p < end {
        // Parse a batch and prefetch the table lines it needs, then do the lookups.
        batch.clear();
        while p < end && batch.len() < BATCH {
            let Some((r, q)) = record(d, p) else {
                ok = false;
                break 'parse;
            };
            if let Some(g) = c.region.get(r.rid) {
                unsafe { _mm_prefetch(g as *const _ as *const i8, _MM_HINT_T0) };
            }
            c.names.prefetch(&r.key);
            // The (name, group) bits usually sit next to the home slot's row: fetch that line for writing.
            let row = &c.seen[4 * c.names.home(r.key.h)];
            unsafe { _mm_prefetch(row as *const _ as *const i8, _MM_HINT_ET0) };
            batch.push(r);
            p = q;
        }
        for r in &batch {
            let g = c.region.get(r.rid).map_or(0, |x| x.load(Relaxed)) as usize;
            let a = &mut aggs[g];
            a.count += 1;
            a.id_sum += r.id;
            a.sum += r.cents;
            a.min = a.min.min(r.cents);
            a.max = a.max.max(r.cents);
            a.name_bytes += r.name_len;
            a.note_bytes += r.note_len;
            let Some(id) = c.names.id(r.name, &r.key) else {
                ok = false;
                break 'parse;
            };
            let w = &c.seen[4 * id as usize + g as usize / 64];
            if w.load(Relaxed) & 1 << (g % 64) == 0 {
                w.fetch_or(1 << (g % 64), Relaxed);
            }
        }
    }
    c.barrier.wait();
    // Count the bits of our share of the name rows.
    let n = 1usize << c.names.bits;
    let rows = n * k / c.threads..n * (k + 1) / c.threads;
    let mut distinct = vec![0u64; c.groups];
    for row in c.seen[4 * rows.start..4 * rows.end].chunks(4) {
        for (j, w) in row.iter().enumerate() {
            let mut w = w.load(Relaxed);
            while w != 0 {
                distinct[j * 64 + w.trailing_zeros() as usize] += 1;
                w &= w - 1;
            }
        }
    }
    Part { end: if ok { Some(p) } else { None }, aggs, distinct }
}

/// Runs all workers; None if a chunk did not end exactly where the next one starts (a bad cut
/// or bad input) or the name table ran full.
fn run(c: &Ctx, starts: &[usize], load_regions: bool) -> Option<(Vec<Agg>, Vec<u64>)> {
    let parts: Vec<Part> = thread::scope(|s| {
        let hs: Vec<_> =
            (0..c.threads).map(|k| s.spawn(move || worker(c, k, starts[k], starts[k + 1], load_regions))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    if parts.iter().enumerate().any(|(k, p)| p.end != Some(starts[k + 1])) {
        return None;
    }
    let mut total = vec![EMPTY; c.groups];
    let mut distinct = vec![0; c.groups];
    for p in &parts {
        for (t, a) in total.iter_mut().zip(&p.aggs) {
            t.count += a.count;
            t.id_sum += a.id_sum;
            t.sum += a.sum;
            t.min = t.min.min(a.min);
            t.max = t.max.max(a.max);
            t.name_bytes += a.name_bytes;
            t.note_bytes += a.note_bytes;
        }
        distinct.iter_mut().zip(&p.distinct).for_each(|(t, n)| *t += n);
    }
    Some((total, distinct))
}

fn money(c: i64) -> String {
    let a = c.unsigned_abs();
    format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, a / 100, a % 100)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let d = map_file(&format!("{data_dir}/records.csv"));
    let r = map_file(&format!("{data_dir}/regions.csv"));
    let threads = thread::available_parallelism().map_or(12, |n| n.get());

    // regions.csv: ids are ascending, so the last line holds the largest.
    let body = r.iter().position(|&b| b == b'\n').map_or(r.len(), |p| p + 1);
    let last = r[..r.len().saturating_sub(1)].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1).max(body);
    let max_id = number(r, last, b',').map_or(0, |(v, _)| v as usize);
    let rcuts: Vec<usize> = (0..=threads).map(|i| next_line(r, body + (r.len() - body) * i / threads)).collect();

    // records.csv: guessed cuts first.
    let header = d.iter().position(|&b| b == b'\n').map_or(d.len(), |p| p + 1);
    let mut starts = vec![header];
    for i in 1..threads {
        let mut p = (d.len() * i / threads).max(header + 1);
        while p < d.len() && !(d[p - 1] == b'\n' && (d[p].is_ascii_digit() || d[p] == 0xEF)) {
            p += 1;
        }
        starts.push(p.min(d.len()).max(starts[i - 1]));
    }
    starts.push(d.len());

    let present: &[AtomicBool] = anon(CODES);
    let region: &[AtomicU8] = anon(max_id + 1);
    let groups = 256; // group numbers are bytes; the real count is known once regions.csv is read
    let ctx = |bits, groups| Ctx {
        d,
        r,
        rcuts: &rcuts,
        present,
        region,
        groups,
        threads,
        names: Names::new(bits, d.len()),
        seen: anon(4 << bits),
        barrier: Barrier::new(threads),
    };
    let mut result = run(&ctx(17, groups), &starts, true);
    let countries = present.iter().filter(|p| p.load(Relaxed)).count();
    assert!(countries < 256, "more than 255 countries");
    if result.is_none() {
        // Exact cuts, and a bigger name table each time it runs full.
        let starts = parity_starts(d, header, threads);
        let mut bits = 17;
        while result.is_none() {
            let c = ctx(bits, groups);
            result = run(&c, &starts, false);
            assert!(result.is_some() || c.names.full.load(Relaxed), "records.csv does not follow the grammar");
            bits += 2;
        }
    }
    let (total, distinct) = result.unwrap();


    let codes: Vec<usize> = (0..CODES).filter(|&c| present[c].load(Relaxed)).collect();
    let mut out = String::new();
    for g in (1..=countries).chain([0]).filter(|&g| total[g].count > 0) {
        let t = &total[g];
        let key = match g {
            0 => "_UNMATCHED".to_string(),
            _ => {
                let c = codes[g - 1];
                [c / 676, c / 26 % 26, c % 26].iter().map(|&x| (b'A' + x as u8) as char).collect()
            }
        };
        let line = [
            key,
            t.count.to_string(),
            t.id_sum.to_string(),
            money(t.sum),
            money(t.min),
            money(t.max),
            distinct[g].to_string(),
            t.name_bytes.to_string(),
            t.note_bytes.to_string(),
        ];
        out += &line.join(" ");
        out.push('\n');
    }
    File::create(format!("{out_dir}/aggregates.txt")).unwrap().write_all(out.as_bytes()).unwrap();
}

/// The first line start at or after p (p > 0).
fn next_line(r: &[u8], p: usize) -> usize {
    r[p - 1..].iter().position(|&b| b == b'\n').map_or(r.len(), |i| p + i)
}

/// Exact chunk starts: a byte is inside quotes iff an odd number of `"` precede it, so count
/// the quotes per chunk in parallel, then move every cut past the first LF outside quotes.
fn parity_starts(d: &[u8], header: usize, threads: usize) -> Vec<usize> {
    let cuts: &Vec<usize> = &(0..=threads).map(|i| header + (d.len() - header) * i / threads).collect();
    let quotes: Vec<usize> = thread::scope(|s| {
        let hs: Vec<_> =
            (0..threads).map(|i| s.spawn(move || d[cuts[i]..cuts[i + 1]].iter().filter(|&&b| b == b'"').count())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut starts = vec![header];
    let mut parity = 0;
    for i in 1..threads {
        parity += quotes[i - 1];
        let (mut inside, mut p) = (parity % 2 == 1, cuts[i]);
        while p < d.len() && (inside || d[p] != b'\n') {
            inside ^= d[p] == b'"';
            p += 1;
        }
        starts.push((p + 1).min(d.len()).max(starts[i - 1]));
    }
    starts.push(d.len());
    starts
}
