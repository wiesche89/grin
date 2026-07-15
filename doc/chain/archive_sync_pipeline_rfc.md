# RFC: Pipelined and Batched Full-History Archive Sync

Status: Draft
Target: Grin node archive mode
Discussion branch: `block_sync`

## Summary

This RFC proposes a dedicated initial-sync pipeline for archive nodes. The
pipeline downloads full blocks concurrently, reorders them by canonical height,
performs state-independent validation in parallel, and applies consecutive
blocks to chain state in bounded atomic batches.

The proposal preserves full validation from genesis and does not use PIBD as a
state shortcut. It changes neither consensus rules nor the data retained by an
archive node.

The intended pipeline is:

```text
BLOCK_HIST peers
       |
       v
request scheduler and bounded download queue
       |
       v
canonical reorder buffer
       |
       v
parallel intrinsic validation
       |
       v
ordered, state-dependent batch application
       |
       v
atomic database commit and PMMR durability checkpoint
```

## Motivation

An archive node deliberately performs a full-history sync.
`Chain::check_txhashset_needed` returns `false` when `archive_mode` is enabled,
so archive sync does not bootstrap current state with PIBD. After header sync,
the node obtains and validates every full block from genesis.

PIHD reduces the cost and latency of obtaining the canonical header chain, but
the full-history body path remains largely unchanged:

- `body_sync` selects peers with `BLOCK_HIST` capability;
- it requests one hash per `GetBlock` message;
- hashes are assigned to random eligible peers;
- request progress is tracked as one aggregate counter;
- received blocks are processed synchronously from peer reader threads;
- out-of-order blocks use the general orphan pool;
- each block enters the normal chain pipeline independently;
- each successful txhashset extension synchronizes the output, rangeproof, and
  kernel PMMR backends;
- normal per-block post-processing, including txpool reconciliation and random
  compaction checks, remains active during initial sync.

This design leaves network, CPU, storage, and lock parallelism coupled. Faster
downloads can create more orphans and lock contention without increasing ordered
chain throughput.

### Preliminary observation

A 10-second macOS sample of an early archive sync on the development branch
showed substantially more top-of-stack samples in `fcntl`-based durability work
than in Bulletproof field arithmetic. Other peer threads spent significant time
waiting for chain locks. Process CPU utilization was below one full core.

This is evidence for the tested height, hardware, filesystem, and workload only.
Later, fuller blocks can shift the bottleneck toward Bulletproof and kernel
signature verification. This RFC therefore requires phase-specific metrics and
does not assume that either CPU or storage is universally dominant.

## Goals

1. Increase archive initial-sync throughput by using network, CPU, and storage
   more efficiently.
2. Retain validation of every historical block and every intermediate PMMR root.
3. Preserve the canonical consensus result of the existing block pipeline.
4. Bound memory, disk staging, and work accepted from untrusted peers.
5. Resume safely after process termination or power loss.
6. Isolate the optimized behavior to initial archive body sync.
7. Provide enough metrics to attribute improvements and regressions.

## Non-goals

- Changing consensus rules or block validity.
- Replacing archive sync with PIBD followed by an unvalidated block backfill.
- Pruning historical blocks, rangeproofs, or other archive data.
- Optimizing steady-state block propagation in the first implementation.
- Requiring a new P2P message in the first implementation.
- Relaxing durability without an explicit configuration and recovery design.

## Current bottlenecks

### Coupled network ingestion and chain processing

`Message::Block` calls `adapter.block_received` directly. Validation, chain
locking, database work, and PMMR updates therefore execute on a peer reader
thread. A slow chain operation prevents that thread from reading further peer
messages.

Several peer threads may enter block processing concurrently. For each block,
`Chain::process_block_single` first calls `process_block_header`, which acquires
the header PMMR and txhashset locks and opens and commits its own batch. It later
acquires those locks again and opens another batch before calling
`pipe::process_block`. State application and intrinsic crypto validation are
consequently serialized together in the second critical section.

### Coarse request accounting

`BodySync` tracks only the number of outstanding requests. It does not retain a
per-hash owner, request time, attempt count, or peer performance history. A
stalled request cannot be distinguished from a slow peer except through coarse
global timeout behavior.

Random assignment also distributes adjacent heights across peers. Responses
arrive out of order and consume the general orphan pool, whose size indirectly
limits the download window.

### Repeated per-block durability work

