# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

The log is memory-mapped and read twice by twelve threads.

The first pass is deliberately dumb. Each thread takes a twelfth of the file and, for every line, looks only at the hour in the timestamp. For every 16 KiB block of the file it records which of the 72 hours appear in it, as a 72-bit mask. Consecutive lines nearly always share an hour, so most lines cost one 8-byte compare. This pass is limited by memory bandwidth, not by the CPU.

The second pass works one hour at a time. Hours are handed out in order from an atomic counter. A thread reads only the blocks whose mask has its hour, which is about 3% more than the hour's own data because the log is only mostly in time order. Lines from other hours are skipped. For each line it finds the endpoint and latency without byte loops: the method's end is found with a SWAR zero-byte test, and the latency digits are decoded with SSE multiply-adds. The endpoint gets a small id from a per-thread hash table whose keys live in a compact arena and are compared 32 bytes at a time. Each line leaves an id (with the 5xx bit) and a latency. The hour's latencies are then counting-sorted by endpoint, in name order, and three `select_nth_unstable` calls give p50, p95 and p99 (nearest rank).

Correctness never depends on how disordered the file is. A line far from its hour only makes one more block get scanned for that hour.

Output: each hour is formatted into its own buffer. Finished hours claim their file offset in output order under a small lock, then write with `pwrite` outside it, so threads never queue behind a writer.

## Dependencies

None. Only `std`. `mmap` is declared by hand.

## With another 24 hours

- Profile on the actual scoring CPU. At 2.5 GHz the per-line compute may matter more than memory bandwidth, which dominates on the laptop.
- Fold the first pass's work into something the second pass can reuse.
- Look for a percentile shortcut for the many tiny groups in the Zipf tail.
