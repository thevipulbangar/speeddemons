# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Local numbers are from a laptop (Ryzen 7 5800H, Docker Desktop on WSL2, 12 threads pinned), best of many runs; the laptop is shared and noisy, so only changes well above the noise are claimed. The scoring box has run about 1.6x faster than the laptop.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Rust, mmap, 12 chunks; pass 1 counts per (endpoint, hour) in thread-local tables, single-threaded merge, pass 2 re-parses and scatters latencies into one flat array, `select_nth_unstable` percentiles | run-da6a6ff9f273 | 0.0793 | 47.6 | 66.6 |
| 2 | Hour-blocked second pass: pass 1 only records which hours occur in each 16 KiB block; pass 2 handles one hour per thread and re-reads only that hour's blocks. No merge, no 12 MB latency array, no atomics (local: ~85 -> ~42 ms, 46 -> 26 MB) | local | 0.042 | 26 | |
| 2a | Failed: claim offsets and write under one lock. Threads queued ~40 ms behind the writer; replaced by claiming offsets under the lock and `write_all_at` outside it | local | 0.06 | 25 | |
| 2b | Failed: single pass, every line stored as a 6-byte record per (thread, hour), merged per hour afterwards. Not faster (~42 ms) and more memory (27-29 MB): building and freeing 18 MB of records cost what the second read saved | local | 0.042 | 28 | |
| 3 | One set of threads for both passes (barrier instead of two spawns); SIMD tail parse (latency digits via `maddubs`/`madd`), SWAR method skip; endpoint keys copied into a small per-thread arena and compared 32 bytes at a time with AVX2, file mapped with a zero page behind it so the compare may over-read (local CPU time -25%) | local | 0.040 | 26 | |
| 4 | One packed u64 record per line (id, latency, 5xx flag) sorted in place by endpoint (American flag sort) instead of ids + latencies + a scatter copy; endpoint ids widened to 24 bits | local | 0.040 | 25 | |
| 5 | Output buffers recycled instead of freed: freeing a big buffer is a munmap, which stalls every other thread's page faults (`osq_lock` in the profile) | local | 0.037 | 27 | |
| 6 | Hours wait for their file offset as compact rows (a line's length is known from digit counts) instead of formatted text; any idle thread formats and writes a ready hour through a 32 KiB buffer | local | 0.040 | 23 | |
