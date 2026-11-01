# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

The queries are read first. Every query term gets an id in a small open-addressing lexicon, and nothing else from the corpus is ever looked up. A 1 MB sample of the corpus gives rough document frequencies. They are used twice: terms are renumbered so frequent ones sit together in every per-term array, and BOOL terms that occur in more than 1/16 of the documents get a bitmap instead of a doc list.

`docs.txt` is mmapped and split into one chunk per thread. Every chunk starts at a doc id divisible by 64, so each thread owns whole bitmap words and needs no atomics. One pass per chunk:

- Tokenize with AVX2. Each 64-byte block becomes a token-byte mask and a newline mask. Token starts and token lengths come from bit tricks on those masks, so there is no per-byte loop. A token is lowercased by OR-ing 0x20 and cut to length with a mask.
- Look the token up in the lexicon. One slot holds the tag, the term id and the term kind. Tokens of 8 bytes or less are their own tag. Longer ones use a hash and are checked against the stored key.
- BOOL terms: set the bitmap bit, or append the term id to the chunk's hit stream. After the pass the stream is counting-sorted into the chunk's own CSR index of u16 doc ids.
- PHRASE queries: run an Aho-Corasick automaton over term ids. A phrase match is reported in the same pass, so positions are never stored. Terms that never come second or later in a phrase skip the failure-link walk.

BOOL queries are evaluated strictly left to right on sorted lists (merge, or binary search when one side is 16x smaller). The result switches to a bitmap only when an operand is a bitmap term. Count, sum and the first ten ids are read straight off the list or bitmap. The id sum of a bitmap word is computed from six popcounts. Queries are answered in blocks of 64 that threads take from an atomic counter, and the blocks are joined in order.

## Dependencies

None. Only the Rust standard library is used, plus `mmap` and `mallopt` declared from libc.

## With another 24 hours

- Cut the memory to about 40 MB. The hit stream (about 44 MB) exists only to size the per-chunk CSR. Writing it as u16 term ids (hot terms are numbered first), or radix-partitioning it, would halve it.
- Pack the automaton's goto table into 8-byte slots. In `perf` it is now the largest single cost after the lexicon.
- Measure on quiet hardware. Local timings on a shared laptop vary by 2x.
