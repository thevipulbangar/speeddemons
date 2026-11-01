//! Track B: answer 50,000 BOOL / PHRASE queries over a 254 MB corpus.
//!
//! The queries are read first, so only the terms they use are ever looked up.
//! The mmapped corpus is split into chunks of whole 64-document groups, one per thread, and
//! read once: tokenize (AVX2), look each token up in the query lexicon, set the bitmap bit of
//! frequent BOOL terms, stream the hits of the other BOOL terms into a per-chunk CSR index of
//! u16 doc ids, and check phrases where their rarest term occurs. No positions are stored.
//! BOOL queries are evaluated left to right on sorted lists, switching to a bitmap
//! only when an operand is a bitmap term.

use std::arch::x86_64::*;
use std::fmt::Write as _;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

const NONE: u32 = u32::MAX;
const DENSE: u32 = 1 << 31;
const SPARSE: u32 = 1 << 30;
const TRIG: u32 = 1 << 29;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
}

fn map_file(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap_or_else(|e| panic!("cannot open {path}: {e}"));
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    // PROT_READ, MAP_PRIVATE: read straight from the page cache.
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// A term of at most 32 bytes, zero padded. Longer corpus tokens can never equal a query term.
type Key = [u64; 4];

fn key_of(term: &[u8]) -> Key {
    let mut b = [0u8; 32];
    b[..term.len()].copy_from_slice(term);
    b.make_ascii_lowercase();
    std::array::from_fn(|i| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()))
}

#[inline(always)]
fn hash(k: &Key) -> u64 {
    let h = k[0].wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ k[1].wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ k[2].wrapping_mul(0x1656_67B1_9E37_79F9)
        ^ k[3].wrapping_mul(0x27D4_EB2F_1656_67C5);
    h ^ (h >> 29)
}

/// One lexicon slot: the term's first 16 bytes, id and kind (0: phrases only, SPARSE: doc
/// list, DENSE | i: bitmap i, plus TRIG if it triggers phrase checks). Tokens of up to 16
/// bytes, nearly all of them, are compared and classified within this one cache line.
#[derive(Clone, Copy)]
struct Slot {
    w: [u64; 2],
    id: u32,
    kind: u32,
    trig: [u32; 2], // its entries in `trig`, if it triggers phrase checks
}

const EMPTY: Slot = Slot { w: [0; 2], id: NONE, kind: 0, trig: [0; 2] };

/// Query terms, each with a dense id.
struct Lexicon {
    slots: Vec<Slot>,
    keys: Vec<Key>,
}

impl Lexicon {
    fn new(n: usize) -> Lexicon {
        Lexicon { slots: vec![EMPTY; n.next_power_of_two().max(1024)], keys: Vec::new() }
    }
    /// The slot of term k, EMPTY when k is not a query term.
    #[inline(always)]
    fn find(&self, k: &Key) -> Slot {
        self.probe(&self.slots, k, hash(k))
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        let i = (h >> 32) as usize & (self.slots.len() - 1);
        // SAFETY: a prefetch of an address inside `slots`.
        unsafe { _mm_prefetch(self.slots.as_ptr().add(i) as *const i8, _MM_HINT_T0) };
    }
    #[inline(always)]
    fn probe(&self, slots: &[Slot], k: &Key, h: u64) -> Slot {
        let mask = slots.len() - 1;
        let mut i = (h >> 32) as usize & mask;
        loop {
            let s = slots[i];
            if s.id == NONE || (s.w == [k[0], k[1]] && (k[2] == 0 || self.keys[s.id as usize] == *k)) {
                return s;
            }
            i = (i + 1) & mask;
        }
    }
    fn insert(slots: &mut [Slot], k: &Key, id: u32) {
        let mask = slots.len() - 1;
        let mut i = (hash(k) >> 32) as usize & mask;
        while slots[i].id != NONE {
            i = (i + 1) & mask;
        }
        slots[i] = Slot { w: [k[0], k[1]], id, kind: 0, trig: [0; 2] };
    }
    fn id(&mut self, term: &[u8]) -> u32 {
        let k = key_of(term);
        let s = self.find(&k);
        if s.id != NONE {
            return s.id;
        }
        self.push(k)
    }
    fn push(&mut self, k: Key) -> u32 {
        let id = self.keys.len() as u32;
        self.keys.push(k);
        if self.keys.len() * 10 >= self.slots.len() * 6 {
            self.slots = vec![EMPTY; self.slots.len() * 2];
            for (i, k) in self.keys.iter().enumerate() {
                Lexicon::insert(&mut self.slots, k, i as u32);
            }
        } else {
            Lexicon::insert(&mut self.slots, &k, id);
        }
        id
    }
}

