# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Local numbers are from a laptop (Ryzen 7 5800H, Docker Desktop on WSL2, 12 threads pinned), best of many runs; the laptop is shared and noisy, so only changes well above the noise are claimed. The scoring box has run about 1.6x faster than the laptop.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Rust, mmap, 12 chunks; pass 1 counts per (endpoint, hour) in thread-local tables, single-threaded merge, pass 2 re-parses and scatters latencies into one flat array, `select_nth_unstable` percentiles | run-da6a6ff9f273 | 0.0793 | 47.6 | 66.6 |
| 2 | Hour-blocked second pass: pass 1 only records which hours occur in each 16 KiB block; pass 2 handles one hour per thread and re-reads only that hour's blocks. No merge, no 12 MB latency array, no atomics. One thread set for both passes (barrier), SIMD tail parse, SWAR method skip, endpoint keys in a small per-thread arena compared 32 bytes at a time (file mapped with a zero page behind it so the compare may over-read); offsets claimed under a lock, `write_all_at` outside it | run-fb7235e1fcc1 | 0.0362 | 25.9 | 76.6 |
| 2a | Failed: claim offsets and write under one lock. Threads queued ~40 ms behind the writer | local | 0.06 | 25 | |
| 2b | Failed: single pass, every line kept as a 6-byte record per (thread, hour) and merged per hour. Not faster locally (~42 ms) and more memory (27-29 MB) | local | 0.042 | 28 | |
| 3 | Failed (reverted): packed u64 records grouped in place (American flag sort), hours waiting as compact rows formatted later by any free thread, row names taken from the mapping. Lower local memory (23 MB) but slower on the box: the in-place sort is a chain of dependent random swaps, and formatting from names scattered over the 239 MB mapping misses cache and TLB on every row; also +74 lines | run-50971e2c5fdd | 0.0473 | 26.1 | |
| 3a | Neutral (not kept): recycle output buffers instead of freeing them (avoids munmap stalls). No measurable change in CPU time | local | | | |
| 3b | Not applicable: O_DIRECT output to keep written pages out of the cgroup. On the box /out is a bind mount of a tmpfs (`/srv/speeddemon/work`, see harness `setup-box.sh`), so the 10 MB lives in tmpfs pages charged to us whichever way it is written | | | | |
| 4a | Neutral (not kept): one call site in the line loop (last SIMD load masked, reading into the padded mapping) so the per-line closure inlines. Closure inlined, but instructions +1.5% (perf stat, 1.234 G -> 1.25 G per run) | local | | | |
| 4b | Not worth it: percentile selection is only ~110 M of 1.23 G instructions (measured by replacing it with a plain index) | local | | | |
| 4c | Failed: scan blocks one by one instead of merging runs of blocks (-8 lines), instructions +3-4% | local | | | |
| 5 | Not kept: newline scan 64 bytes per step (two compares, one 64-bit mask) and digit formatting through a fixed 24-byte block (no memcpy call per number). User instructions -6% (1.162 G -> 1.095 G, perf stat), but local wall and CPU time did not drop: the passes are bound by memory, not instructions | local | | | |
