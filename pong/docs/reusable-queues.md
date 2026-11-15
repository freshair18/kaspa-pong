# Reusable queues

Host and Join on the mass-optimization branch use the v34
covenant and the exact previous compact code encoder: host key (32 bytes),
fresh tag/settings (8 bytes), and checksum (4 bytes), about 60 characters. The existing tag version bits identify the new protocol;
no network or queue metadata is carried in the code.
Development and verification used synthetic keys and the pinned SDK VM; no live
transactions were submitted. Real-network testing remains to be done.

## Client flow

The wallet keeps a durable, public journal in
`games/queues-v34-NETWORK-PUBLICKEY.json`. Keep this file with wallet recovery
metadata; it contains queue identities, generation/state data, offers, games,
chain undo history and exact signed authorizations, but no private keys.

Host/Join reuse an available set of three native covenant UTXOs. When no
non-closed set or uncertain creation remains, creation (including through Wallet)
uses the invitation auto-approval setting:
5 seconds by default, 0 for manual approval. The countdown begins after the
complete signed transaction and fee are ready. Additional or alternate sets
always require explicit approval. A pending first creation is remembered across
restarts and cannot trigger another automatically approved creation. Closed
historical sets do not prevent auto-approval.

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
finalization without the opponent's signature or fee coin. The caller uses
ordinary wallet funding for finalization and collection.

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

All three active queues use the same Lane script. Each lane authorizes its
matching replacement output, and the game contract validates the complete queue
transition. Court validates contact updates, Collector validates settlement,
and Lobby validates cancellation. The idle first lane still uses Bank.

Previous v27 through v33 journals and artifacts remain available. Idle sets without
an open offer or uncertain spend can be closed or included in manual
consolidation. Use the previous client to finish active games and reconcile
pending spends. New games use the v34 journal and require v34 sets.

The response timeout is 110 DAA, about 11 seconds at the target block rate.
An unresolved contact gets a visible status after 60 DAA (about six seconds).
The court continues animating from the local projection. The client keeps new
paddle inputs local once the opponent's proof is 20 DAA overdue.
Pending inputs become eligible to send when the next contact can proceed. If
the timeout wins on chain, the client clears those inputs and settles as usual.

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
mass. Movement retains the prior 0.1-KAS ceiling; other game operations retain their
0.5-KAS ceiling. Paid and pending game fees stay above the live court. Completion
fees exclude the separately reviewed queue-creation fee and recoverable buffer.
A fee cap can lower requested priority but cannot undercut the relay
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

The v32 synthetic lifecycle fixtures priced every signed transaction, including the
reservation payload, at 5 and 254 KAS. One fixture using large ordinary fee
inputs measured:

| Transaction | Storage at 5 KAS | Full normalized mass |
|---|---:|---:|
| Initial set creation | 44,998 | 44,998 |
| Guest reservation | 81,413 | 81,413 |
| Host start | 73,383 | 92,564 |
| Movement | 21,813 | 21,813 |
| Contact resolution | 73,383 | 73,383 |
| Timeout finalization | 7,995 | 51,118 |
| Queue-preserving collection | 147,481 | 147,481 |

Amounts, change, scripts and state affect these figures. At the same 5 KAS
fixture, v28 measured 149,724 full mass for Start, 83,838 for a contact and
164,478 for collection. The new common lane template is 2,366 bytes. v28 had a
11,297-byte primary and two 1,812-byte supporting templates. In the 254 KAS
fixture, where movement storage mass is only 921, primary movement full mass
fell from 23,558 to 5,778. The 5 KAS fixture remains limited by storage mass.

These are consensus-estimated serialized bytes, calculated from raw
transient mass for the 5 KAS fixture.

| Transaction | v30 bytes | v31 bytes | v32 bytes |
|---|---:|---:|---:|
| Guest reservation | 28,974 | 28,558 | 28,612 |
| Host start | 53,137 | 52,051 | 46,282 |
| Cancel reservation | 45,074 | 44,520 | 31,026 |
| Movement | 2,848 | 2,848 | 2,863 |
| Contact resolution | 33,230 | 32,560 | 26,910 |
| Timeout finalization | 31,951 | 31,240 | 25,559 |
| Queue-preserving collection | 64,975 | 64,520 | 37,494 |