/// 32 bytes of data from p; past the end of the file reads zeros, which are separators.
#[inline(always)]
fn load32(data: &[u8], p: usize) -> __m256i {
    let mut pad = [0u8; 32];
    let src = if p + 32 <= data.len() {
        &data[p..p + 32]
    } else {
        let rest = &data[p.min(data.len())..];
        pad[..rest.len()].copy_from_slice(rest);
        &pad[..]
    };
    // SAFETY: src has 32 bytes.
    unsafe { _mm256_loadu_si256(src.as_ptr() as *const __m256i) }
}

/// Token-byte and LF bit masks of the 64 bytes from p. A byte is a token byte when it is a
/// digit or, with bit 5 set (ASCII lowercasing for letters), a lowercase letter.
#[inline(always)]
fn masks(data: &[u8], p: usize) -> (u64, u64) {
    // SAFETY: AVX2 is in the build's target features.
    unsafe {
        let set = |b: u8| _mm256_set1_epi8(b as i8);
        let le = |x, n: u8| _mm256_cmpeq_epi8(_mm256_min_epu8(x, set(n)), x);
        let m = |v| {
            let alpha = le(_mm256_sub_epi8(_mm256_or_si256(v, set(0x20)), set(b'a')), 25);
            let digit = le(_mm256_sub_epi8(v, set(b'0')), 9);
            let tok = _mm256_movemask_epi8(_mm256_or_si256(alpha, digit)) as u32 as u64;
            (tok, _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, set(b'\n'))) as u32 as u64)
        };
        let ((t0, n0), (t1, n1)) = (m(load32(data, p)), m(load32(data, p + 32)));
        (t0 | t1 << 32, n0 | n1 << 32)
    }
}