Each block is processed in a separate store batch and txhashset extension. A
successful extension commits its child database batch and synchronizes all three
PMMR backends. This makes durability-call frequency proportional to block count.

Even when PIHD has already validated and stored the canonical header,
`process_block_single` first calls the normal header-processing path and opens a
separate batch. `pipe::process_block` also performs the normal header/PoW checks.
Some of these calls return early for known headers, but still incur locking and
transaction overhead.

### Crypto validation cannot currently scale independently

`Block::validate` performs state-independent checks including rangeproof and
kernel signature verification. These checks currently execute while the
txhashset write lock is held. The rangeproof and signature paths also use
`static_secp_instance()`, protected by a global mutex.

Moving the same call to several worker threads without changing secp context
ownership would merely move contention to that mutex. Effective parallel
prevalidation requires a secp context owned by each worker.

This is a broad consensus-core API change, not a local scheduler refactor.
`static_secp_instance()` is called below `Block::validate` by rangeproof,
signature, coinbase, commitment-sum, and kernel-sum helpers. Passing an explicit
context through this call graph changes several consensus-critical interfaces.
It therefore needs a dedicated implementation phase and review. Context
initialization time and generator-table memory per worker must also be measured.

## Proposed design

### Activation and fallback

The new path is active only if all of the following hold:

- the node runs with `archive_mode = true`;
- the node is in initial `BodySync`;
- a locally validated header chain exists ahead of the body head;
- requested blocks target that canonical header chain.

Normal propagation, fork processing, reorg handling, and non-archive sync keep
using the existing path. Any invariant violation or unsupported chain condition
falls back to ordinary single-block processing.

The initial implementation should be guarded by an experimental configuration
option. Once equivalence and recovery testing are complete, it can become the
archive-sync default.

### 1. Metrics before behavior changes

Add timing and counters for:

- requested, received, duplicated, timed-out, and reassigned blocks per peer;
- response latency, bytes per second, and useful bytes per peer;
- reorder-buffer depth and distance from the lowest missing height;
- queue wait, intrinsic validation, state validation, PMMR apply, database
  commit, PMMR sync, and post-processing time;
- lock acquisition time for header PMMR and txhashset;
- blocks, inputs, outputs, kernels, and bytes per committed batch;
- rollback, fallback, and crash-recovery events.

Metrics must distinguish early sparse, historically busy, and current-chain
blocks. Aggregate blocks per second alone is not sufficient.

### 2. Per-block request scheduler

Replace aggregate request accounting with bounded records keyed by block hash.
A hash may have more than one live attempt after hedging, so assignment and
timing belong to the attempt rather than to the hash-level record:

```text
RequestState {
    hash,
    height,
    active_attempts: Map<PeerAddr, AttemptState>,
    completed_attempts: BoundedHistory<AttemptOutcome>,
    state
}

AttemptState {
    attempt_id,
    requested_at,
    deadline,
    kind: Primary | Hedge
}
```

The existing per-peer tracking adapter may continue to retain request options,
but the scheduler record is the authoritative source for ownership, timeout,
retry, and performance accounting.

The scheduler should:

- prioritize the lowest missing canonical heights;
- assign contiguous ranges of hashes to a peer where practical;
- maintain a configurable per-peer in-flight limit;
- use observed latency and throughput when selecting peers;
- retry timed-out hashes on a different peer;
- avoid duplicate requests unless a hedge timeout has elapsed;
- penalize invalid, unsolicited, or repeatedly stalled responses;
- retain `BLOCK_HIST` as an eligibility requirement;
- bound the total request window independently of the orphan pool.

The first valid response wins. Other live attempts for that hash become
superseded but remain recognizable until their response deadline, because a
wire request cannot be cancelled. A valid late response from a superseded
attempt is counted as a duplicate, not as unsolicited peer behavior. Invalid
responses remain attributable to the peer and attempt that supplied them.
Completed attempt history is capped and reduced to aggregate peer metrics after
expiry so hedging cannot turn request accounting into unbounded memory use.

The first version continues to send individual `GetBlock(hash)` messages. This
isolates scheduler benefits from protocol changes.

### 3. Asynchronous ingestion and canonical reorder buffer

Peer reader threads should deserialize and perform inexpensive admission checks,
then enqueue blocks instead of applying them to the chain directly.

Admission requires:

- a matching outstanding request, unless normal propagation handles the block;
- a block hash equal to the locally stored canonical header hash at its height;
- a bounded accepted height window;
- message and queue size limits.

