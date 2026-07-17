# 004: Outcome-Aware Evidence Retention

| Field | Value |
| --- | --- |
| Status | Proposed |
| Specification | 004 |
| Product | `bazel-mcp` |
| Last updated | 2026-07-15 |

## Summary

Replace blanket retention of complete invocation evidence with an
outcome-aware policy. Raw stdout, stderr, and Build Event Protocol (BEP) data
are still captured locally before reduction, but terminal processing retains
only the evidence required to serve an advertised follow-up view. Every
retained view has a bounded, outcome-specific time to live (TTL).

The store persists a view-availability manifest for each terminal invocation.
`bazel.run` derives `available_views`, `more_available`, and `inspect_hint` from
that manifest. `bazel.inspect` reads the same manifest before reading files or
normalized rows. An unavailable view returns a normal structured result such
as `not_retained`, `expired`, or `evicted`; it does not surface an expected
retention event as a filesystem or MCP tool error.

Evidence purging and invocation deletion become separate operations. Expiry
and quota garbage collection (GC) run at startup, after terminal transitions,
and periodically for the lifetime of the server.

## Relationship to specifications 001 through 003

This specification makes a deliberate amendment to specification 001:

- Product principle 3, **Preserve evidence**, is replaced by:

  > Raw evidence is captured locally before reduction. It is retained only
  > when required by an advertised follow-up view, under a bounded,
  > outcome-specific TTL.

- Section 13.1 no longer requires the complete BEP file to remain for the
  invocation retention period. BEP is working evidence and is purged after the
  terminal reduction attempt.
- Section 16.3's requirement that complete captured files remain inspectable
  after restart applies only to files selected for retention whose view TTL has
  not elapsed.
- Section 16.4's seven-day retention applies to invocation metadata and the
  compact summary, not to all raw and normalized evidence.
- Section 16.4's size setting is a retained-evidence high-water mark, not a hard
  bound on live capture bytes or physical filesystem allocation.
- Section 17.3 continues to require redaction before summaries, Turso text
  fields, inspection results, and telemetry. This specification reduces the
  duration for which unredacted raw capture files exist.

Specification 002 remains authoritative for crate dependency direction and
storage boundaries. In particular, only `bazel-mcp-server` depends on `rmcp`,
Turso remains the production database driver, reducers remain deterministic,
and the store accepts protocol-neutral domain types.

This specification is the separate evidence-retention decision anticipated by
specification 003. An unexpired deferred task protects the compact invocation
record and summary needed to reconstruct its final result. It does not retain
stdout, stderr, BEP, or normalized follow-up rows beyond the policy in this
document. Because specification 003 reserves migration `0005`, this
specification uses `0006_invocation_views.sql`. If implementation order changes,
the change uses the next unused migration number and both specifications are
updated before either migration is released. Released migrations are never
renumbered or edited.

## Motivation

The current implementation captures stdout, stderr, and BEP into invocation
files, reduces the result, and keeps the entire invocation directory until a
single invocation-wide retention pass deletes it. This has four problems:

1. Successful invocations retain logs and BEP even when the bounded result is
   complete and no follow-up view needs them.
2. `available_views` is a static list, so the server may advertise a view whose
   backing evidence was never useful, has expired, or has been removed.
3. A missing log file becomes an I/O or tool error even when policy
   intentionally removed it.
4. Retention runs only during server startup, so a long-lived process can grow
   indefinitely after startup.

The service needs raw capture while Bazel runs because reduction can fail and
some commands are fundamentally text-oriented. It does not need blanket
post-terminal retention. Separating capture from retention preserves reducer
fallbacks without turning every successful build into seven days of stored raw
logs and BEP.

## Goals

- Capture complete stdout, stderr, and required BEP before or during reduction.
- Make one durable view manifest the source of truth for availability.
- Retain evidence only when it backs a useful, advertised follow-up view.
- Apply short defaults to successful follow-up data and longer defaults to
  unsuccessful diagnostic data.
- Purge BEP and other non-view intermediate data after terminal reduction.
- Return deterministic structured availability for expected retention events.
- Preserve compact invocation metadata independently from follow-up evidence.
- Run expiry and quota GC throughout a long-lived server process.
- Make partial evidence purges crash-safe and restart-recoverable.
- Keep retention decisions deterministic and independently testable.
- Preserve model-visible byte ceilings and redaction requirements.

## Non-goals

- Eliminating local spooling while an invocation is running.
- Providing a public raw BEP inspection view.
- Retaining raw evidence for operator debugging when no model-facing view uses
  it.
