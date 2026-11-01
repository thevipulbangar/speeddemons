# Track C: performance log

T and M for local steps are from a shared 8-core laptop (Ryzen 7 5800H under WSL2, other jobs running), so they are noisy; only the board run is comparable with other teams. Points = 50·log10(K) where the board reports K.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Rust, mmap, one chunk per thread with cuts from a quote-parity pre-pass, per-thread aggregates, shared lock-free name table with an atomic (name, country) bitset | local | 0.35 | 13 | |
| 2 | Profiled: the shared bitset (every first sighting of a pair writes a line another core holds) and two dependent table misses per record were most of the time. Name ids now feed per-thread pair buffers, owners set bits privately afterwards | local | 0.12–0.20 | 25 | |
| 3 | Records go through in batches of 16: parse, prefetch region and name slots, then look up | local | 0.08–0.12 | 25 | |
| 4 | SWAR digit parsing (8 bytes at a time), name keys read as masked words instead of memcpy | local | ~0.09 | 25 | |
| 5 | Name table slots are 16 bytes holding the name itself (one compare, one cache line); pairs mailed in 1024-entry buffers to the owning thread, which drains between batches, so no 13 MB pair buffers; region table one byte per region; regions.csv parsed in parallel; guessed cuts (LF + digit/BOM) checked by chunk-end alignment instead of a quote-count pass; MADV_POPULATE_READ per chunk | run-a4169056e491 | 0.0601 | 11.3 | 68.8 |
| 6 | Owner and bitset row by multiply-shift ranges instead of `%` and `/` by the thread count | local | ≈ step 5 | 13 | |
| – | Failed: simdjson-style stage 1 (prefix-XOR quote mask over 64-byte blocks, separator queue). IPC went up but instructions rose 30% and wall time did not move; kept out | local | ≈ step 5 | 13 | |
| – | Failed: partitioning names by hash and mailing 24-byte keys to private per-owner tables (no shared table at all). Memory-light, but owner lookups miss L2 and the mail allocations churned; 1.5–2× slower | local | 0.10–0.19 | 37 | |
