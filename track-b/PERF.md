# Track B: performance log

Times marked "local" are from an 8-core Ryzen laptop under Docker, with other jobs running on it, so they move by up to 2x from run to run. Board runs on the scoring box are listed with their run IDs.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2,200 | 0 |
| 1 | Rust, one parallel pass. Only query terms are looked up. Atomic bitmaps for frequent BOOL terms, per-thread `Vec<Vec<u32>>` lists for the rest. Phrases are matched by an Aho-Corasick automaton over term ids in the same pass. BOOL queries run on a scratch bitmap | local | 1.3-4.1 | 403 | |
| 2 | Two passes: count first, then write each list straight into one exact-size CSR array. Lists stay sorted, so BOOL runs on lists and switches to a bitmap only for a dense operand | local | 1.0-1.5 | 86 | |
| 3 | AVX2 tokenizer: token starts, token lengths and line ends all come from 64-byte bit masks. Short tokens (8 bytes or less) are their own hash key | local | ~0.9 | 86 | |
| 4 | Terms renumbered by frequency in a 1 MB sample, so hot terms share cache lines. Per-thread state packed into one array | local | ~0.9 | 90 | |
| 5 | One pass instead of two. Bitmap terms are chosen from the sample. Chunks start at a doc id divisible by 64, so every chunk owns whole bitmap words and plain stores replace `lock or`. Sparse hits go into a per-chunk stream that becomes a per-chunk CSR with u16 doc ids | local | 0.73-1.1 | 93 | |
| 6 | Query phase reuses its buffers. `perf` showed 1.3 s of system time from glibc arena-lock futexes and page faults on every list allocation. Also `mallopt` so freed memory is kept | local | 0.43 | 117 | |
| 7 | Lexicon slot holds tag + id + kind (one cache line per token). The automaton skips its failure chain for terms that never come second or later in a phrase | local | 0.42-0.65 | 118 | |
| - | Failed: the first one-pass version (board run-f66016a2e0b9) reused the query buffer without clearing it between queries. With dynamic work stealing the output depended on scheduling, so it gave a different hash on every run. Fixed in step 7; 8 full runs now give identical, correct output | run-f66016a2e0b9 | 0.387 | 119 | FAIL |
| 8 | Step 7 measured on the board | run-8a593b5412b6 | 0.4006 | 120 | 59.3 |
| 9 | Same code, second board run | run-14c440ae5699 | 0.427 | 120.6 | |
| 10 | Aho-Corasick removed. Each phrase is filed under its rarest term (by the sample): the earlier terms are checked against a ring of the last 8 term ids, the later ones as the next tokens arrive. A trigger entry carries the whole phrase. Lexicon lookups now lag one 64-byte block behind a prefetch. Bitmap terms skip the per-doc `seen` check, which is u16 now. Locally, A/B in one container against step 7: 0.62 s -> 0.45 s, 12.6 -> 9.1 G cycles | local | -28% | 117 | |
| - | Tried and dropped, no measurable gain: a 128 KB bit filter in front of the lexicon, and a 2048-term hot table in L2 | local | | | |