- Secure erasure guarantees from the filesystem, storage device, Turso free
  pages, snapshots, or backups.
- A hard physical-disk limit while running invocations have unbounded capture.
- Changing Bazel invocation arguments, output-base behavior, or reducers'
  semantic output.
- Adding MCP tools. The server continues to expose exactly `bazel.run`,
  `bazel.inspect`, and `bazel.cancel`.
- Extending task TTL merely to keep follow-up evidence available.

## Terminology

- **Capture evidence:** stdout, stderr, BEP, or another complete stream written
  locally so reduction does not depend on model-visible output.
- **Working evidence:** capture or intermediate data used during reduction but
  not directly exposed by an inspect view. BEP and `artifacts.json` are working
  evidence in the current implementation.
- **Normalized evidence:** redacted, structured rows such as diagnostics, test
  results, coverage summaries, artifact metadata, and query rows.
- **Compact summary:** the bounded summary stored with the invocation and used
  to reconstruct `bazel.run` or a deferred task result.
- **View manifest:** durable per-invocation records describing whether each
  inspect view is available, why it is unavailable, and when it expires.
- **Logical retained bytes:** bytes attributed to retained views at write time.
  This is the deterministic quantity used for quota selection.
- **Metadata deletion:** removal of the invocation record, compact summary, view
  manifest, remaining normalized rows, and invocation directory after all
  protections expire.
- **Evidence purge:** removal of one view's backing rows or files without
  deleting the invocation record or compact summary.

## Invariants

1. Raw evidence is available to terminal reduction before policy can purge it.
2. A terminal invocation advertises a view only when its manifest state is
   `available` and the required backing evidence was durably written first.
3. Inspection consults the manifest before decoding a view cursor, querying
   view rows, or opening view files.
4. Policy-driven unavailability is a normal domain result, not an MCP protocol
   error.
5. BEP is never retained by default because no advertised view reads raw BEP.
6. The raw stdout and stderr captures are retained or purged together. The
   public `log` view reads an encoding-neutral, normalized evidence file and
   never exposes stream identity.
7. No GC operation deletes a nonterminal invocation.
8. An unexpired deferred task may protect metadata and the compact summary but
   never extends a follow-up view TTL.
9. Raw bytes never enter Turso text fields, summaries, status messages, or
   telemetry.
10. Purging one view does not remove unrelated retained views.
11. All expiry calculations and quota ordering use persisted timestamps and
    stable tie-breakers.

## View inventory and backing evidence

The per-invocation manifest contains exactly the eight public inspection views
from specification 001. An internal invocation-listing operation, if present,
is not a per-invocation view and has no manifest row.

| View | Backing data | Retention unit |
| --- | --- | --- |
| `summary` | Compact summary in the invocation record | Invocation metadata |
| `diagnostics` | Redacted diagnostic rows | All diagnostic rows for the invocation |
| `tests` | Redacted test-result rows and bounded failure detail | All test rows for the invocation |
| `test_log` | Private failed-test snapshots plus redacted line evidence | All failed-test evidence for the invocation |
| `coverage` | Redacted coverage summary and per-file rows | All coverage rows for the invocation |
| `artifacts` | Bounded artifact metadata rows | All artifact rows for the invocation |
| `query_results` | Redacted query rows | All query rows for the invocation |
| `log` | Redacted normalized failure-evidence strings, derived from both raw captures | The evidence file and raw captures together |

Target-result rows that are not exposed by a public view are working evidence.
After their contribution to the compact summary is committed, they are purged
with other non-view intermediate data. `artifacts.json` duplicates normalized
artifact metadata and is also working evidence; inspection continues to read
the Turso artifact rows.

Artifact view availability describes retained artifact metadata, not the
lifetime of Bazel outputs in the workspace or remote cache. If an artifact
reference later cannot be resolved, the available artifact view returns an
item-level availability reason as required by specification 001.

The `log` view is a deterministic, encoding-neutral view derived from both
streams. It MUST NOT silently omit a nonempty stream, but stdout/stderr choice
and failure sequencing remain internal implementation details. The public shape
is always `items: [string]`; there is no source field or stream selector.
Normalization, redaction, exact deduplication, actionable-line ranking, a
bounded fallback tail, and opaque forward pagination apply before return.