The reorder buffer is keyed by height and stores block ownership metadata. It is
not the chain orphan pool. The apply side consumes only the contiguous prefix
beginning at `body_head.height + 1`.

Backpressure stops issuing requests when any configured byte, block-count, or
height-distance limit is reached. Blocks beyond the accepted window are rejected
or routed through the normal propagation path.

### 4. Parallel intrinsic validation

Downloaded canonical blocks are submitted to a bounded worker pool. Each worker
owns an independently initialized secp context and performs checks that do not
depend on mutable txhashset state.

Candidate checks include:

- transaction-body structure, weight, sorting, and cut-through rules;
- rangeproof verification;
- kernel signature verification;
- kernel feature and lock-height rules;
- NRD feature enablement, header-version rules, and duplicates within the block;
- coinbase structure and sums;
- per-block kernel sums using the previous canonical header's total kernel
  offset.

Header PoW and header-chain rules may use a fast path only when the entire header
is byte-for-byte/hash-identical to the locally stored, previously validated
canonical header. The general `SKIP_POW` testing option must not be reused as an
unconditional network-data bypass.

A successful result produces an in-memory `ValidatedBlock` that owns the exact
block value that was checked. The block and its private validation metadata are
not separable through the public chain API. The wrapper is not serializable and
exposes neither mutable block access nor a method that returns the block and
metadata separately. The metadata is bound to:

- the owned block's header hash;
- previous header hash;
- validation rule/version identifier;
- canonical-header epoch or equivalent reorg guard.

The ordered apply path consumes the `ValidatedBlock` and accepts its metadata
only if these bindings still match. It never accepts a separately supplied
token for an arbitrary `Block`. This is important because `Block::hash()` is the
header hash and does not by itself identify the in-memory body value that was
validated. A reorg invalidates affected queued validated blocks. Validation
metadata is never received from peers and is not persisted as trusted data.

The common initial-sync reorg is a header-tip change above the much older body
head. In that case it is sufficient to increment the canonical epoch and
invalidate only scheduler entries, buffered blocks, and validated blocks at and
above the header fork point. If the fork point reaches the applied body chain,
the batch pipeline stops and delegates rewind/reorg handling to the existing
chain path.

The apply path consumes a successful `ValidatedBlock` instead of repeating
expensive proof/signature verification. The wrapper remains a performance
optimization only: an ordinary block or a wrapper with stale metadata is routed
through the same intrinsic validator to produce a fresh `ValidatedBlock` before
state application.

### 5. Ordered state-dependent application

Only the ordered apply stage mutates chain state. For every block, in height
order, it still performs all state-dependent checks:

- parent and canonical-chain continuity;
- coinbase maturity against the state at the preceding block;
- input existence and double-spend checks;
- NRD relative-height checks against the recent-kernel index;
- cumulative block sums;
- PMMR application;
- PMMR root and size validation against that block's header;
- block, index, head, and block-sum database updates.

Every intermediate block root and size is checked. Batching must never validate
only the final root of a group.

#### One consensus apply implementation

The optimized path must not copy the state-dependent closure from
`pipe::process_block` into an archive-only implementation. The existing pipeline
should instead be factored into shared primitives conceptually similar to:

```text
validate_intrinsic(block, previous_header, crypto_context) -> ValidatedBlock
apply_state_dependent(validated_block, context) -> AppliedBlock
```

Both normal block processing and archive batch processing call the same
state-dependent function. Normal processing obtains its `ValidatedBlock`
synchronously; archive processing receives one from a worker. Construction,
fields, and access to the owned block remain private to the validation
implementation so a caller cannot manufacture a boolean "already valid" bypass
or pair validation metadata with a different block body.

Future consensus checks must have one authoritative location. A check belongs
either to intrinsic validation or to state-dependent application, and all entry
points reach that same implementation. An ordinary block, a stale wrapper, or a
wrapper for a different parent or validation-rule epoch is passed back through
intrinsic validation before any state is applied.

### 6. Atomic batch application

Apply a configurable number of consecutive, prevalidated blocks in one archive
sync unit. Initial candidates are 16, 32, and 64 blocks, additionally limited by
total serialized bytes and element count.

The batch should:

