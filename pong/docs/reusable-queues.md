# Reusable queues

Branch `queus_reuse`, based on `broadcast_opt`. Host and Join now use the v28
covenant and the exact previous compact code encoder: host key (32 bytes),
fresh tag/settings (8 bytes), and checksum (4 bytes), about 60 characters. The existing tag version bits identify the new protocol;
no network or queue metadata is carried in the code.
Development and verification used synthetic keys and the pinned SDK VM; no live
transactions were submitted. Real-network testing remains to be done.

## Client flow

The wallet keeps a durable, public journal in
`games/queues-v28-NETWORK-PUBLICKEY.json`. Keep this file with wallet recovery
metadata; it contains queue identities, generation/state data, offers, games,
chain undo history and exact signed authorizations, but no private keys.

Host/Join reuse an available set of three native covenant UTXOs. The first set
is created only when needed and uses the invitation auto-approval setting:
5 seconds by default, 0 for manual approval. The countdown begins after the
complete signed transaction and fee are ready. Additional or alternate sets
always require explicit approval. A pending first creation is remembered across
restarts and cannot trigger another automatically approved creation.

If every set is busy, choose to wait or manually approve an alternate set.
Leaving a wait does not discard an already authorized transaction. New sets
receive fresh identities even if funding is reaccepted after a reorg. A guest's
code identifies the host and agreed terms without carrying queue metadata.
The configured node determines the network. At start the host supplies its idle
queue descriptor in a signed transaction payload. The covenant observes the
selected native pool, verifies all three lanes' ownership, idle status,
generation, backing and successors, and commits the actual pool into Court.
The guest reconstructs and independently verifies the same accepted start.
The covenant requires the exact correct disclosure payload, so a missing or
false proof cannot produce a valid but unreadable start.

Guest reservation atomically leases the guest queues and creates the Lobby.
The host then activates its own queues and starts; start does not spend or
recreate the guest queues. Each set can back only one game at a time.

Settlement preserves both sets, clears commands and increments generations.
Winnings stay in the winner's queues. A losing or timed-out player can initiate
finalization without the opponent's signature or fee coin. The caller supplies
ordinary fee funding; keep ordinary wallet coins available for gameplay fees.
A queue buffer is backing and storage-efficient residual capital, not a fee pot.

A rematch uses the same network, players, speed, balls, score and wager, plus a
fresh code. The host offers its reset set after accepted collection. The guest
pastes the new code; unchanged conditions proceed automatically without another
join approval. If another set must be created, its manual-approval rule still
applies. Changed conditions require a new ordinary Join review.

## Wallet controls

Wallet offers Reusable queues, Consolidate wallet coins, and Game status / fee budget.
Queue controls list balances/status and allow manual creation, top-up,
withdrawal keeping the protected buffer, and closing an idle set. Closing returns
its balance less fees to the owner's ordinary wallet. An unused host code can
also be cancelled here, including after a restart.

Game status shows automatic recovery progress and allows explicitly extending
its fee allowance. Timeout and settlement are automatic; there is no manual
timeout action. A live game must reach a valid result or timeout before its
queues can be freed. Recovery continues across menus and other games.

Creation, top-up and starts select multiple mature ordinary inputs directly.
They never automatically consolidate the wallet. Manual consolidation combines
available ordinary coins by default. Explicit inclusion closes eligible idle
sets (including previous-contract sets) under exact-fee manual approval, then
merges ordinary outputs when useful. Zero/one ordinary output is normal, not
an error. Offered, active, reserved and uncertain inputs remain excluded.

The changed start contract changes queue script commitments. Previous v27
journals stay intact; idle sets with no pending old spend or open offer are visible as
previous-contract sets and can be explicitly closed or included in manual
consolidation using the preserved v27 artifact. Active, offered or pending old sets need
the previous client to finish/reconcile. Old pending inputs remain reserved;
new games use the separate v28 journal and new sets. There is no silent upgrade
or automatic closing of previous sets.

## Accounting and reserve

For wager W and pre-game capital B, each player moves W/2 into the game
covenant and keeps the remainder in the existing queues. The game holds W in
total. On collection the winner receives that W plus the loser's remaining
W/2 exposure. Ignoring separately paid network fees:

- Winner: B + W.
- Loser: B - W.

There is no additional control UTXO. The contract protects at least 2 KAS per
set. The client calculates an additional storage-conscious requirement in
quarter-KAS increments: the six reset queues' worst-loss storage mass must stay
at or below 150,000, assuming no input credit. At the current pinned parameters
the calculated buffer is 3 KAS for a 5-KAS wager and 2.5 KAS for 254 KAS.
It is recoverable by closing an idle set, and shares the existing outputs.
Funding an older set uses the greater of its stored reserve and this calculation.

Ignoring input credit is deliberate: a large ordinary fee input can raise the
arithmetic mean input value and remove most queue-input storage credit. Exact
signed candidates still receive full mass checks, including ordinary change.
A small change output can dominate storage mass despite an adequate queue buffer.
An over-limit candidate is skipped during input selection rather than accepted
on a compute-only estimate.

Compute, transient, storage and gas limits are checked. Priority fee pricing
uses full normalized mass; relay-floor pricing uses non-contextual normalized
mass. A fee cap can lower requested priority but cannot undercut the relay
minimum. Reviews show the actual fee and whether requested priority was capped.
Withdrawals and closing can pay fees from the returned balance without another
ordinary fee input.

## Acceptance and recovery

An RPC acknowledgement never makes a set available. Exact signed authorizations
and their source claims are saved before submission and retried unchanged after
uncertain results. Canonical accepted spends prove success or conflict. Undo
restores stages, queue leases, pending receipts and paid-fee accounting; local
cancellation choices and fee allowances persist. Reaccepted inputs use their
actual canonical DAA after VM validation.

Cold recovery can recognize exact authorized wallet successors and native idle
resets from public UTXOs. It cannot infer acceptance or availability from an
absent output or elapsed time. A verified reset can free this wallet's queues
without the opponent's data or signature, but does not invent a missing score.

An unfinished game whose source history was pruned needs a node retaining that
public chain history to reconstruct arbitrary movement states. It is labelled
as needing recovery. Its origin replay runs separately from normal following;
other available sets and manually approved alternates remain usable. No
opponent cooperation is required. Changing the configured node to one retaining
that history and restarting permits automatic replay from the saved anchor.

## Offline measurements and limits

Synthetic lifecycle tests price every signed transaction, including the
reservation payload, at 5 and 254 KAS. One fixture using large ordinary fee
inputs measured:

| Transaction | Storage at 5 KAS | Full normalized mass |
|---|---:|---:|
| Initial set creation | 44,998 | 44,998 |
| Guest reservation | 81,413 | 81,413 |
| Host start | 73,383 | 149,724 |
| Movement | 21,813 | 23,558 |
| Contact resolution | 73,383 | 83,838 |
| Timeout finalization | 7,995 | 65,234 |
| Queue-preserving collection | 147,481 | 164,478 |

Amounts, change, scripts and state affect these figures. They are not current
network fee estimates. Prior v26 starts measured roughly 245,000–248,000 full
mass, so this fixture reduces start mass, but collection and primary-lane
witness overhead remain substantial. Savings across a complete game have not
been established. First creation is a separate, reusable setup cost.

Focused tests cover fragmented funding, identity-preserving owner operations,
scoring, role reversal, cancellation, malicious start attachments, loser-driven
timeouts, journal restart, uncertain submissions, conflicts, reorg rollback,
positive cold-reset recovery, rematch condition checks and wallet navigation.
The live network, real connectivity and two-terminal experience still need the
user's real-world testing.
