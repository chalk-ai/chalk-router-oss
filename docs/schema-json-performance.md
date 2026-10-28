# Retained JSON: guarantees and performance

[Documentation index](README.md) · [Development checks](development.md#testing)

Run `just schema-json-bench` to measure synthetic local requests. It makes no
provider calls and requires no credentials or database. The debug build reports
microseconds, allocation count, and cumulative allocated bytes per operation,
after one warm-up and across eight repetitions. Bytes measure allocation traffic,
not peak or retained memory. Timings are informational; CI has no timing assertions.

The examples cover 64 KiB, 1 MiB, and 8 MiB string payloads, one/four/eight
fallback candidates, and schemas nested 16/32/64 levels. Separate cold retained
schema cases use 16/32 logical levels and include ingress parsing and initial
container indexing; the constructed depth-64 fixture exceeds the existing
128-container ingress limit. The retained input strings exercise the same shared storage used for long compaction input
and embedded image data.

The following allocation comparison used the same fixture and debug build on
this workspace, before (`62d8fbd`) and after (`db3adb3`) the shared-storage refactor.
These historical measurements precede compact, unique-key wire serialization:

| Operation | Before allocated bytes | After allocated bytes |
| --- | ---: | ---: |
| Eight model edits, 64 KiB input | 3,884,343 | 5,000 |
| Eight model edits, 1 MiB input | 61,883,703 | 5,000 |
| Eight model edits, 8 MiB input | 494,945,591 | 5,000 |
| Model edit and wire serialization, 8 MiB input | 92,277,787 | 25,166,596 |
| Eight fallback candidates, 8 MiB input | 939,561,056 | 67,112,264 |
| Normalize and serialize, depth 16 | 1,299,863 | 37,969 |
| Normalize and serialize, depth 32 | 4,511,079 | 73,073 |
| Normalize and serialize, depth 64 | 16,729,351 | 143,281 |
| Canonical schema construction, depth 64 | 12,546,842 | 84,433 |

The cold retained normalization cases allocated 82,362 bytes at 16 logical
levels and 159,386 bytes at 32 levels, including ingress parsing and indexes.

Unchanged child handles share one retained source and its parsed semantic tree.
Container indexes store ranges into that source, and semantic lookup paths share
parent links rather than copying ancestor keys. Object mutations copy only
member names and handles; serialization emits the final envelope directly.
Constructed provider envelopes and normalization output do not serialize and
reparse every ancestor. Semantic views of edited objects are computed only when
requested, in one traversal. Validated object handles share both ordered fields
and cached semantic views; mutation creates an independent view only for the
edited object. Response-format inspection borrows schemas and metadata instead
of deep-cloning them. Model and stream routing changes share Responses
compatibility parsing and translation; semantic changes still invalidate it.

Final wire serialization scales with payload size. Returning owned translated
chat requests still clones their message content for each candidate. Cold
indexing of deeply nested retained containers uses serde's borrowed `RawValue`
decoder, which can scan ancestor ranges; shared storage removes subtree copying
and repeated envelope serialization, without claiming fully linear cold parsing.

Wire forwarding emits compact JSON with unique keys, preserving effective
declaration order and numeric tokens. Lookup and forwarding both resolve
duplicate members using the last value. Strict normalization transforms only
schema nodes, leaving property names and embedded data intact.
Telemetry removes encrypted-content fields and shadowed values from its retained
copy while preserving order and numbers in the content it records. Cache identity
uses schema-aware traversal: actual property dictionaries keep their order,
while schema keywords, definition names, and ordinary data objects are sorted.
Existing numeric cache semantics remain unchanged, and duplicate-bearing schemas
are uncacheable. The v3 cache namespace separates these schema-aware keys from
the previous v2 keys.

## Chat cache key

`chat_cache_key` builds the canonical request from borrowed fields instead of
cloning the request, serializing it, and reparsing it into a field map. Messages
are serialized once; only tool definitions are copied (to canonicalize their
schemas). The digest bytes are identical to the previous implementation, so the
cache format version is unchanged. Measured with `just schema-json-bench`
(request is two messages totalling the stated size plus one tool):

| body  | before allocations / bytes | after allocations / bytes |
| ----- | -------------------------- | ------------------------- |
| 1 MiB | 68 / 8,918,516             | 61 / 7,869,863            |
| 8 MiB | 68 / 71,308,788            | 61 / 62,920,103           |

The remaining cost is the single message serialization plus the final map
serialization, each a growing buffer; allocation count barely moves because the
old path was dominated by a few large buffers rather than many small ones.