1. acquire chain mutation locks once;
2. open one outer database transaction;
3. extend txhashset state across consecutive blocks;
4. validate and record each intermediate block in order;
5. atomically publish the final database state;
6. synchronize PMMR backends once at the durability boundary;
7. release locks;
8. deliver accepted-block notifications after commit, in order.

If block `n` fails, no state from the batch may become visible. The implementation
must identify the failing block and its supplying peer, roll back the entire
batch, and retry or fall back without marking earlier blocks as permanently
processed.

#### Database and PMMR crash consistency

LMDB atomicity alone is insufficient because PMMR append files and database
metadata form one logical state. The current single-block extension provides the
required ordering:

1. commit the child LMDB transaction into its still-uncommitted parent;
2. flush and `fsync` the output, rangeproof, and kernel PMMR backends;
3. update the in-memory PMMR handle sizes;
4. commit the outer LMDB transaction containing the block, body head, block
   sums, and indexes. The body head identifies the stored header whose fields
   commit to the PMMR sizes.

The outer LMDB commit is the publication point. If the process fails after PMMR
sync but before that commit, the files can contain an appended suffix that is not
represented by the durable body head.

`PMMRBackend::new` does not truncate this suffix while opening the files. It
opens the hash and data files at their on-disk lengths; its optional header is
used for leaf-set snapshot handling. Reconciliation happens actively later in
`setup_head`:

1. load the durable body head from LMDB and resolve its stored header;
2. enter a writeable txhashset extension and call
   `rewind_and_apply_fork` for that header;
3. rewind the PMMRs to the output and kernel sizes committed by the header;
4. validate the resulting roots against that header;
5. commit the extension, whose backend flush truncates the excess file suffix.

There is also a PIBD-specific oversized-PMMR check that deliberately skips
normal root validation so an interrupted PIBD transfer can continue. The current
gate is not sufficient evidence of an active PIBD transfer: `pibd_head()`
defaults to the genesis tip when its key is absent, so a fresh archive node whose
body head is still genesis can enter the PIBD branch after an ordinary archive
sync crash.

Before batch sync is enabled, startup must use an explicit persisted
`PIBD_IN_PROGRESS` marker. PIBD sets the marker before publishing any partial
PMMR state and clears it only after the completed state has been validated and
published. The oversized-PMMR bypass is allowed only when that marker exists and
its stored target/generation agrees with the PIBD head. Outside the one-time
legacy migration below, a missing marker always selects normal archive rewind
and validation, including at genesis. The genesis default from `pibd_head()` must
not be used as proof of PIBD activity.

The marker change needs a one-time upgrade rule. On first startup with marker
support, code must query whether the `PIBD_HEAD` key actually exists instead of
calling the accessor that substitutes genesis. If the key exists, no marker is
present, and the legacy oversized-PMMR predicate indicates a partial transfer,
startup converts that persisted progress into a legacy marker and resumes PIBD.
If the key is absent, the returned genesis default is not migration evidence.
The conversion is idempotent and is removed after the compatibility window.

For ordinary archive startup, the normal rewind-and-validate path above performs
reconciliation after the PMMR backends have opened. If root validation fails,
`setup_head` rewinds to the preceding header, deletes the bad block, moves the
body head back, and retries until it finds a valid state.

That fallback does not currently cover every torn artifact. Hash, data, and size
files and the leaf-set and prune-list bitmaps are opened before `setup_head`.
A malformed artifact can therefore fail startup before the delete-and-retry loop.
Batch sync must add a pre-open recovery protocol rather than assuming that root
validation will see every failure:

1. write leaf-set and prune-list state as immutable, generation-named files;
2. `fsync` each new file and its containing directory before publication;
3. store the selected and preceding sidecar generations, logical PMMR sizes and
   roots, physical append-file lengths, and body heads in the same outer LMDB
   transaction that publishes the batch;
4. retain the preceding generation until a later checkpoint is known durable;
5. on startup, read the durable manifest first, validate the raw hash, data, and
   size-file lengths against a complete recorded generation, and truncate only
   to its verified physical boundaries before opening the files;
6. open only that generation's leaf-set and prune-list files, ignoring an
   uncommitted newer generation;
7. validate file framing, generation identifiers, and checksums before
   constructing `PMMRBackend`; if the selected generation is unreadable, restore
   the retained generation and roll the durable body head back to its checkpoint,
   or stop with an explicit recovery error if no verified generation exists;
