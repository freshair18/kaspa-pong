# Protocol and recovery reference

The source of covenant rules is `contracts/pong.ag`; its comments explain actor
syntax, queue timestamps, bounded physics and output/refund checks. The Rust
projection and builders are in `src/engine.rs`.

## State and timing

- Lobby commits speed (1–15 DAA/step), ball count (1–8), and winning score (1–15). Court keeps
  eight fixed ball slots and the original launch epoch. Only the selected count
  is active. Initial launches and subsequent respawns follow the phases in
  [multiball rules](MULTIBALL.md). Auto goals are a client preference only.
- Lobby starts Court and six independent Player lanes in L0,R0,L1,R1,L2,R2
  order. Each lane holds one operational coin and four batches. Court holds its
  one operational coin plus both stakes. The client fixes each stake at five KAS;
  the covenant carries a bounded parameter for future client/rules changes.
  Lane zero stores its side's checkpoint position and watermark as data, using
  the same lightweight Player template as every other lane.
- Signed movement spends only its lane. The latest timestamp is -1 until a
  subsequent spend stamps the real acceptance DAA from consensus input metadata.
  Every queue re-creation preserves original command acceptance times.
- Court.resolve(side, until, receiver_sig) is one Argent coordinated leader transition.
  Its exact input group is Court and that side's three Player lanes. It folds
  up to four chronological timestamp groups strictly before until, preserving
  all leftovers and applying equal-time heads in lane order. It cannot pass
  the earliest deterministic contact across all active balls.
- A relative sequence lock on Court proves acceptance cannot precede until.
  When the receiving side's prefix reaches the contact deadline, the SAME
  transaction computes the collision, rebound or goal and emits the next ball
  trajectory plus the three queue successors. There is no separate certificate
  transaction followed by a contact transaction. Equal-DAA movement follows
  collision physics and cannot rescue an already missed ball.
- A partial prefix carries its leftover queues and watermark forward without
  advancing the ball past unresolved history. A busy contact may therefore
  need multiple bounded resolves. A non-receiving side can compact safely up
  to the next ball contact without touching the other side's commands.
- Contacts and pressure checkpoints require the selected paddle owner's
  signature in addition to the original signed commands. Only the receiver can
  prove its contact. Court.timeout accepts the opposite player's signature once
  the accepted-progress expiry has elapsed, awarding that
  player the entire match without fabricating a goal or changing the scores.
  Opening contact time uses Court's actual acceptance DAA. Expiry is
  max(contact deadline, accepted response anchor) + 150 DAA. A new flight or
  receiver prefix consuming pre-contact batches marks response_at=-1; its next
  spend reads the actual input acceptance DAA. Other Court replacements carry
  that anchor, not their own birth. Watermark-only updates, nonreceiver work and
  post-contact commands cannot renew it. At most twelve pre-contact batches
  remain once contact is due, and new movements cannot replenish past history.
  This bounds renewals. A late-confirmed outgoing flight also gives its next
  receiver a full 150 DAA before timeout, avoiding instant inherited forfeits.
  150 DAA is fifteen nominal seconds, not an exact wall-clock guarantee. A valid
  late proof and an eligible timeout claim compete for the same Court UTXO;
  whichever is accepted first invalidates the other. Timeout uses the same
  separate Collector handoff as a normal win.
- Early compaction happens at eight queued batches, with movement priority
  while there is room. Quiet tails wait for contact instead of paying extra
  age-triggered checkpoints. All three movement scripts have equal cost;
  lane selection balances queue occupancy and retains three transactions in flight.
- After the winning contact, Court.settle spends only Court and authorizes a
  Collector. Collector.collect then spends the Collector and all six latest
  Player queues. Their six KAS plus Court's four KAS are the entire pot; it pays
  all ten KAS to the winner's player key, including a timeout winner. No extra
  collateral or collateral-return output exists.
  The six-lane collection implementation is absent from Court; only the small
  score/signature authorization gates remain. There are still two ending
  transactions and no extra dispatch transaction per contact. Separate funding
  pays fees. Already-in-flight commands accepted after the handoff are tracked
  and their actual successors are used for collection.
- Geometry is 58x32, with contact planes x=2 and x=55 and center-line serves
  at x=29. Version 16 uses nine-cell paddles centered in 4..27 and a one-cell
  vertical ball radius for paddle overlap. Hosts select 1–15 DAA per movement
  tick, one to eight balls, and a winning score from one to fifteen.
  The opening delay is 25 DAA. A goal's minimum 20-DAA pause extends to the next
  original launch-phase slot. Rebound offsets retain five slopes; PongServe/v3
  varies launch height, side and angle separately for each ball and serve.

