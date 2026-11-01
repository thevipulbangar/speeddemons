# Track D: performance log

Every step from the reference to the current version. Times marked "local" were measured with
`./test.sh d full` on a shared 8-core Ryzen 5800H laptop (12 threads pinned, other jobs running,
so they swing by up to 2x); A/B comparisons were run interleaved in one container to cancel the
noise. Points use the scoring formula against the reference row.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Rust, mmap + 2-pass parallel parse into CSR (atomic degree counts), Dijkstra per query with a std binary heap, 12 threads | local | 2.5 | 219 | |
| 2 | BFS region renumbering (12 roots) so neighbours get nearby ids; distances in a paged per-thread pool | local | 1.6 | 82 | |
| 3 | Build without random atomics: bucket scatter with per-thread cursors + in-cache counting sort; parallel perm | local | 0.50 | 81 | |
| 4 | Radix heap instead of binary heap | local | 0.45 | 88 | |
| 5 | Prefetch a node's edge row when it is pushed | local | 0.40 | 85 | |
| 6 | 6-byte edges (u32 head + u16 weight, heavy weights in a side table) | local | 0.36 | 79 | |
| 7 | 8-byte heap entries (low 32 bits of key + node), pool address space reserved up front | local | 0.33 | 68 | |
| 8 | SWAR number parser (branch-free, 3 multiplies per number) | local | 0.30 | 68 | |
| 9 | BFS prefetches the row 8 queue slots ahead; pass 3 prefetches perm lookups in batches of 32 | local | 0.27 | 69 | |
| 10 | u32 distance pools, rerun with u64 only if a distance reaches 2^32 - 1 | local | 0.27 | 60 | |
| - | Board check of an earlier commit (5844050) | run-bcc760902372 | 0.3525 | 65.9 | 61.2 |

## Experiments that did not pay

| Change | Result |
|---|---|
| Transparent huge pages (MADV_HUGEPAGE) for the big arrays | No measurable change; faults were not the bottleneck. |
| Bidirectional Dijkstra with a reverse CSR | 38% fewer edge scans, but single-thread time unchanged and +40 MB. Dropped. |
| Heap of bare node ids (4 B) with keys read from the distance table | Heap 10 MB to 2 MB, but queries 12% slower. Replaced by the 8 B variant. |
| Non-atomic cursors in pass 3 (timing-only test) | Same time: contention, not the lock, was the cost. |
| Extra counting pass so pass 3 can use per-thread cursors | Slower overall: one more parse costs more than the atomics. |
| Keep weights in the first CSR and gather into the new order (no third parse) | Build -36 ms, but peak 68 to 88 MB: a wash on points. |
| Distance block size 32 instead of 64 | Pool 12% smaller, no speed change. |
| Shrinking heap buckets after each query | No change in peak. |
| MADV_DONTNEED on the pool after big searches | Peak -7 MB, but re-faulting cost more time than it saved. |