8. rebuild the in-memory bitmap accumulator from the selected durable leaf set.
   For header version 3 and later, verify it against the checkpoint header via
   the merged output root. Earlier headers do not commit the bitmap root, so the
   manifest checksum of the selected leaf-set generation is the authoritative
   integrity check before entering `setup_head`.

The existing temporary-file write and rename can be reused, but directory
durability and generation selection must be added. Append-only PMMR suffixes
remain handled by the normal rewind; sidecar selection is handled before open.
The selected generation is also materialized at the legacy fixed sidecar paths
through the existing temporary-file and rename mechanism, so generation-named
files are additive and an older binary can still open the state. If that binary
advances the chain, a later upgrade detects that the fixed files and durable body
head no longer match the generation manifest, validates the legacy state through
normal recovery, and imports it as a new generation before batch sync is enabled.

If neither retained generation is verifiable, the default is to stop with an
explicit recovery error. Archive mode may additionally offer an operator-selected
last-resort replay: build a fresh txhashset in a temporary directory by applying
the retained full blocks from genesis, validate every intermediate root, then
atomically replace the damaged state. It must never infer or silently patch only
part of a corrupted sidecar set.

Multi-block application must preserve this ordering: all per-block child writes
remain inside one uncommitted outer transaction, every intermediate root and size
is checked, the PMMR suffix is synced once, and only then is the outer transaction
published. After a crash, startup must recover either the complete batch or the
state before the batch. Losing up to one uncommitted batch and downloading it
again is acceptable; exposing a partially applied batch is not.

The first version retains a real durability checkpoint at each batch boundary.
It must not enable multi-block commits until the explicit PIBD marker and
pre-open sidecar recovery are implemented and fault-tested. An optional
relaxed-fsync mode is a separate follow-up and must clearly state its power-loss
semantics.

### 7. Sync-specific post-processing

During initial archive body sync:

- suppress per-block txpool reconciliation and reconcile once after catching up;
- suppress random per-block compaction checks and run a controlled compaction at
  a checkpoint or after sync;
- preserve required chain event semantics, but dispatch hooks outside the chain
  mutation locks after a successful batch;
- update UI progress at a bounded frequency rather than once for every internal
  event.

Steady-state behavior remains unchanged.

### 8. Optional protocol extension

Only after the local pipeline is measured should a negotiated batch transfer be
considered, for example a request for a bounded list of canonical hashes or a
height range anchored by header hashes.

Such an extension must have a new capability bit, strict byte/block limits, and
fallback to individual `GetBlock` messages. It is not required to obtain the
initial performance gains: many individual requests may already keep the
download queue full.

## Correctness and security invariants

The optimized path must satisfy the following:

1. The accepted chain and stored archive blocks are identical to the existing
   full-history pipeline for the same inputs.
2. No peer can cause PoW or header validation to be skipped for a header not
   already validated locally and selected canonically.
3. Every block receives all intrinsic and state-dependent consensus checks
   exactly once or more, never zero times, and prevalidation metadata cannot be
   paired with a different block body.
4. Every intermediate PMMR root and size is verified.
5. State mutation remains strictly ordered by height.
6. Queue memory, staged bytes, worker count, and outstanding requests are bounded.
7. A reorg invalidates stale scheduling, buffering, and validation state.
8. Invalid data is attributable to the peer that supplied it.
9. Crash recovery returns to a valid committed checkpoint.
10. Disabling the feature restores the existing code path.

## Failure handling

- **Peer timeout:** reassign the hash, reduce the peer score, and keep lower
  missing heights at highest priority.
- **Invalid intrinsic data:** discard the block without constructing a
  `ValidatedBlock`, penalize or ban according to existing bad-data policy, and
  request the hash elsewhere.
- **State-dependent failure:** roll back the batch, isolate the first failing
  block, and verify it through the ordinary path before applying peer penalties.
- **Reorg during sync:** stop issuing work for the old epoch, drain or invalidate
  affected entries, recompute the canonical window, and resume or fall back.
- **Queue saturation:** apply network backpressure; do not spill unbounded data to
  the orphan pool.
- **Worker failure:** return the item to the validation queue with a bounded retry
  count or fall back to synchronous validation.
- **Process or power loss:** recover the last complete durability checkpoint and
  request the incomplete suffix again.

## Implementation plan

### Phase 0: Instrumentation and benchmark harness

Add the metrics above and capture an unchanged baseline. No sync behavior changes.

### Phase 1: Scheduler and download/apply separation