The `test_log` view uses the same public string shape. Complete failed-test logs
are first copied into private invocation storage so a later Bazel invocation
cannot mutate prior evidence. Redacted line records provide bounded filtering
and pagination. A literal line match includes one redacted line of same-target
context on either side; overlapping windows merge in source order, and every
context line consumes the ordinary item and byte budgets. Missing, remote,
rejected-by-containment, and expired evidence
have explicit reasons.

## Default retention policy

### Configuration defaults

`ServerConfig` gains the following settings:

```toml
metadata_retention_days = 7
successful_follow_up_ttl_seconds = 3600
unsuccessful_follow_up_ttl_seconds = 86400
gc_interval_seconds = 300
maximum_retained_evidence_bytes = 10737418240
```

- `metadata_retention_days` MUST be greater than zero.
- Either follow-up TTL MAY be zero. Zero means that matching follow-up views are
  recorded as `not_retained` and their evidence is purged after reduction.
- `gc_interval_seconds` MUST be greater than zero.
- `maximum_retained_evidence_bytes` MUST be greater than zero.
- Durations are converted to absolute millisecond timestamps with saturating,
  deterministic arithmetic.
- Settings are read only at startup and apply to invocations that reach a
  terminal transition under that server process. Changing settings does not
  retroactively shorten or lengthen a persisted view TTL.
- A longer newly configured TTL MUST NOT resurrect an expired, evicted, or
  not-retained view.

For one compatibility release, `retention_days` is accepted as a deprecated
alias for `metadata_retention_days`, and `maximum_storage_bytes` is accepted as
a deprecated alias for `maximum_retained_evidence_bytes`. Supplying both names
with different values is a startup error. The example configuration and README
use only the new names.

### Outcome classes

`succeeded` is the successful outcome class. `failed`, `cancelled`,
`timed_out`, and `interrupted` form the unsuccessful outcome class. A spawn or
post-acceptance execution failure uses the terminal invocation state selected
by the existing lifecycle rules and therefore belongs to one of those classes.

### Policy matrix

| Evidence | Successful outcome | Unsuccessful outcome |
| --- | --- | --- |
| Compact metadata and summary | 7 days | 7 days |
| Nonempty normalized follow-up view | 1 hour | 24 hours |
| `log` | 1 hour only when selected as an explicit fallback | 24 hours when either stream contains bytes |
| `test_log` | Not retained | 24 hours when a failed-test snapshot exists |
| BEP | Purge after reduction attempt | Purge after reduction attempt |
| Non-view normalized/intermediate data | Purge after summary commit | Purge after summary commit |

A successful `log` view is selected only when bounded reduction explicitly
reports that log inspection may provide omitted information. Initial selection
conditions are:

- an informational or generic-text result was truncated;
- structured post-processing failed and the compact result uses captured text
  as its fallback; or
- another deterministic reducer outcome sets a typed `log` inspect hint.

A nonempty normalized view is retained only when it contains semantically
useful items. An empty table does not make a view available. The initial command
examples are:

| Invocation | Expected available views at completion |
| --- | --- |
| Successful build | `summary`, plus nonempty `artifacts` or other normalized views |
| Successful test | `summary`, plus nonempty `tests`, `diagnostics`, and `artifacts`; `test_log` only when a flaky/failing snapshot exists |
| Successful coverage | `summary`, plus nonempty `coverage`, `diagnostics`, and `artifacts` |
| Successful query | `summary`, `query_results` when rows exist |
| Successful truncated informational command | `summary`, `log` |
| Unsuccessful build-like command | `summary`, nonempty normalized views, and nonempty `log` |
| Spawn failure with no capture bytes | `summary`; `log` is `not_applicable` |

This table describes selection, not a promise that every named view exists for
every command. The persisted row counts and reducer facts determine the exact
manifest.

The `summary` view remains available while its invocation metadata is retained.
If an unexpired deferred task extends the effective metadata lifetime, the
summary manifest expiry is extended to the same time. Other view expiries are
unchanged.

## Domain types

`bazel-mcp-types` gains protocol-neutral types equivalent to:

```rust
pub enum InvocationView {
    Summary,
    Diagnostics,
    Tests,
    TestLog,
    Coverage,
    Artifacts,
    QueryResults,
    Log,
}

pub enum ViewAvailability {
    Available,
    Pending,
    NotRetained,
    NotApplicable,
    Expired,
    Evicted,
    Unavailable,
}

pub enum ViewAvailabilityReason {
    OutcomePolicy,
    NoItems,
    NoCapturedBytes,
    RetentionTtlElapsed,
    StorageQuota,
    BackingEvidenceMissing,
    PurgeFailed,
}

pub struct InvocationViewRecord {
    pub invocation_id: InvocationId,
    pub view: InvocationView,
    pub availability: ViewAvailability,
    pub reason: Option<ViewAvailabilityReason>,
    pub available_at_ms: Option<i64>,
    pub expires_at_ms: Option<i64>,
    pub purged_at_ms: Option<i64>,
    pub item_count: u64,
    pub logical_bytes: u64,
}
```