Each side still has twelve batch slots. Queue pressure can require intermediate
prefix checkpoints, and every paddle-plane crossing is explicit, including flat
rallies. A projection is provisional until contract transitions are accepted.

## Client guarantees

Gameplay separates chain fetching, ordered independent validation, preparation/
submission and diagnostic persistence. Only validated coherent state authorizes
spends; pending-spend durability still precedes submission. See
[pipelines and timing](RESPONSIVENESS.md) for reservation/merge rules, Full-RPC
limitations and the distinction between observed and actual node acceptance.

Readiness checks exact game UTXOs, including acceptance metadata, and requires
history within 50 DAA of observed chain time. The global tip need not stop moving.
The node validates sources and covenant timestamps atomically on submission;
the client avoids a redundant preflight RPC for every independent lane. Mempool acknowledgment is not
acceptance. Up to three local movement transactions are saved before submitting
and reconciled individually while the client is running. A lane with a pending spend is reserved.
Resolve still waits for outstanding movements before construction. Once submitted,
its explicit replacement queues allow one movement child per available lane without
waiting for resolve acceptance (at most resolve plus three movement children).
The child's new timestamp stays -1 until its own actual acceptance; another movement
on that lane must wait. This planning view never becomes accepted game state and
cannot authorize a further Court spend. Refunds remain exclusive. Dropping a
transaction also drops its local queue and fee-change descendants. Unaccepted lost
movements are rebuilt after a fresh coherent acceptance check.

Fee funding can use already-submitted P2PK change without waiting for acceptance;
these ordinary inputs do not depend on DAA metadata. Covenant inputs match
accepted metadata exactly, except the verified explicit queue replacements of a
submitted resolve. Their exact outputs may move from pending to accepted while a
child is being prepared. Reserved inputs cannot be reused. Submission
uses fee parents already acknowledged by this node; reconciliation checks the
mempool when needed. Six queue outputs hold one coin apiece; Court holds
eleven KAS (the pot plus one operational coin). Seven operational coins
remain refundable independently of the wager. Actual transactions must fit the
pinned storage-mass limit. The default fee cap is 25,000,000 sompi; existing
explicit smaller caps produce an actionable error when insufficient. Launch
and gameplay fees are calculated from the configured relay rate, within caps.

The host locks twelve KAS in Lobby with a compact creation manifest in that same
transaction's payload. Its player signature binds the network/rules version,
participants, refund/funding keys, amounts, fee, anchor and specific funding
outpoint, without signing the containing transaction ID. Creation's ordinary
funding signature separately commits the transaction. Clients verify both and
the exact pinned Lobby output; the covenant itself governs the locked funds.
Lobby.start requires the invited guest's ALL signature and enforces the
full seven-output game split. The guest adds five KAS and external fees, selects
among its actual mature coins by SDK storage mass, and returns change to its
funding address. Completed joins validate canonical outputs, source metadata,
signatures, storage commitments and covenant execution before submission.
No host signature package is exchanged. Host cannot preflight an unknown guest
coin before locking Lobby; invalid guest funding is rejected before authorization.
Pricing converges from a small trial fee: trialing the cap could manufacture tiny
change and incorrectly reject a valid transaction.
Fee caps still apply. Other versions, amounts, paces and already-started games
are rejected. See [funding, payouts and cancellation](WAGERING.md).

The frontend runs independently of RPC/transaction work. Playback defaults to two
DAA behind observed time; the user can select a base from zero to fifteen DAA.
Advancing-observation gaps raise the reserve to
base + ceil(max(gap_ms - 100, 0) / 100), capped at fifteen DAA. Lower requirements
reduce it one DAA every two seconds; duplicate targets do not change adaptation.
Playback rebuilds a depleted reserve at 0.8x without rewinding, and catches up
at up to twenty DAA/second (2x), tapering toward normal speed near the reserve.
RPC bursts do not teleport the ball. Playback never
passes observed time. Old Court checkpoints remain available until playback
reaches their replacements. Accepted future queue commands apply at their actual
DAA. White collision paddles use the same historical projection as the ball.
Gray markers one column outward show latest accepted positions when different;
the red pending target sits one column inward. The yellow ball uses 2x4 Braille
dot positions per character, with a visible 4x4-dot sprite on a 16-row court.
The visual court follows terminal width (32..128 columns); the physical field
remains 58x32. Fractional DAA playback drives subcell interpolation without
passing observed chain time. Version 18 uses spatial, not temporal, sub-dot
coverage: the opaque sprite retains sixteen distinct dots while its centroid
moves in quarter-dot increments. Vertical interpolation reflects at walls rather
than cutting across reflected endpoints;
collision/scoring physics remain covenant rules. Brief catch-up/disconnect gaps
retain inputs locally without expiry or a twenty-key local cap; signing waits
for coherent state. Snapshots older than ten seconds stop new clock samples,
but controls on an already live court remain queued. Missing/conflicting movement
transactions and their dropped fee descendants are rebuilt only after another
coherent sync checks for acceptance. Accepted and still-pending moves are not
duplicated. Game rollbacks reset playback clocks while retaining pending intent.
Input retention lasts for the running match; quitting or completion ends it.
Once the receiver's contact is due, new movement authorization/selection pauses
while existing commands are confirmed, so new inputs cannot starve its proof.
Unissued intent remains queued; accepted metadata/reservations are still required.
Pending fee change is preferred as before. With separate keys, gameplay uses a
usable payer-address coin immediately and queries the player address only if
needed. Launch retains exhaustive storage-mass candidate choice.

