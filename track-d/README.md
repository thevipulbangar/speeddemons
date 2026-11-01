# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

The work splits into building a cache-friendly graph and then running 2,000 plain Dijkstra
searches as cheaply as possible.

**Build.** `graph.txt` is mmapped and split into 12 chunks on line boundaries. Numbers are read
with a branch-free SWAR parser. Edges are scattered into buckets of 256 consecutive tail ids
with per-thread cursors, and each bucket is then counting-sorted in cache, which gives a CSR with
no random atomics. Self-loops are dropped while parsing.

**Renumbering.** Node ids in the file are a random relabelling, so a search on them misses the
cache at every step. Twelve threads grow BFS regions from different roots, claiming nodes in an
atomic bitset; the concatenated visit order becomes the new numbering, so nodes that are close on
the map get close ids. The text is then parsed once more straight into the final CSR under the
new ids. An edge is 6 bytes: a u32 head and a u16 weight. The few weights of 65535 or more are
escaped into a small sorted side table.

**Queries.** Trivial queries are answered up front (`s == t`, a target with no in-edges, a source
with no out-edges). The rest are handed out to 12 threads through an atomic counter. Each thread
runs Dijkstra with a radix heap of 8-byte entries; the low 32 bits of a key are enough because
every queued key is within one u32 weight of the last settled one. Distances live in 64-node
blocks allocated on first touch from a per-thread pool, so memory follows the size of the search.
They are stored as u32; the rare search that reaches 2^32 - 1 is rerun with u64 slots. When a
node's distance improves, its edge row is prefetched, so it is in cache by the time it is popped.

## Dependencies

None. `mmap` is declared directly against libc.

## With another 24 hours

- Cut the build further: the third parse and its `perm` lookups are the largest build cost left.
- A better 2-D ordering than BFS wavefronts (for example a few recursive bisections) to shrink
  the working set of the long searches, which carry half of all query work.
- Try a contraction or landmark scheme sized so its preprocessing fits in about 50 ms on 12
  threads, since the per-node Dijkstra cost is now mostly compulsory cache misses.