`Pending` is synthesized for a nonterminal invocation and is not stored as a
terminal manifest row. `Unavailable` represents an integrity or purge failure,
not an ordinary policy choice. Enum serialization uses stable snake-case names.

`InvocationSummary.inspect_hint` becomes `Option<InvocationView>`. It MUST NOT
remain an arbitrary string. The compact summary does not store a copy of
`available_views`, because that copy would become stale after expiry or quota
eviction.

The policy crate defines a typed `EvidenceRetentionPlan` containing one
decision for each view plus internal purge decisions. The plan is a pure
function of:

- command classification;
- terminal outcome;
- persisted item and byte counts;
- reducer truncation and fallback facts;
- terminal timestamp; and
- validated retention configuration.

It does not inspect the filesystem, run Bazel, or depend on MCP wire types.

## Durable view manifest

### Turso migration

Add append-only migration `0006_invocation_views.sql` equivalent to:

```sql
CREATE TABLE invocation_views (
    invocation_id TEXT NOT NULL
        REFERENCES invocations(id) ON DELETE CASCADE,
    view TEXT NOT NULL,
    availability TEXT NOT NULL,
    reason TEXT,
    available_at_ms INTEGER,
    expires_at_ms INTEGER,
    purged_at_ms INTEGER,
    item_count INTEGER NOT NULL,
    logical_bytes INTEGER NOT NULL,
    PRIMARY KEY (invocation_id, view)
);

CREATE INDEX invocation_views_expiry
    ON invocation_views(availability, expires_at_ms, invocation_id, view);
```

The implementation uses the conservative SQL subset supported and tested by
the pinned Turso version. Every terminal invocation has exactly eight manifest
rows. A terminal invocation missing any row is an integrity error and is
reconciled during recovery; the server does not infer availability from file
existence.

Every manifest read evaluates expiry against one captured `now_ms`. An
`available` row with `expires_at_ms <= now_ms` is atomically changed to
`expired` before the read returns, and physical purge is scheduled. This lazy
check makes TTL behavior correct even when the periodic GC worker is delayed.

`logical_bytes` is computed when backing data is committed:

- for `log`, it is the sum of the evidence file and raw capture lengths;
- for `test_log`, it is the private snapshot plus redacted evidence length;
- for normalized views, it is the sum of the redacted serialized record byte
  lengths before database insertion; and
- for `summary`, it is the compact serialized summary and metadata size.

These values drive deterministic quota selection. Physical database allocation
is observed separately because deleting rows may free reusable pages without
shrinking the database file immediately.

### Terminal commit point

The runner performs terminal processing in this order:

1. Stop and reap the Bazel child.
2. Finish and flush stdout, stderr, and BEP capture.
3. Parse and reduce BEP and captured text.
4. Persist redacted normalized rows. Large query rows MAY be staged incrementally
   while the invocation is running, but remain inaccessible until terminal
   manifest commit.
5. Compute the compact summary, item/byte counts, and retention plan.
6. Atomically commit terminal invocation state, termination information,
   compact summary, normalized-row visibility, and all eight manifest rows.
7. Purge nonretained view evidence and working evidence.
8. Notify the GC worker and build the model-visible result from the latest
   manifest snapshot.

Backing evidence MUST be durable before the transaction marks its view
`available`. If the terminal transaction fails, the invocation does not expose
a partially committed terminal manifest. Startup recovery transitions the
orphan to `interrupted`, derives an unsuccessful retention plan from whatever
complete evidence exists, and commits a complete manifest.

Immediate physical purge is allowed after the terminal transaction because
the manifest already gates access to nonretained evidence. Purge failure does
not make the evidence inspectable again.

## `bazel.run` result behavior

`available_views` is computed from manifest rows whose current state is
`available`. The list uses the stable `InvocationView` order and contains no
duplicates. It is a snapshot taken as late as practical during response
construction; a later inspection may report that GC expired or evicted a view.

`more_available` is true when at least one available non-summary view contains
detail beyond the bounded run result. It is not inferred solely from summary
truncation or the presence of an inspect hint.