Each Court contact updates the checkpoint for the one ball involved. An
untouched ball's opening flight is derived from the accepted start and its
launch phase. Cancel and collection carry one Bank template in the
transaction payload. Each input that creates a Bank output still verifies
the disclosed template against its own committed template hash.

During start and collection, the wallet compares single coins from the existing
funding snapshot against the usual descending-value funding selection. It prices up
to three alternate signed transactions using full mass. This can avoid
storage-expensive small change without an additional wallet RPC. Collection
remains storage-mass limited in this fixture.

The queue reserve defaults to 5 KAS per player, in addition to the wager.
Choose **Queue reserve target** under **Settings → Advanced** to change it.
The client saves `queue_reserve_target_kas` in `node.json` or the file
specified by `--config` and applies it immediately.
New sets start at the target. Existing sets can join or start with the wager
and their protected floor, without topping up to the target.

The protected floor is 3 KAS for wagers of 5–8 KAS, 2.75 KAS for 9–28 KAS,
and 2.5 KAS for 29–254 KAS. A set's higher on-chain floor still applies.
Moves and contacts can pay fees out of the reserve above that floor. When
wallet funding is needed, the same transaction pays the fee and refills the
lanes it spends toward the target, as far as the chosen inputs allow.
Invitation cancellation can spend up to 0.2 KAS above the guest's floor.
Timeout, score finalization and collection use wallet funds for their fees.

The client again reuses exact submitted ordinary fee change, after proving the
parent in the node's non-orphan mempool or the output in the accepted UTXO set.
Authorization runs outside the display/input loop. Physics, the 250-ms input
batching window and 33-ms rendering interval are unchanged. Movements may
spend the exact queue replacements of a submitted contact
resolution. The client executes and interprets that parent against canonical
sources, checks ownership and queue state, and requires positive node mempool
or accepted-output proof. Unknown residual event times prevent pipelining.
These planning outputs never replace accepted state in the renderer. Parent
conflicts invalidate dependent movements and reorgs restore their reservations.

Focused tests cover fragmented funding, identity-preserving owner operations,
scoring, role reversal, cancellation, malicious start attachments, loser-driven
timeouts, journal restart, uncertain submissions, conflicts, reorg rollback,
positive cold-reset recovery, rematch condition checks and wallet navigation.
The live network, real connectivity and two-terminal experience still need the
user's real-world testing.

Queue availability checks show the synchronization status and elapsed time,
returning to the menu with an explanation after 30 seconds while background
recovery continues. Idle wallets without open host codes verify current outputs
before following recent chain changes; they do not replay old games before
offering queues. Open codes still need inbound reservation history. Missing
headers/pruned cursors and repeated history timeouts trigger anchored UTXO
reconciliation. Unfinished games and uncertain signed inputs remain reserved
until positive chain evidence proves their state.

## Rebuilding the v34 artifact

The client embeds `artifacts/reusable/artifact.json`. Building or running
the client uses the pinned Argent runtime and the checked-in artifact.
Regenerating it requires the compiler patch in
`tools/argent-shared-bank.patch`. The patch applies only to the three
ReusablePong entries that read the Bank template from the payload.

With Pong's Cargo dependencies already cached, use a clean Argent checkout
at `9a9f4b107116d0b259fae629f200fa3b663e7e9d`. From the Pong directory:

```powershell
.\tools\build-reusable-artifact.ps1 -ArgentSource <path-to-pinned-argent>
```

The wrapper builds in the Cargo target directory and writes the result to
`target/argent/reusable` by default. Compare the reported artifact ID against
the checked-in JSON when reproducing an unchanged contract. Source path
metadata can vary by checkout. The build leaves the checked-in artifact and
the Argent checkout untouched.