Introduce per-block request tracking, the bounded reorder buffer, and a dedicated
ordered apply worker. Continue calling the existing single-block chain API.

This phase validates network scheduling, backpressure, and peer attribution
without changing consensus validation or commit semantics.

### Phase 2: Safe sync fast paths

For an exact match to the locally validated canonical PIHD header, avoid redundant
header mutation/commit work and repeated PoW verification. Suppress txpool
reconciliation and random compaction during initial sync.

Each fast path should be independently benchmarked and independently disableable.

### Phase 3: Shared validation/apply factorization

Extract intrinsic validation and the state-dependent apply core without changing
which checks run or introducing parallelism. Make both the existing path and
tests call these shared primitives. Introduce the private, block-owning
`ValidatedBlock` and prove that ordinary blocks and stale wrappers always cause
intrinsic revalidation and that metadata cannot be paired with another body.

This phase prevents the optimized path from becoming a second source of
consensus logic.

### Phase 4: Explicit secp context API

Thread an explicit secp context through the intrinsic validation call graph while
preserving the current static-context path for existing callers. Treat this as a
separate consensus-core change with dedicated equivalence review. Measure context
construction cost and memory before selecting a worker count.

### Phase 5: Parallel intrinsic validation

Add bounded workers with one secp context per worker and produce reorg-bound
`ValidatedBlock` values. Continue using the existing single-block apply/commit
path so crypto parallelism can be measured independently.

### Phase 6a: Recovery prerequisites

Add the explicit PIBD-in-progress marker, generation-based PMMR sidecar manifest,
legacy-marker migration, and pre-open recovery. Verify these changes first with
the existing single-block commit path. The shared startup behavior must land and
pass its recovery fault matrix before batching is introduced.

### Phase 6b: Atomic multi-block application

Add the batch chain API. Multi-block commits remain disabled until Phase 6a has
passed on-disk upgrade, downgrade, and fault-injection tests. Start with small
batches, then tune adaptive limits from measured commit cost and block size.

### Phase 7: Optional P2P batch transfer

Implement only if metrics show that request framing, latency, or peer serving
overhead remains material after the local pipeline is optimized.

## Testing

### Equivalence

- Sync the same fixed block corpus through old and new paths.
- Compare body head, header head, tail, every stored block hash, block sums, PMMR
  sizes and roots, bitmap accumulator, indexes, and final database state.
- Repeat across historical hard-fork boundaries.
- Include empty, maximum-weight, NRD, coinbase-spend, and forked blocks.
- Restart an ordinary non-archive PIBD sync with the new marker semantics and
  confirm that download, validation, completion, and later startup behave like
  the existing path.

### Invalid data

Inject invalid PoW/header matches, rangeproofs, kernel signatures, sums, inputs,
coinbase maturity, roots, sizes, ordering, duplicates, and unsolicited blocks.
Confirm identical acceptance/rejection and peer-penalty behavior.

Keep `ValidatedBlock` fields private and add compile-fail/API tests showing that
callers cannot construct one or replace its body. Internally inject stale parent
hashes, validation-rule versions, and canonical epochs and confirm they force
intrinsic revalidation. Repeat after a header-tip reorg and confirm fallback
validation rejects invalid data.

### Concurrency and resource bounds

- Randomize response order and latency across several peers.
- Issue a hedge, deliver the primary and hedge responses in both orders, and
  confirm the first valid response wins while the other is counted as a
  duplicate rather than unsolicited data.
- Deliver invalid primary and hedge responses independently and confirm the
  exact supplying peer is penalized.
- Disconnect peers with outstanding ranges.
- Saturate each queue independently.
- Trigger reorgs while blocks are downloaded, validating, and awaiting commit.
- Run race-detection or concurrency-model tests where available.

### Crash recovery

Use fault injection before and after every database commit and PMMR flush/sync
boundary. Kill the process at each point, restart, validate the complete chain
state, and resume sync.

Cover at least these startup cases explicitly:

- a complete synced PMMR suffix with an outer LMDB transaction that was never
  published: `setup_head` must rewind to the pre-batch body head and truncate it;
- a malformed hash, data, or size file detected during open: pre-open recovery
  must select a verified checkpoint or stop with an explicit recovery error;
- a missing, truncated, checksum-invalid, or wrong-generation leaf-set or
  prune-list sidecar: startup must select the retained generation before
  constructing `PMMRBackend`, then rebuild the bitmap accumulator and verify it
  through the merged output root for header version 3 or later, or through the
  selected leaf-set manifest checksum for earlier headers;