`inspect_hint` is returned only when the hinted manifest row is currently
`available`. If the reducer's preferred view was not retained or was evicted,
the result omits the hint rather than pointing at unavailable evidence.

The same `RunResultBuilder` behavior applies to synchronous calls, legacy task
results, and Tasks extension results. Task execution mode does not change view
selection.

## `bazel.inspect` result behavior

Every per-invocation inspection result gains these fields:

```json
{
  "invocation_id": "019...",
  "view": "log",
  "availability": "expired",
  "availability_reason": "retention_ttl_elapsed",
  "available_until_ms": 1780000000000,
  "items": [],
  "total_count": null,
  "filtered_count": null,
  "next_cursor": null,
  "truncated": false
}
```

For `available`, the existing items, counts, pagination, filtering, redaction,
and byte-budget behavior is unchanged. `available_until_ms` contains the
persisted expiry when one exists.

For `pending`, `not_retained`, `not_applicable`, `expired`, `evicted`, or
`unavailable`:

- `items` is empty;
- counts and cursor are null;
- `truncated` is false;
- the request is a successful MCP tool execution with `isError: false`; and
- inspection does not query or open the backing evidence.

The distinctions are normative:

- `pending`: the invocation is nonterminal and no terminal view decision exists.
- `not_retained`: policy completed reduction but deliberately kept no backing
  evidence for this otherwise meaningful view.
- `not_applicable`: the command or result cannot produce the requested view.
- `expired`: the view was available until its TTL elapsed.
- `evicted`: the view was available but storage-pressure GC removed it early.
- `unavailable`: retained evidence is unexpectedly missing or could not be
  safely reconciled. This condition produces error telemetry and a redacted
  reason.

The manifest check precedes cursor decoding. If a view expires between pages,
the next request returns `expired`, not `invalid_cursor`. If expiry races a read
that already acquired the view's read lease, the read may complete normally;
otherwise it returns the final structured state. A missing file for a manifest
row marked `available` is atomically changed to `unavailable` with reason
`backing_evidence_missing` before returning the structured result.

An invocation ID removed after metadata retention cannot be distinguished from
an ID that never existed without retaining tombstones indefinitely. It
continues to return the existing unknown-invocation domain error. View-level
TTLs are intentionally shorter than metadata retention so normal evidence
expiry remains explainable.

## Purge operations

The store exposes separate protocol-neutral operations:

```text
purge_view_evidence(invocation_id, view, final_state, reason)
purge_working_evidence(invocation_id)
delete_invocation_metadata(invocation_id)
```

### View purge

A view purge uses this crash-safe sequence:

1. Acquire the invocation/view write lease and wait for bounded active read
   leases to finish.
2. In a transaction, change `available` to the final nonavailable state and
   reason. Leave `purged_at_ms` null.
3. For file evidence, atomically rename files to invocation-local `.purging`
   tombstones. For row evidence, delete only rows belonging to that view.
4. Remove tombstone files.
5. Set `purged_at_ms` after all backing evidence has been removed.

Once step 2 commits, new inspections cannot reach the evidence. A crash or
failure after step 2 leaves a nonavailable row with `purged_at_ms IS NULL`;
startup and periodic GC retry physical removal. An outcome-policy purge starts
from `not_retained`; an expiry purge uses `expired`; a quota purge uses
`evicted`.

Stdout and stderr use separate tombstone names but one `log` manifest
transition. Recovery completes both before setting `purged_at_ms`. File modes
remain user-only throughout rename and deletion.

### Working-evidence purge

After terminal manifest commit, the store purges:

- `events.bep` after the reduction attempt, regardless of invocation outcome;
- `artifacts.json` after artifact rows and summary data are committed;
- non-view target-result rows after target counts and bounded summary fields are
  committed; and
- any future intermediate file unless a specification assigns it to a public
  retained view.

Working-evidence purge uses recoverable file tombstones but has no public view
state. Failure is retried and recorded in metrics; it does not advertise a new
view.

### Metadata deletion

Metadata deletion is the final invocation-wide operation. It is permitted only
when:

- the invocation is terminal;
- metadata retention has elapsed;
- no unexpired deferred task protects the compact result;
- no view is still available; and
- no read or purge lease is active.

It reuses the existing recoverable whole-directory tombstone and removes the
invocation row, compact summary, manifest, normalized rows, and remaining
files. Foreign-key cascade may remove manifest and deferred rows only after
their independent protections have been checked by application logic.

## Garbage collection

