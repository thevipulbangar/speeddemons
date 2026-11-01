# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Both files are mmapped; nothing is copied out of them. One worker per hardware thread runs every phase, with barriers in between.

**Regions.** Each worker parses a slice of `regions.csv` twice: once to mark which `[A-Z]{3}` codes occur, once to fill a byte table `region_id -> group`. Ranking the codes that occur gives group numbers that already sort bytewise. Group 0 is `_UNMATCHED`, so the zero-filled table sends holes and out-of-range ids there for free.

**Cutting the file.** `records.csv` is split into one chunk per thread. A cut can land inside a quoted note, so each cut moves to the byte after the next LF that is followed by a digit or a BOM. Under spec 2.2 that is always a record start, but the program does not trust it: the parser is strict (any byte sequence outside the 2.1 grammar stops it), and every chunk must end exactly where the next one begins. Since the first chunk starts right after the header, induction proves every cut is a real record start. If any check fails, the cuts are recomputed from quote parity (a byte is inside quotes iff an odd number of `"` come before it) and the parse runs again. So the fast path depends on 2.2, and correctness only depends on 2.1.

**Parsing.** A hand-written parser with AVX2 quote search and SWAR digit conversion. Amounts are exact integer cents and nothing uses floats. Value lengths are `raw length - number of "" pairs`. A BOM is skipped only at a record start.

**Distinct names, exactly.** All threads share one lock-free open-addressing table. A 16-byte slot holds the length and up to 15 name bytes, so a lookup is one compare on one cache line, and the slot index is the name's id. Longer names keep their bytes in an arena. A slot is claimed with a CAS and published once complete. Names are keyed by their raw bytes between the quotes, which map one to one to the unescaped value, and both empty forms give the same key. Each name id has a shared 4-word row with one bit per group. A thread sets its bit with a plain load first and `fetch_or` only if the bit is unset; the row is prefetched for writing together with the slot. Records go through in batches of 16 so that the region and name-slot lines can be prefetched before they are needed.

Tested against the Python reference on seeds 1–4 at tiny, sample and full scale, the spec's worked example and "rules" variants, and the shared stress sets (`./stress.sh c`: 18/18 PASS).

## Dependencies

None. Only std.

## With another 24 hours

- Measure on a quiet machine. The laptop's noise (±40%) hid every change below about 20%.
- Shrink the per-record instruction count, which is about 600 now: no `Option` plumbing in the hot path, and unchecked indexing behind one bounds check per record.
- Try a 2^18-slot name table: fewer probes, at the cost of about 4 MB.
- Overlap the regions phase with the start of record parsing.