- a persisted `PIBD_IN_PROGRESS` marker and matching target with oversized PMMR
  files: the PIBD-resume branch must remain distinct from archive recovery;
- an upgrade with an in-flight legacy PIBD transfer, a physically present
  `PIBD_HEAD` key, oversized PMMR files, and no marker: startup must convert the
  state once and resume PIBD rather than enter archive recovery;
- a fresh archive node that never ran PIBD and crashed near genesis with
  oversized PMMR files: the absent marker must select normal archive
  rewind-and-validation, never the PIBD bypass;
- stale or mismatched PIBD marker metadata: startup must reject the bypass and
  take the normal recovery path;
- no verifiable sidecar generation: normal startup must stop explicitly, while
  an operator-requested archive replay must rebuild in a temporary directory and
  publish only after full validation;
- upgrade, downgrade to a legacy binary, advance the chain, and upgrade again:
  the legacy binary must use the fixed sidecar paths and the second upgrade must
  validate and import that state as a new generation before enabling batching;
- a crash after the outer LMDB commit: startup must retain the complete batch.

### Performance

Benchmark at least:

- sparse early-chain blocks;
- historically busy/full blocks;
- recent blocks;
- one local low-latency archive peer;
- several WAN archive peers with asymmetric latency and bandwidth;
- warm and cold filesystem cache;
- batch sizes 1, 8, 16, 32, 64, and adaptive;
- crypto worker counts 1 through the number of physical cores.

Report blocks/s, MB/s, elements/s, CPU utilization by core, disk write and sync
latency, queue occupancy, memory, and time spent in each pipeline stage. Compare
each phase against the unchanged baseline, not only against the preceding phase.

## Rollout

1. Land metrics with no behavior change.
2. Ship the pipeline behind an experimental archive-sync option.
3. Keep the old path available as a runtime fallback.
4. Collect full-sync results on Linux and macOS with multiple storage devices.
5. Enable by default only after equivalence and crash-injection tests pass.
6. Remove the old archive initial-sync path only after a separate review.

## Alternatives considered

### PIBD followed by historical block backfill

PIBD transfers current txhashset state, not the complete historical block set.
Applying older blocks through the normal forward chain pipeline after installing
current state is not valid. A separate archival backfill representation and
verification model would be a larger protocol and security change. It is outside
this RFC.

### Increase only the request window

A larger request window can improve utilization when the node is network-bound,
as shown by the initial experiment. Alone, however, it increases reordering,
orphan pressure, and concurrent chain-lock contention. It should be controlled by
the new scheduler and bounded queues.

### Only replace the global secp mutex

The current pipeline already serializes validation under the txhashset mutation
lock. Independent secp contexts become valuable after intrinsic validation is
moved outside that lock; changing the context alone does not create a parallel
pipeline.

### Disable fsync globally during sync

This may improve throughput but has unclear power-loss semantics and does not
address repeated locking, validation coupling, or network scheduling. Bounded
atomic batches with explicit recovery are preferred. Relaxed durability may be a
separate opt-in experiment.

## Open questions for review

1. Which parts of `Block::validate` can be cleanly exposed as an intrinsic
   validation API without duplicating consensus logic?
2. Which read-only `ValidatedBlock` accessors are required by metrics and peer
   attribution without exposing mutable body access or separable metadata?
3. What is the minimum secp context capability required by each worker, and what
   is the acceptable initialization/memory cost per worker?
4. Can the existing PMMR extension safely span multiple blocks while retaining
   per-block root checks and reliable rollback to the pre-batch sizes?
5. What is the smallest generation manifest that lets startup select a verified
   PMMR sidecar set before open and safely retire the preceding generation after
   publication?
6. Should accepted-block hooks be emitted for every block after commit, or should
   initial-sync consumers receive a batch/checkpoint event?
7. Which txpool and compaction operations can be skipped during sync without
   changing externally observable correctness?
8. Is contiguous range assignment sufficient with individual `GetBlock`
   messages, or do measurements justify a negotiated batch message?
9. What adaptive batch limit best bounds rollback cost: block count, bytes,
   outputs/kernels, estimated PMMR writes, or a combination?
10. Which fallback boundary is safest when the canonical header chain changes
    while a batch is being validated or applied?
