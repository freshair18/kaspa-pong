# Independent peer acceptance views

The existing RPC broadcast pool still targets 8 nodes total. This branch reuses
its secondary connections for optional accepted-chain observation; it does not
create additional sockets or use P2P transport.

Run normally to enable it. Add `--single-observer` for the previous observation
behavior while keeping the same broadcast pool. JSON configuration can instead
set `multi_peer_acceptance` to false. A one-node broadcast pool has no secondary
observers.

## What can advance early

Qualified secondary peers maintain separate cursors, bounded fetch queues and
independently replayed game histories. They must support Full accepted-chain
data and the UTXO index, match the network and report synchronization. Every
candidate goes through the existing covenant validation, source-metadata checks
and current-UTXO consistency checks. A submission acknowledgement is never used
as acceptance.

New secondary readers seed only chain state, rewound by up to 20 retained blocks
to provide a recent shared checkpoint. They copy no reservations, authorization
totals, fee ownership or durable outboxes.

A compatible live-game extension can drive the displayed ball and paddles
before the designated observer catches up. It is labelled provisional. The
coordinator's saved Cache, pending transactions, fee ledger, signing inputs,
settlement and fund reservations remain entirely owned by the designated
observer. Consequently this initial version improves early visual observation;
it does not accelerate authorization of dependent transactions.

Locally submitted paddle intent remains retained until primary confirmation.
When a secondary already shows that move, the preview skips applying it again.
If the provisional view disappears, the retained preview returns. A backward or
conflicting presentation transition resets the renderer's buffered timeline.

## Disagreement policy

- A peer that has not seen a transaction is lagging; it does not cast a negative
  vote. Compatible extensions are selected without waiting for a quorum.
- Histories are compared only from a shared chain checkpoint. Current actor
  outpoints and UTXO metadata must agree for equivalent states. Different game
  transaction histories or differing acceptance metadata cause conservative
  fallback. Even benign independent-transaction reordering can cause fallback.
- A conflicting peer is quarantined from provisional selection. The game shows
  the designated observer's view and pauses new game submissions until another
  coherent primary validation advances its cursor. Already exposed submissions
  cannot be recalled and remain reserved.
- Quarantine clears when that peer's history becomes compatible with the
  designated observer. No majority vote, reported peer count or raw DAA score
  determines consensus. Unrelated histories cannot advance the display.
- Candidate views expire after about two seconds. A secondary clock more than
  50 DAA ahead of the primary cannot drive presentation. Primary failure uses
  the existing reconnect path; a secondary is not silently promoted into a
  signing or settlement authority.

## Work and diagnostics

Secondary qualification, RPC reads and validation run in background tasks.
Secondaries poll at the configured interval; the primary retains virtual-chain
notifications. Secondary polling avoids sharing a notification receiver or
leaving extra subscriptions behind on a submission connection. Each follower
retains the existing two-batch fetch and two-update validation bounds.

History comparisons run on fresh updates, primary changes or a 100 ms freshness
check, not every 5 ms game-loop iteration. Secondary failures are logged and
retried after 30 seconds. Workers are cancelled when a peer disappears or the
game exits. Keeping several Full-history readers increases incoming traffic,
memory use and validation work; a gameplay speedup has not been measured yet.

`target/broadcast.jsonl` includes `acceptance_peer_ready`,
`acceptance_peer_transaction`, `acceptance_peer_error`, `acceptance_conflict`
and `acceptance_selection`. Each peer transaction record carries its identity,
accepting block, DAA, local arrival and validation timestamps. Gameplay timing
logs add `provisional_ui_published` for locally pending moves. These diagnostic
records do not release reservations or become the authoritative transaction log.

This retains the existing RPC trust model: independently replaying game rules
does not turn remote node reports into a proof of Kaspa consensus.