/// Calls `f(doc, lexicon slot)` for every token of the lines in data[start..end].
/// Works on 64-byte blocks: token starts, token lengths and line ends all come out of
/// the blocks' bit masks, so the inner loop runs once per token with no per-byte work.
#[inline(always)]
fn tokens(data: &[u8], start: usize, end: usize, lex: &Lexicon, mut f: impl FnMut(u32, Slot, usize)) {
    const PREFIX: [u8; 64] = {
        let mut m = [0u8; 64];
        let mut i = 0;
        while i < 32 {
            m[i] = 0xFF;
            i += 1;
        }
        m
    };
    let (mut doc, mut want_id, mut carry) = (0u32, true, 0u64);
    // Lookups lag one block behind: a block's slots are prefetched, then the previous block's
    // tokens are looked up, so the cache misses of up to two blocks overlap.
    let mut batch = [(0u32, 0usize, [0u64; 4], 0u64); 32];
    let mut ready = [(0u32, 0usize, [0u64; 4], 0u64); 32];
    let mut nready = 0;
    let (mut tok, mut nl) = masks(data, start);
    let mut b = start;
    while b < end {
        let (next_tok, next_nl) = masks(data, b + 64);
        let mut ev = (tok & !(tok << 1 | carry)) | nl;
        carry = tok >> 63;
        if end - b < 64 {
            ev &= (1u64 << (end - b)) - 1;
        }
        let mut n = 0;
        while ev != 0 {
            let i = ev.trailing_zeros();
            ev &= ev - 1;
            if nl >> i & 1 != 0 {
                want_id = true;
                continue;
            }
            let p = b + i as usize;
            let len = (!((tok as u128 | (next_tok as u128) << 64) >> i) as u64).trailing_zeros() as usize;
            if want_id {
                // The first token of a line is the document id.
                doc = data[p..p + len].iter().fold(0, |a, &c| a * 10 + (c - b'0') as u32);
                want_id = false;
            } else {
                let mut k: Key = [0; 4]; // stays zero, so matches nothing, for a token longer than any term
                // SAFETY: PREFIX[32 - len..] has 32 readable bytes; k is 32 bytes.
                if len <= 32 {
                    unsafe {
                    let m = _mm256_loadu_si256(PREFIX[32 - len..].as_ptr() as *const __m256i);
                    let v = _mm256_or_si256(load32(data, p), _mm256_set1_epi8(0x20));
                    _mm256_storeu_si256(k.as_mut_ptr() as *mut __m256i, _mm256_and_si256(v, m));
                    }
                }
                let h = hash(&k);
                lex.prefetch(h);
                batch[n] = (doc, p + len, k, h);
                n += 1;
            }
        }
        for (d, e, k, h) in &ready[..nready] {
            f(*d, lex.probe(&lex.slots, k, *h), *e);
        }
        (ready, batch, nready) = (batch, ready, n);
        (tok, nl, b) = (next_tok, next_nl, b + 64);
    }
    for (d, e, k, h) in &ready[..nready] {
        f(*d, lex.probe(&lex.slots, k, *h), *e);
    }
}

/// Bitmap words written by many threads at once, each into words no other thread touches.
struct Shared(*mut u64);
unsafe impl Sync for Shared {}

/// A query after parsing: BOOL as (op, term) pairs, PHRASE as its distinct-phrase index.
enum Query {
    Bool(Vec<(u8, u32)>),
    Phrase(u32),
}

/// Sorted-list AND / OR / NOT (ops b'A', b'O', b'N').
fn merge(a: &[u32], b: &[u32], op: u8, out: &mut Vec<u32>) {
    out.clear();
    if op != b'O' && a.len() * 16 < b.len() {
        return out.extend(a.iter().filter(|x| b.binary_search(x).is_ok() == (op == b'A')));
    }
    if op == b'A' && b.len() * 16 < a.len() {
        return out.extend(b.iter().filter(|x| a.binary_search(x).is_ok()));
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            if op != b'A' {
                out.push(a[i]);
            }
            i += 1;
        } else if a[i] > b[j] {
            if op == b'O' {
                out.push(b[j]);
            }
            j += 1;
        } else {
            if op != b'N' {
                out.push(a[i]);
            }
            i += 1;
            j += 1;
        }
    }
    if op != b'A' {
        out.extend_from_slice(&a[i..]);
    }
    if op == b'O' {
        out.extend_from_slice(&b[j..]);
    }
}

/// Appends "count sum id1 .. id10".
fn format(out: &mut String, count: usize, sum: u64, first: &[u32]) {
    write!(out, "{count} {sum}").unwrap();
    for d in first {
        write!(out, " {d}").unwrap();
    }
    out.push('\n');
}

fn format_list(out: &mut String, l: &[u32]) {
    format(out, l.len(), l.iter().map(|&d| d as u64).sum(), &l[..l.len().min(10)]);
}