The guest retains the fully signed start until the lobby is spent, allowing a
dropped attempt to be resubmitted. Its mempool lookup uses the actual guest
funding owner. Start failures stay visible instead of falling back to
an indefinite signature-wait message. This is in-process retry, not saved-match resume.

The tracker retains 2048 selected-chain undo entries, replaying deeper history
from the invitation anchor. Catch-up evidence survives restarts and is evaluated
when coherent: only lost game transactions or changed acceptance DAAs cause new
reorg notices/logs. Same-DAA reacceptance is silent. Transaction logs preserve net
acceptance/reversion changes; durable outboxes recover interrupted writes.
Routine notices use a quiet activity line. Three distinct lost/retimed game
transactions, or a game acceptance shift of at least twenty DAA, trigger stronger
wording; global chain depth alone does not. All notices expire two seconds after
first display, even when snapshots repeat, without requiring acknowledgment.
Detailed logs and rollback handling are unaffected.

Fees use signed transaction mass and the configured relay rate, subject to
per-transaction and cumulative authorization caps. The accepted fee ledger
attributes setup/gameplay/refund costs to their payer. Pending fees and refundable
deposits are shown separately. Caches must have the current required fields; no historical fee-history migration runs.

## Current-game inspection

Only the current protocol and artifact are supported. Older game versions are
rejected before signing; create a fresh match. There is no runtime compatibility
layer, legacy artifact set or cache upgrade path. The alpha tag is historical
source, not a supported alternate client. Wallets and saved game files remain
user-owned and are not deleted by a rules update.

For current-game inspection or explicit history rebuild:

```powershell
.\pong --config node.json run --watch-only --once --invitation OFFER_INVITATION --state observer-state.json
```

The `run` interface takes the local public Invitation diagnostic, not a game
code. Normal play uses Host/Join and exchanges only a code. `run --rescan` rebuilds
current history while preserving pending-spend and authorization accounting.
File locking prevents simultaneous signers against one state file.

Current artifact: `artifacts/artifact.json`, ID
`fe67363b9aabedd89083ab2f24d973d54ca52d7d735d04f4891dfbf380d0d55d`.
The client checks this identity as well as the invitation's version and ID,
so an unrelated covenant cannot be interpreted with these physics.

Host/Join menu choices use immediate 1/2 hotkeys. Guest enters a checksummed
game code; an explicitly configured read-only invitation directory retrieves
the public creation transaction. No local file discovery or file transfer is
required. Clients reject wrong network, rules, participant, altered signatures
and ambiguous codes, then check the exact accepted unspent Lobby through their
own node. See [directory deployment and coverage](ONCHAIN_INVITATIONS.md).
q/Esc returns from setup to the
main menu; only Escape returns there during gameplay. Gameplay q is ignored;
main-menu q exits. Machine-readable
and bounded developer commands retain their noninteractive exit behavior.
Every interactive match owns its runtime; background work, terminal readers,
writer durability and state locks are shut down before another session starts.
Setup finishes any current operation/submission before leaving. Returning to a
menu is not cancellation, refund or saved-match resume.

Player Resume and session aliases are retired. Launch creates public
invitation.json and diagnostic state, without saving a player session or an
external offer/signature-exchange file.
Host/Join does not reopen an existing player cache. Existing files are not
migrated or deleted. The developer run interface remains for explicit inspection
and local test tooling; it is not presented as a player rejoining flow.

Client input optimization: opposite movements in the waiting sequence can cancel
while both are still unissued; individually clipped no-op presses can also be
removed. The complete clipped target, not merely the arithmetic sum, is preserved.
Final input capture, cancellation and commitment share the frontend enqueue lock,
closing the old drain-to-authorization race. Current intent is rechecked after
preparation and before network handoff. A fresh authorization carries a runtime
proof that it has never been submitted; that proof is not reconstructed from a
timestamp or saved cache. Known-never-submitted drafts can be withdrawn during
required persistence/Ready. A durable withdrawal marker must be acknowledged
before releasing reservations or refunding their local authorization budget.
Recovery applies persisted withdrawal markers before any resubmission; other
recovered, previously exposed or uncertain pending spends remain protected.
Issued commands and retries retain their original grouping and are never cancelled.

