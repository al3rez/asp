# File synchronization strategy

## Sources

Read Andrew Tridgell’s [rsync algorithm technical report](https://rsync.samba.org/tech_report/) and inspected [RsyncProject/rsync](https://github.com/RsyncProject/rsync) revision `7c20b077c980036a19587701cec320cc88e42a4a`, especially `checksum.c`, `generator.c`, `match.c`, and `sender.c`.

## Rsync algorithm

The receiver divides its basis file into fixed blocks and sends each block’s cheap rolling checksum plus a strong digest. The sender scans its new file with a window of the block size. The weak checksum is updated in constant time as the window moves; candidate hits are verified with the strong digest. The sender emits literal bytes and references to receiver blocks. Insertions do not permanently shift matching because the scan advances byte by byte until it realigns.

Modern rsync negotiates stronger/faster checksum choices and contains defenses against pathological weak-checksum collision chains. The algorithm saves bandwidth when both sides hold similar large files, but requires basis signatures, full-file scanning, hashing, token generation, and an extra exchange before deltas can be sent.

## Alternatives for source code

### Compressed whole file

For small files this is usually best. Source files are commonly under tens of kilobytes; one request with compressed content is cheaper than signature negotiation and CPU-heavy chunking. QUIC does not compress application data, so ASP v17 applies bounded fast zlib at the frame layer when it wins; zstd/brotli and per-file codec selection still need measurement.

### Prefix/suffix or ordinary patch

Agents typically know the exact base version they read and make localized edits. A version/hash-guarded replacement range or edit list is one request, cheap to apply, and detects conflicts. Unified text patches are human-readable but line-context application can be ambiguous; a structured byte/range patch with expected base hash is safer.

ASP v0 implements a single prefix/suffix replacement with SHA-256 base verification and an optional negotiated multi-range replacement for scattered edits. Equal-length byte runs are detected directly; length-changing source files use a bounded line-aware matcher so independent insertions/deletions can also remain separate. The client derives ranges only from a cached exact base, uses a conservative encoded-size estimate, and falls back to the contiguous or whole-file form when ranges are not a clear win or matching would exceed its CPU/memory bounds. General rsync/CDC manifests remain future work because their extra signature exchange and indexing are not justified for the common small source-file path.

### Rsync fixed-block rolling delta

Useful when transferring a large changed file without a known shared version or when edits shift content. It is less attractive for a 4 KiB source file and does not exploit that the agent already read the exact base.

### Content-defined chunking (CDC)

CDC makes chunk boundaries stable across insertions and enables cross-file/global deduplication. It requires scanning and hashing whole files, chunk indexes, storage policy, and security controls. It is valuable for large artifacts/caches or repeated synchronization across many versions, not automatically for ordinary source edits.

## Recommended adaptive policy

| Condition | Transfer |
|---|---|
| New or ≤64 KiB file | compressed whole file |
| Agent has exact base hash and localized edits | structured range patch |
| 64 KiB–8 MiB, basis exists, similarity unknown | choose whole compressed vs rolling delta after a cheap estimate |
| Large binary/artifact with recurring dedup | CDC/chunk manifest, future work |
| Already stored content hash | content-addressed reference |

Thresholds are hypotheses and must be calibrated on real repositories. Always compare encoded patch bytes + metadata + extra RTT against compressed whole-file bytes.

## Correctness and concurrency

Every mutation carries an expected content hash/version. The server either applies atomically and returns a new version/hash, or returns `VERSION_CONFLICT`; it never guesses a fuzzy merge for an agent. Writes use a temporary file and rename. Watch events reference the resulting version. Final-component symlinks are rejected. Replacing an existing file preserves its ordinary Unix mode bits (including executable/shared-checkout bits); newly created files remain `0600` because v0 does not carry an explicit mode field. Sparse-file preservation, explicit mode negotiation, and case behavior remain v1 work.