fn format_bits(out: &mut String, r: &[u64]) {
    const M: [u64; 6] = [
        0xAAAA_AAAA_AAAA_AAAA,
        0xCCCC_CCCC_CCCC_CCCC,
        0xF0F0_F0F0_F0F0_F0F0,
        0xFF00_FF00_FF00_FF00,
        0xFFFF_0000_FFFF_0000,
        0xFFFF_FFFF_0000_0000,
    ];
    let (mut count, mut sum, mut first) = (0, 0u64, Vec::new());
    for (i, &w) in r.iter().enumerate().filter(|x| *x.1 != 0) {
        let c = w.count_ones() as u64;
        count += c as usize;
        // id sum of this word: its base times the count, plus the bit positions,
        // which are added one binary digit at a time.
        sum += i as u64 * 64 * c + (0..6).map(|j| ((w & M[j]).count_ones() as u64) << j).sum::<u64>();
        let mut x = w;
        while x != 0 && first.len() < 10 {
            first.push(i as u32 * 64 + x.trailing_zeros());
            x &= x - 1;
        }
    }
    format(out, count, sum, &first);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let docs = map_file(&format!("{data_dir}/docs.txt"));
    let qtext = map_file(&format!("{data_dir}/queries.txt"));
    // glibc hands big blocks back to the kernel on free (munmap, heap trimming), and every
    // reuse then page-faults again with all threads contending on the address-space lock.
    // Keep freed memory: M_TRIM_THRESHOLD and M_MMAP_THRESHOLD to 1 GiB.
    unsafe {
        mallopt(-1, 1 << 30);
        mallopt(-3, 1 << 30);
    }
    let threads = thread::available_parallelism().map_or(12, |n| n.get());

    // Parse queries. Every term gets a lexicon id; identical phrases share one entry.
    let mut lex = Lexicon::new(qtext.len() / 16);
    let mut phrases: Vec<Vec<u32>> = Vec::new(); // distinct phrases
    let mut phrase_ids = std::collections::HashMap::new();
    let mut queries = Vec::new();
    for line in qtext.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            let mut op = b'O';
            let mut q = Vec::new();
            for (i, w) in rest.split(|&b| b == b' ').enumerate() {
                if i % 2 == 1 {
                    op = w[0];
                } else {
                    q.push((op, lex.id(w)));
                }
            }
            queries.push(Query::Bool(q));
        } else {
            let rest = &line[8..line.len() - 1]; // PHRASE "...."
            let p: Vec<u32> = rest.split(|&b| b == b' ').map(|w| lex.id(w)).collect();
            let n = phrase_ids.len() as u32;
            queries.push(Query::Phrase(*phrase_ids.entry(p.clone()).or_insert_with(|| {
                phrases.push(p);
                n
            })));
        }
    }
    // Sample the first megabyte of the corpus. Its document frequencies decide which BOOL
    // terms get a bitmap, and the terms are renumbered by them so the few thousand terms
    // that make up most tokens share the same cache lines in every per-term array.
    let mut sdf = vec![(NONE, 0u32); lex.keys.len()];
    let mut sdocs = 0;
    tokens(docs, 0, line_start(docs, docs.len().min(1 << 20)), &lex, |d, s, _| {
        sdocs = d + 1;
        let t = s.id;
        if t != NONE && sdf[t as usize].0 != d {
            sdf[t as usize] = (d, sdf[t as usize].1 + 1);
        }
    });
    let mut order: Vec<u32> = (0..lex.keys.len() as u32).collect();
    order.sort_by_key(|&t| std::cmp::Reverse(sdf[t as usize].1));
    let mut rank = vec![0u32; order.len()];
    order.iter().enumerate().for_each(|(i, &t)| rank[t as usize] = i as u32);
    lex.keys = order.iter().map(|&t| lex.keys[t as usize]).collect();
    lex.slots.iter_mut().filter(|s| s.id != NONE).for_each(|s| s.id = rank[s.id as usize]);
    phrases.iter_mut().flatten().for_each(|t| *t = rank[*t as usize]);
    let nterms = lex.keys.len();
    // Per term: NONE (phrases only), SPARSE (sorted doc list) or DENSE | bitmap index.
    let mut kind = vec![0; nterms];
    let mut ndense = 0;
    for q in queries.iter_mut() {
        if let Query::Bool(q) = q {
            for (_, t) in q.iter_mut() {
                *t = rank[*t as usize];
                if kind[*t as usize] == 0 {
                    // A u16 list costs 2 bytes a match, a bitmap 1/8 byte a corpus document.
                    kind[*t as usize] = if sdf[order[*t as usize] as usize].1 * 16 > sdocs {
                        ndense += 1;
                        DENSE | (ndense - 1)
                    } else {
                        SPARSE
                    };
                }
            }
        }
    }
    // Each distinct phrase is checked only where its trigger occurs: its rarest term in the
    // sample (the last one on a tie, so fewer terms have to be read ahead). The terms before
    // the trigger come from a ring of the document's last 8 term ids, the ones after it are
    // matched as the next tokens arrive. A trigger entry carries the whole phrase, so a check
    // touches one cache line.
    let mut trig: Vec<Trig> = Vec::with_capacity(phrases.len());
    for (i, p) in phrases.iter().enumerate() {
        let j = (0..p.len()).rev().min_by_key(|&j| sdf[order[p[j] as usize] as usize].1).unwrap();
        let mut terms = [NONE; 8];
        terms[..p.len()].copy_from_slice(p);
        trig.push(Trig { term: p[j], phrase: i as u32, at: j as u32, len: p.len() as u32, terms });
    }
    trig.sort_unstable_by_key(|g| (g.term, g.phrase));
    let mut trig_start = vec![0u32; nterms + 1];
    trig.iter().for_each(|g| trig_start[g.term as usize + 1] += 1);
    for t in 0..nterms {
        trig_start[t + 1] += trig_start[t];
    }
    for s in lex.slots.iter_mut().filter(|s| s.id != NONE) {
        let t = s.id as usize;
        s.kind = kind[t] | if trig_start[t] < trig_start[t + 1] { TRIG } else { 0 };
        s.trig = [trig_start[t], trig_start[t + 1]];
    }

    // Document count from the last line's id.
    let last_line = docs[..docs.len().saturating_sub(1)].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let ndocs = if docs.is_empty() { 0 } else { doc_id(docs, last_line) as usize + 1 };
    let words = ndocs.div_ceil(64);
    // Chunks start at a document id divisible by 64, so each owns whole bitmap words, and hold
    // fewer than 65536 documents (asserted below), so a doc id within a chunk fits in a u16.
    let nchunks = threads.max(ndocs.div_ceil(20_000));
    let bounds: Vec<usize> = (0..=nchunks).map(|i| aligned_start(docs, docs.len() * i / nchunks)).collect();
    let mut bitmaps = vec![0u64; ndense as usize * words];

    // The one pass over the corpus. Each chunk sets its bitmap bits in place, writes its
    // sparse postings as a stream of (doc marker, term ids...) that then becomes the chunk's
    // own CSR index, and collects its phrase matches.
    let (lex, kind, bm) = (&lex, &kind, &Shared(bitmaps.as_mut_ptr()));
    let (trig, nphrases) = (&trig, phrases.len());
    let chunks: Vec<Chunk> = thread::scope(|s| {
        let hs: Vec<_> = (0..nchunks)
            .map(|i| {
                let (a, b) = (bounds[i], bounds[i + 1]);
                s.spawn(move || {
                    let base = if a < b { doc_id(docs, a) } else { 0 };
                    let mut seen = vec![u16::MAX; nterms]; // last doc (minus base) of each term
                    let mut stream: Vec<u32> = Vec::with_capacity((b - a) / 16);
                    let mut last_match = vec![NONE; nphrases];
                    let mut hits = Vec::new();
                    let (mut prev, mut pos, mut marked) = (NONE, 0usize, false);
                    let mut ring = [NONE; 8];
                    let mut pending: Vec<(&Trig, usize)> = Vec::new(); // (phrase, index of its next term)
                    tokens(docs, a, b, lex, |d, s, _| {
                        let t = s.id;
                        if d != prev {
                            (prev, pos, marked) = (d, 0, false);
                            pending.clear();
                        }
                        ring[pos % 8] = t;
                        pos += 1;
                        // Phrases whose trigger came earlier in this document wait for their next term.
                        pending.retain_mut(|(g, m)| {
                            if g.terms[*m] != t {
                                return false;
                            }
                            *m += 1;
                            if *m == g.len as usize && last_match[g.phrase as usize] != d {
                                last_match[g.phrase as usize] = d;
                                hits.push((g.phrase, d));
                            }
                            *m < g.len as usize
                        });
                        if t == NONE {
                            return;
                        }
                        let (ti, k) = (t as usize, s.kind & !TRIG);
                        if k & DENSE != 0 {
                            // Setting a bit twice is harmless, so bitmap terms skip the `seen` check.
                            // SAFETY: the word lies in this chunk's documents, which no other
                            // thread writes, and inside `bitmaps`.
                            unsafe { *bm.0.add((k & !DENSE) as usize * words + d as usize / 64) |= 1 << (d % 64) };
                        } else if k == SPARSE && seen[ti] != (d - base) as u16 {
                            seen[ti] = (d - base) as u16;
                            if !marked {
                                stream.push(DENSE | (d - base));
                                marked = true;
                            }
                            stream.push(t);
                        }
                        if s.kind & TRIG == 0 {
                            return;
                        }
                        for g in &trig[s.trig[0] as usize..s.trig[1] as usize] {
                            let j = g.at as usize;
                            if pos <= j || (0..j).any(|m| ring[(pos - 1 - j + m) % 8] != g.terms[m]) {
                                continue;
                            }
                            if j + 1 < g.len as usize {
                                pending.push((g, j + 1));
                            } else if last_match[g.phrase as usize] != d {
                                last_match[g.phrase as usize] = d;
                                hits.push((g.phrase, d));
                            }
                        }
                    });
                    assert!(prev == NONE || prev - base < 1 << 16, "chunk too large for u16 doc ids");
                    // Counting sort of the stream by term: off[t]..off[t + 1] are t's documents.
                    let mut off = vec![0u32; nterms + 1];
                    stream.iter().filter(|&&x| x & DENSE == 0).for_each(|&x| off[x as usize + 1] += 1);
                    for t in 0..nterms {
                        off[t + 1] += off[t];
                    }
                    let mut post = vec![0u16; off[nterms] as usize];
                    let mut cur = off[..nterms].to_vec();
                    let mut doc = 0;
                    for x in stream {
                        if x & DENSE != 0 {
                            doc = x as u16;
                        } else {
                            post[cur[x as usize] as usize] = doc;
                            cur[x as usize] += 1;
                        }
                    }
                    Chunk { base, off, post, hits }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut matches: Vec<Vec<u32>> = vec![Vec::new(); phrases.len()];
    chunks.iter().flat_map(|c| &c.hits).for_each(|&(e, d)| matches[e as usize].push(d));

    // Answer queries in blocks; any thread takes the next block.
    const BLOCK: usize = 64;
    let next = AtomicUsize::new(0);
    let (queries, chunks, bitmaps, matches, next) = (&queries, &chunks, &bitmaps, &matches, &next);
    // All documents of a SPARSE term, ascending.
    let list = |t: usize, out: &mut Vec<u32>| {
        out.clear();
        for c in chunks {
            out.extend(c.post[c.off[t] as usize..c.off[t + 1] as usize].iter().map(|&x| c.base + x as u32));
        }
    };
    let mut blocks: Vec<(usize, String)> = thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(move || {
                    let mut r = vec![0u64; words];
                    let (mut cur, mut l, mut tmp) = (Vec::new(), Vec::new(), Vec::new());
                    let mut done = Vec::new();
                    loop {
                        let b = next.fetch_add(1, Relaxed);
                        if b * BLOCK >= queries.len() {
                            return done;
                        }
                        let mut out = String::new();
                        for q in &queries[b * BLOCK..((b + 1) * BLOCK).min(queries.len())] {
                            let terms = match q {
                                Query::Phrase(p) => {
                                    format_list(&mut out, &matches[*p as usize]);
                                    continue;
                                }
                                Query::Bool(terms) => terms,
                            };
                            // The running result is a sorted list, or the bitmap r when `bits`.
                            let mut bits = false;
                            cur.clear();
                            for (i, &(op, t)) in terms.iter().enumerate() {
                                let k = kind[t as usize];
                                let dense = k != SPARSE;
                                let bm = if dense { &bitmaps[(k & !DENSE) as usize * words..][..words] } else { &[][..] };
                                let has = |d: &u32| bm[*d as usize / 64] >> (d % 64) & 1 != 0;
                                if !dense {
                                    list(t as usize, &mut l);
                                }
                                if i == 0 || (!bits && dense && op == b'O') {
                                    if !dense {
                                        std::mem::swap(&mut cur, &mut l);
                                    } else {
                                        r.copy_from_slice(bm);
                                        cur.iter().for_each(|&d| r[d as usize / 64] |= 1 << (d % 64));
                                        bits = true;
                                    }
                                } else if !bits && !dense {
                                    merge(&cur, &l, op, &mut tmp);
                                    std::mem::swap(&mut cur, &mut tmp);
                                } else if !bits {
                                    cur.retain(|d| has(d) == (op == b'A'));
                                } else if dense {
                                    for (x, &y) in r.iter_mut().zip(bm) {
                                        *x = match op {
                                            b'A' => *x & y,
                                            b'O' => *x | y,
                                            _ => *x & !y,
                                        };
                                    }
                                } else {
                                    match op {
                                        b'O' => l.iter().for_each(|&d| r[d as usize / 64] |= 1 << (d % 64)),
                                        b'N' => l.iter().for_each(|&d| r[d as usize / 64] &= !(1 << (d % 64))),
                                        _ => {
                                            cur.clear();
                                            cur.extend(l.iter().filter(|&&d| r[d as usize / 64] >> (d % 64) & 1 != 0));
                                            bits = false;
                                        }
                                    }
                                }
                            }
                            if bits {
                                format_bits(&mut out, &r);
                            } else {
                                format_list(&mut out, &cur);
                            }
                        }
                        done.push((b, out));
                    }
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    blocks.sort_unstable_by_key(|b| b.0);
    let out: String = blocks.into_iter().map(|b| b.1).collect();
    std::fs::create_dir_all(out_dir).ok();
    std::fs::write(format!("{out_dir}/results.txt"), out).expect("cannot write results.txt");
}

/// A phrase filed under its trigger term, which is terms[at].
struct Trig {
    term: u32,
    phrase: u32,
    at: u32,
    len: u32,
    terms: [u32; 8],
}

/// One chunk's share of the index.
struct Chunk {
    base: u32,             // first doc id of the chunk
    off: Vec<u32>,         // per term, where its documents start in `post`
    post: Vec<u16>,        // doc ids minus `base`, grouped by term, ascending within a term
    hits: Vec<(u32, u32)>, // (phrase end state, doc)
}

/// The id of the line starting at p.
fn doc_id(docs: &[u8], p: usize) -> u32 {
    docs[p..].iter().take_while(|c| c.is_ascii_digit()).fold(0, |a, &c| a * 10 + (c - b'0') as u32)
}

/// First line start at or after byte p whose doc id is a multiple of 64.
fn aligned_start(docs: &[u8], p: usize) -> usize {
    let mut p = line_start(docs, p);
    while p < docs.len() && doc_id(docs, p) % 64 != 0 {
        p = line_start(docs, p + 1);
    }
    p
}

/// First line start at or after byte p.
fn line_start(docs: &[u8], p: usize) -> usize {
    if p == 0 {
        return 0;
    }
    docs[p - 1..].iter().position(|&b| b == b'\n').map_or(docs.len(), |q| p + q)
}