Batched movement (game version 9): Player.command accepts signed displacement
-27..-1 or +1..+27. One batch uses one existing slot/output and moves its paddle
by that many cells, clamped to 4..27, at the new transaction's acceptance DAA.
Earlier appends keep their original times. Court scratch encoding uses four
base-64 digits (steps+27), retaining full signed magnitudes in leftovers.
The client holds a new unissued burst for 250 ms from its first press. That is
an initial readiness window, not a membership cutoff: later same-direction keys
may extend/rebuild a still-unsent draft up to 27 steps. Repeats do not reset the
timer; 27-step full batches may send early. Previously issued retries retain their
original group and incur no new collection delay. The readiness check never
sleeps or gates Court actions: an unready movement batch passes no movement
request to the scheduler, which can still resolve a due contact. Local target
previews and unsent-opposite cancellation remain immediate. Each original key maps
to the same transaction ID for acknowledgment and reorg recovery. A dropped batch
retries as its original group; new keys cannot join that previously issued group.
Long holds may span several batches, and terminal autorepeat can begin after the
window. This bounded collection policy is not a key-release debounce.
Three lanes per player retain independent submission. Atomic prefix/contact resolves consume
Court and one side's three lanes; settlement/timeout authorizes a separate collector,
whose payout consumes all six lanes. There is no live-game cancellation. The
seven actors together hold the ten-KAS pot, with no additional collateral.
New artifact/version require fresh matches. No migrations or extra batch outputs.

Visual impact cues observe projected frames without changing the simulation.
Paddle contacts flash cyan for 250 ms and wall contacts spark for 180 ms.
Version 17 interpolates the last approach tick even when its endpoint scores a
goal, instead of freezing before that tick. The goal exit carries the incoming
segment's horizontal speed and vertical slope beyond the paddle plane, using
the same interior screen mapping rather than a fixed-duration horizontal slide.
The waiting ball and direction arrow remain visible after the exit, including
during a goal notice; numeric countdowns are hidden. Effects never delay a serve
or alter consensus state.
Version 18 ties live goal-exit age to continuous playback DAA, including its
fractional part, so catch-up playback cannot slow down at the post or expire
the exit in wall time. At widths/speeds that cannot fully clip the outgoing
sprite by the existing serve deadline, its coverage fades over the final eight
DAA. Final settlement alone continues cosmetic animation in wall time from
the last playback position. Neither policy changes flight or serve timing.
Version 19 separates a three-second wall-clock goal notice from that outgoing
sprite. Score increases observed after a skipped pause still highlight the
scored end; known crossing height is retained, otherwise the end is highlighted.
Initial scored frames are baselines, exact goal identities are deduplicated,
and corrections/reorgs clear contradicted notices. A later distinct goal at the
same score can notify again. Only Escape exits gameplay; q remains a menu/setup
control and movement repeats are unaffected.
Finalized/refunded wins continue
the last visual court with the accepted score while the existing final-board
hold runs. This score override is paint only and never enters game state.

Heading arrows appear beside the ball only while waiting to serve, showing
its launch direction. They are hidden during flight and goal-crossing effects.
Opening delay is 25 DAA in both Argent and Rust. The between-goals pause
remains 20 DAA; ball pace is the 6-, 7-, or 8-DAA interval committed in the Lobby.

Paddle radius is four game cells (length nine); centers stay in 4..27.
Rust projection, Argent collisions, rendered paddles, previews and cancellation
boundaries use these limits. The 4x4-dot ball has a one-cell vertical radius for
paddle collisions: absolute center offset <= paddle radius + ball radius hits.
Outgoing vertical speed still clamps the center offset to -2..2. Wall reflection
coordinates/period and horizontal contact planes/times are unchanged; this is
an edge-overlap correction at the paddles, not a new swept-wall model.
Serve arrow spacing follows sprite
size to keep it visible. The painted ball's horizontal extent maps to the inner
paddle faces: at a hit, its visible edge touches the paddle without overwriting
it, across terminal widths. Outside the posts, the mapping still reaches the
court edges for goal animation. Horizontal sprite placement is presentation only.
Version 16 adds ball-edge paddle overlap and longer paddles to version 15's
accepted-progress timeout freshness and seven-DAA pace, retaining
receiver-owned proofs and wager escrow/payouts. Empty no-progress checkpoints are rejected so they cannot
keep replacing Court solely to invalidate timeout drafts. Fresh matches required.
Version 17 fixes the client stake at five KAS per player and smooths goal
presentation. The covenant artifact is unchanged; it already carries the
bounded stake parameter. Restart clients and use fresh offers/matches.
