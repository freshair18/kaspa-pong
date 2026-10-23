# Gameplay pipelines and timing

The gameplay coordinator owns local input intent, pending spends, reservations
and fee authorization. Independent jobs do not mutate those fields directly.

## Concurrent work

- A bounded chain reader fetches accepted-chain batches ahead of validation.
  Its speculative cursor is not spendable state; replay resets discard batches
  from an old reader epoch.
- A separate follower validates transitions in game order, executing covenants
  and reconstructing state with exact accepted input metadata. VM work runs off
  the async worker. Readiness still checks current UTXOs and synchronization.
- Preparation and submission run independently of that follower. The coordinator
  polls completed jobs, rechecks exact covenant inputs and fee parents, and
  authorizes only non-conflicting drafts. An unrelated tip tick does not stale
  an otherwise valid movement. Resolutions/refunds remain exclusive actions.
- One ordered disk writer saves required pending-spend records before submission.
  Diagnostic cache snapshots coalesce to the latest version, while unacknowledged
  transaction/reorg outboxes stay queued. Old snapshots cannot overwrite newer
  required records. Logs drain before clean shutdown; failures remain visible.
- A separate chain clock drives presentation only. It cannot authorize spending.

Publishing a validated follower update merges chain-owned fields into the current
cache; it does not replace newer local reservations or authorized fee totals.
Acceptance updates reach the UI before preparation, disk or submission results
are handled. Reconnects restart the reader/follower from the merged validated
cursor. Ambiguous submission outcomes remain durably reserved for reconciliation.

All three movement lanes, fee-change dependencies, batching, cancellation of
unissued inputs and replay/reorg recovery are retained. Already-in-flight lane
commands accepted after Court hands off to Collector remain tracked and are
included using their actual latest successors at refund time.

## RPC choice and remaining bandwidth cost

The pinned Rusty Kaspa API supports accepted-ID notifications and lower V2
verbosity levels. They are not a direct substitute for Full replay: exact
historical input acceptance DAA is included only at Full verbosity, and this
revision exposes no general historical accepted-transaction-by-ID lookup with
that metadata. A block-body lookup alone does not recover historical spent-input
UTXOs. The relevant pinned sources are rpc/core/src/api/rpc.rs and
rpc/core/src/convert/verbosity.rs at revision a41a333b08848f41bf737b72592e463a6011b8ac.

The implemented reader therefore retains Full queries, including initialization,
catch-up and recovery. No validation is removed and no ID-only observation becomes
spendable state. A lower-verbosity prefilter is possible future work, not an
implemented bandwidth improvement or a demonstrated impossibility.

## Reading timing diagnostics

Each state cache has an adjacent *.timing.jsonl file. Records contain a phase,
local observed_unix_ms, an optional transaction ID/elapsed_ms and acceptance DAA.
Phases cover keypress, batch_selected, preparation_complete,
required_persistence_complete, submit_request, submit_acknowledged,
acceptance_data_arrival, validation_complete, ui_published and cache_persisted.

- Keypress time derives from the frontend's monotonic press instant; batch
  selection records waiting time separately from transaction preparation.
- Submit acknowledgment means the RPC node acknowledged the request, not that
  the transaction was accepted on chain.
- Acceptance-data arrival is when the client received its accepted-chain batch.
  Arrival-to-validation includes queueing, local validation and readiness checks.
  UI publication is a separate event after the validated result is merged.
- accepted_daa is the actual consensus acceptance DAA. It is not a wall-clock
  timestamp. node_acceptance_unix_ms remains null because the pinned RPC does
  not provide the node's actual acceptance instant. A block header timestamp
  must not be substituted for it.

Submission-to-observed-acceptance therefore includes network/consensus time and
client observation lag. These logs separate client-side phases but cannot fully
isolate actual node acceptance latency without additional node instrumentation.
There is no measured public-network before/after latency claim. Full-RPC transfer,
ordered validation, network round trips and consensus acceptance remain limits.

## Post-wager audit

Committed versions 570ab48 and 73823a3 retain identical pipeline.rs and rpc.rs
blobs: fetch-ahead, ordered independent validation, asynchronous preparation/
submission and the persistence writer were not reverted. UI publication still
precedes handling spend completions. One active spend-work job, three movement
lanes and exclusive Court actions remain deliberate constraints.

Public matches d12ef317aa2e (eight DAA/tick) and 98389a731aee (six DAA/tick)
are not controlled performance comparisons: endpoints, command patterns and
outcomes differ. Matched movement median preparation stayed about 45-46 ms.
On the newer match's right client, submission-to-data-arrival was about 1,440 ms
and arrival-to-validation 592 ms, versus about 1,078 ms and 260 ms in the older
match. These are client-observed intervals, not actual node-acceptance latency.
The next logged UI publication followed validation within 21 ms. Input queueing
occasionally exceeded six seconds; without scheduler wait-reason diagnostics,
the logs cannot assign those waits to a particular cause.

Six rather than eight DAA leaves 25% less nominal time per ball step. The next
fixed pace is seven DAA. This changes ball pace, not network transaction latency.

The audit also found an avoidable serial lookup for separate-key wallets.
Gameplay now returns an available payer coin without querying the optional
player address; it falls back only when needed. Existing pending-change funding,
maturity/reservation filtering and actual-owner signing are unchanged. Launch
still considers all candidates for storage mass, and guest candidate VM work is
offloaded from the async runtime. These remove specific unnecessary work; no
public-network improvement is claimed without a controlled measurement.

## Adaptive visual playback

Version 19 replaces the fixed twenty-DAA visual reserve with an adaptive five
to fifteen DAA reserve. It starts at five, raises on advancing-observation gaps,
and lowers gradually after observations stabilize. Playback remains monotonic
within an epoch, bounded by observed time, with up to 2x catch-up. Rebuilding
reserve uses 0.8x playback rather than a rewind. This reduces intentional
presentation delay in deterministic checks, not actual transaction acceptance
latency. Late independently validated movement can still correct a displayed
trajectory; the red local target remains immediate. Goal notices have a separate
three-second visible lifetime and do not hold up serves or transaction work.

## Input collection experiment

Version 16 extends first-press movement collection from 100 to 250 ms. This
deliberately adds up to 150 ms of collection time in exchange for grouping more
same-direction presses per transaction. Repeats never restart the timer. Full
27-step batches may send early; issued retries retain their group without an
extra wait. Local previews/cancellation and Court proof priority are unchanged.
A long hold can span windows, and terminal autorepeat may begin after 250 ms;
this is not a guarantee of one transaction per physical hold. No measured
network-latency or fee improvement is claimed for this experiment.