### Lifecycle

GC runs:

1. after startup migration and orphan recovery, before accepting MCP requests;
2. after every terminal transition through a nonblocking notification; and
3. on a periodic interval, defaulting to five minutes.

At most one GC pass runs at a time. Multiple terminal notifications coalesce.
The server owns the background worker and shutdown cancellation; the store owns
selection, leases, transactions, and physical purge. A terminal result does not
wait for a complete global GC pass, although its immediate outcome-policy purge
is scheduled promptly.

GC failures are written to stderr and metrics with redacted context. They do
not change the Bazel outcome or turn a completed `bazel.run` into an MCP error.
The next notification or interval retries them.

### Pass ordering

Each pass uses one persisted `now_ms` and performs:

1. complete previously interrupted purges;
2. purge available views whose `expires_at_ms <= now_ms` as `expired`;
3. purge working evidence left by terminal processing;
4. enforce the retained-evidence quota by eviction;
5. delete terminal invocation metadata whose effective retention elapsed; and
6. publish a bounded GC report to metrics and tracing.

Expiry is applied before quota selection so naturally expired evidence is not
reported as quota-evicted.

### Quota ordering

If logical retained bytes exceed `maximum_retained_evidence_bytes`, available
views are evicted in this order:

1. successful follow-up views other than `summary`, oldest expiry first;
2. unsuccessful `log` views, oldest expiry first;
3. unsuccessful normalized follow-up views, oldest expiry first.

Ties use `available_at_ms`, invocation ID, and stable view order. `summary` is
not quota-evicted; it remains governed by metadata retention and task
protection. Nonterminal capture is not eligible. GC continues until logical
retained evidence is at or below the configured high-water mark or no eligible
view remains.

The store returns a `GcReport` containing examined views, expired views,
evicted views, completed purges, metadata deletions, bytes selected, bytes
remaining, failures, and elapsed time. It contains no payload text or command
arguments.

### Disk-limit semantics

`maximum_retained_evidence_bytes` is not a hard physical filesystem bound:

- a running invocation can produce more capture data than the limit;
- running evidence cannot be evicted without weakening reduction or cancelling
  the invocation;
- Turso may reuse freed pages without immediately shrinking its database file;
  and
- a periodic worker reacts after bytes have already been written.

The server exposes physical cache usage and over-quota logical bytes as
telemetry and emits a redacted warning when it cannot reach the high-water
mark. Documentation MUST NOT describe this setting as a hard 10 GiB disk cap.

A future hard-cap design requires a separate specification defining a
per-invocation capture limit and the behavior at that limit, such as bounded
ring capture, loss markers, invocation rejection, or cancellation. This
specification does not silently truncate raw capture needed by reduction.

## Concurrency and recovery

- An available view read holds a bounded read lease from manifest check through
  page construction.
- Purge waits for existing read leases, then changes the manifest before
  removing evidence. It does not wait indefinitely; failure is retried.
- Different views of one invocation may be read concurrently and may be purged
  independently when their backing stores do not overlap.
- The two log files share one lease because they form one view.
- Metadata deletion requires the invocation-wide write lease.
- Recovery never marks a view available merely because a file or row exists.
- A terminal invocation with an incomplete manifest is reconciled to the most
  conservative state: summary when valid, otherwise `unavailable`; existing
  non-summary evidence is retained only if policy can prove its backing data is
  complete and its TTL has not elapsed.
- Orphaned running invocations transition to `interrupted`, receive the
  unsuccessful retention plan, and then undergo ordinary purge and GC.
- `.purging` and whole-invocation `.deleting` tombstones are idempotently
  completed before the startup GC pass.

## Security and privacy

- Cache directories and files retain specification 001's user-only modes.
- Raw capture is never inserted into Turso. Only redacted, bounded normalized
  text is inserted.
- Evidence selected for `log` remains unredacted at rest in its private capture
  files and is redacted before inspection output.
- BEP is treated as potentially sensitive raw evidence and is unlinked after
  reduction rather than retained for operator convenience.
- Availability reasons are enumerated and contain no filesystem paths, command
  arguments, source text, or raw error strings.
- Purge and GC tracing contains invocation IDs, enum values, byte counts, and
  durations only.
- Filesystem unlink is application-level deletion, not guaranteed secure
  erasure. Deployments needing media-level guarantees use encrypted storage and
  manage snapshots and backups accordingly.

## Observability

Add counters and histograms for:

- aggregate process-output capture bytes and BEP capture bytes;
- logical retained bytes by view and outcome class;
- purged bytes by evidence kind and reason;
- view decisions by availability, view, and outcome class;
- GC pass duration, views examined, views expired, views evicted, and failures;
- logical bytes over the high-water mark;
- physical cache bytes as an observational gauge;
- incomplete purge retries and backing-evidence integrity failures; and
- inspections returning each structured availability value.

Metrics MUST NOT contain workspace paths, target labels, command arguments,
filter values, cursor contents, raw text, or other unbounded cardinality.

## Implementation changes by crate

### `bazel-mcp-types`

- Add stable `InvocationView`, availability, reason, manifest, retention-plan,
  and GC-report domain types.
- Make `inspect_hint` typed.
- Add deterministic expiry and stable quota-ordering helpers.
- Keep MCP and Turso types out of the crate.

### `bazel-mcp-policy`

- Add validated evidence-retention configuration.
- Implement the pure command/outcome/view policy matrix.
- Test every terminal state, command family, zero-TTL setting, and fallback
  reason.

### `bazel-mcp-store`

- Add migration `0006_invocation_views.sql` after specification 003's migration.
- Add atomic terminal-manifest commit and manifest read operations.
- Gate normalized view pagination on a store-issued read lease.
- Add view, working-evidence, and metadata purge operations.
- Add deterministic GC selection, partial-purge recovery, and quota accounting.
- Keep Turso and filesystem failures below protocol-neutral store errors.

### `bazel-mcp-runner`

- Compute reduction facts and request a typed retention plan.
- Replace string inspect hints with `InvocationView`.
- Commit terminal state and the complete manifest at one visibility point.
- Gate inspection before cursor decoding or evidence access.
- Return structured availability and derive encoding-neutral `log` evidence
  from both streams without exposing stream identity.
- Notify GC after terminal transitions without owning storage policy.

### `bazel-mcp-server`

- Parse and validate the new configuration and compatibility aliases.
- Derive `available_views`, `more_available`, and `inspect_hint` from the
  manifest in the shared `RunResultBuilder`.
- Encode additive inspect availability fields in every result encoding.
- Own startup, periodic, notification, and shutdown lifecycle for one GC worker.
- Keep all GC tracing on stderr and MCP stdout protocol-only.

### `bazel-mcp-benchmark`

- Record retained bytes separately from raw captured bytes and model-visible
  bytes.
- Add successful and unsuccessful retention cases to the token benchmark
  fixtures without counting private evidence as model-visible output.

No MCP task protocol code enters the store, runner, policy, BEP, or reducer
crates. No reducer becomes nondeterministic or filesystem-aware.

## Test strategy

### Policy tests

- Every command family and terminal state produces exactly eight decisions.
- Successful log retention requires a typed deterministic fallback fact.
- Empty normalized views are never advertised.
- Zero successful or unsuccessful TTL produces `not_retained`.
- Expiry calculations saturate and are independent of wall-clock reads inside
  the pure policy function.
- Stable quota ordering is invariant under input order.

### Store integration tests

- Migration 0006 upgrades databases containing migrations 0001 through 0005.
- Terminal state and all eight manifest rows become visible atomically.
- An available manifest row always has complete backing evidence.
- Each normalized view purge deletes only its own rows.
- Log purge removes both raw streams and normalized log evidence while leaving
  summary metadata and failed-test snapshots intact.
- Test-log purge removes both its private raw snapshot and redacted evidence.
- BEP and duplicate intermediate files are purged after terminal reduction.
- `expired`, `evicted`, and `not_retained` survive restart.
- A crash after the logical manifest transition but before file deletion is
  recovered idempotently.
- A crash after tombstone rename is recovered idempotently.
- Metadata deletion waits for view expiry and deferred-task protection.
- Expiry precedes quota eviction and uses stable tie-breakers.
- Nonterminal invocations are never selected.
- Freed Turso pages do not cause quota selection to loop or delete protected
  summaries.

### Runner and server tests

- A successful build discards raw logs and BEP and advertises only retained
  nonempty views.
- A successful query retains normalized rows but purges raw stdout and stderr.
- A successful truncated informational command retains `log` for the successful
  TTL.
- Failed, cancelled, timed-out, and interrupted invocations retain nonempty log
  evidence for the unsuccessful TTL.
- A spawn failure with empty streams reports `log: not_applicable`.
- `available_views` changes after expiry or eviction and never remains static.
- An inspect hint is omitted when its view is unavailable.
- Expired, evicted, and not-retained inspections are `isError: false` with empty
  items and the specified reason.
- A page cursor used after expiry returns `expired`, not `invalid_cursor`.
- Concurrent inspect and purge returns either a valid bounded page or the final
  structured availability, never a missing-file error.
- An unexpectedly deleted retained file returns `unavailable` and records an
  integrity metric.
- `log` inspection incorporates both nonempty streams but returns only strings,
  with no source identification or selector.
- Failed-test snapshots remain immutable across later invocations and
  `test_log` pagination never reads mutable workspace output paths.
- Text, TOON, structured, and both result encodings have the same logical
  availability fields and byte ceilings.
- Synchronous, legacy-task, and extension-task result builders derive identical
  view lists for the same invocation fixture.

### GC lifecycle tests

- Startup recovery completes before startup GC.
- A terminal notification triggers a pass without waiting for the periodic
  interval.
- Paused Tokio time triggers periodic expiry at the configured interval.
- Concurrent notifications coalesce and no two passes overlap.
- A failed purge is retried and does not alter the invocation outcome.
- Quota eviction follows successful views, unsuccessful logs, then unsuccessful
  normalized views.
- The worker shuts down without writing non-protocol bytes to stdout.

### Property and security tests

- For every generated terminal fixture, no advertised view lacks complete
  backing evidence.
- Every unavailable view is inaccessible even when stale files remain before a
  retry purge.
- Redaction still precedes Turso text insertion, model-visible inspection, and
  telemetry.
- GC and availability errors never include raw payloads, arguments, environment
  values, or secret fixtures.
- Existing model-visible response byte budgets remain enforced.

Fixture and golden changes require reviewed diffs.

## Rollout and compatibility

Implementation is delivered in these stages:

1. Add domain types, pure policy tests, migration 0006, and manifest store APIs
   without changing current retention behavior.
2. Commit truthful manifests at terminal transitions and derive dynamic
   `available_views` while retaining current files as a temporary compatibility
   stage.
3. Add structured inspect availability and gate every inspection path on the
   manifest.
4. Add crash-safe view and working-evidence purges, then enable the new default
   outcome policy.
5. Add startup, terminal-triggered, and periodic GC with deterministic quota
   eviction.
6. Remove deprecated configuration aliases after one compatibility release and
   update benchmarks and operational documentation.

Stages 2 and 3 MUST land before destructive evidence purging is enabled. This
ensures the server never advertises files merely because older code expected
them to exist.

Existing invocation directories created before migration 0006 are reconciled
once at startup:

- nonterminal invocations remain protected and receive a manifest only during
  their terminal or recovery transition;
- terminal invocations younger than their legacy retention deadline receive
  conservative manifest rows based on complete backing data and the new TTLs,
  never an expiry later than their legacy deletion deadline;
- missing or incomplete backing evidence is not advertised; and
- already legacy-expired invocations are deleted by metadata GC.

The README describes the privacy and availability behavior, the difference
between capture and retention, and the fact that the byte setting is not a hard
live-spool disk cap.

## Acceptance criteria

This specification is complete when:

- Specification 001's blanket raw-evidence and BEP-retention language is
  amended as described here.
- Every terminal invocation has exactly eight durable view-manifest rows.
- `bazel.run` advertises only currently available, actually backed views.
- `more_available` and `inspect_hint` agree with the manifest.
- Expected view unavailability returns the specified normal structured result.
- No inspect path touches backing evidence before checking availability.
- Successful stdout and stderr are purged unless `log` is an explicit fallback.
- Unsuccessful nonempty logs receive the configured bounded TTL.
- BEP and non-view working evidence are purged after terminal reduction.
- Evidence purge preserves invocation metadata and unrelated views.
- Metadata deletion respects metadata TTL, terminal state, active leases, and
  deferred-task protection.
- GC runs at startup, after terminal transitions, and periodically without
  overlapping passes.
- Expiry and quota eviction have deterministic ordering and never select live
  invocations or protected summaries.
- Configuration aliases, zero follow-up TTLs, and invalid values have tests.
- Physical cache and logical retained-byte semantics are documented accurately.
- Raw logs and BEP never enter Turso text fields or telemetry.
- MCP stdout remains protocol-only and all GC diagnostics go to stderr.
- The server still exposes exactly `bazel.run`, `bazel.inspect`, and
  `bazel.cancel`.
- `make build`, `make test`, `make check`, `make test-bazel-matrix`,
  `make fuzz-smoke`, and the explicit token benchmark targets pass.
