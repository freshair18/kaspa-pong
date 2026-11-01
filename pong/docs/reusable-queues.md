# Reusable queues: implementation status

Branch: `queus_reuse`, based on `broadcast_opt`.

This is an experimental protocol implementation, **not yet activated in the
Host/Join or Wallet menus**. Existing invitations and recovery continue to use
the v26 contract. Do not interpret a successful offline test as a deployed game
upgrade. No live transactions were used to develop this implementation.

## Implemented

- Independent, owner-controlled sets of three queue UTXOs. The covenant identity
  persists across matches; host/guest role is assigned for each new match.
- Idle queue creation, direct top-up, surplus withdrawal and closing. Closing
  can combine ordinary coins into the same owner-approved withdrawal.
- Direct multiple-input funding, without a preliminary consolidation transaction.
- Atomic guest reservation and creation of the invitation covenant. The host
  activates its own queues; start does not recreate the guest's queues again.
- Commands and deterministic contact resolution using the existing game physics.
- Cancellation, normal settlement and timeout settlement. Collection needs no
  signature or fresh funding from the losing player. Both sets return to idle,
  commands reset and generations advance. Winnings remain in the winner's set.
- A public inventory with durable signed creation authorizations, ownership and
  network checks, input reservations, and conservative acceptance reconciliation.
- An accepted-transaction interpreter for the new protocol.
- Full compute/transient/storage mass accounting shared by the existing client,
  including ordinary wallet sends and host starts. Priority fees use full
  normalized mass; the relay floor uses non-contextual normalized mass. A fee cap
  cannot silently undercut the relay floor.

The new contract and artifact are separate from `artifacts/artifact.json`.
Tests use synthetic keys and the pinned SDK's actual VM.

## Accounting

For wager W and pre-match queue balance B, W/2 moves into the game covenant.
The queues retain the rest of the wager plus their reserve and surplus. Across
both players the game covenant therefore holds W, drawn entirely from wagers.
No control UTXO is added.

Collection transfers the loser's remaining W/2 exposure and the game's W into
the winner's queues. Ignoring separately paid network fees:

- Winner: B + W.
- Loser: B - W.

A valid timeout is permissionless: the timed-out player can also finalize it
and free the queues when the winner is offline. The contract fixes the winner
and payout; the caller cannot redirect them.

Ordinary coins currently pay fees. The loser need not sign or supply any coin
for settlement; a claimant supplies its own fee funding. A set cannot close
while leased. Release is permitted only when its game covenant terminates.
Active checkpoint delegation specifically requires a Court, so a host cannot
attach forged queue changes to an invitation start.

## Reserve policy

The contract protects at least 2 KAS per set. The client additionally calculates
a storage-conscious capital requirement for each wager. It chooses quarter-KAS
increments so the six reset queues' worst-loss storage mass is at most 150,000,
even with no input storage credit. This is below the currently measured collection
witness mass. For the supported wager range, the calculated buffer is around
2.5–3 KAS. It occupies the existing queues and can be withdrawn by closing an idle
set. It is not a fee.

Ignoring input credit is deliberate: adding a large ordinary fee input can
increase the arithmetic mean input value and remove most of the credit from the
queue inputs. An estimate based only on the covenant inputs understates this
case. Exact signed transactions still receive independent full-mass preflight;
the reserve model does not replace that check. A very small ordinary change
output can still dominate storage mass and must be avoided during selection.

The buffer may exceed the reserve recorded in an older set. Funding that set
for another wager uses the larger of its recorded reserve and the current
calculation, rather than creating another identity.

## Offline measurements

One synthetic 5-KAS fixture, at the same fee priority rate, measured:

| Transaction | Storage mass | Full normalized mass |
|---|---:|---:|
| Host start | 66,228 | 148,768 |
| Contact resolution | 35,373 | 83,838 |
| Timeout authorization | 7,245 | 65,234 |
| Queue-preserving collection | 128,395 | 164,478 |

Values depend on the exact input and output amounts, fees, scripts and game
state. These are not network fee forecasts. Earlier actual v26 starts measured
approximately 245,000–248,000 full mass. The experimental start improves that
fixture, but reusable queue management adds script/witness overhead, particularly
to collection and the primary lane. It is not a claim that every transaction
becomes cheaper. A saving across a complete game has **not** been demonstrated;
script/witness optimization remains necessary before activation. Initial set
creation remains a separate setup transaction.

## Required before activation

1. Connect the new interpreter to the existing chain follower, undo/reorg journal,
   pending-spend pipeline and startup recovery. Inventory reconciliation alone
   cannot discover arbitrary later queue successors or replace chain replay.
2. Extend authenticated game codes/invitation discovery with the host's queue
   identity and generation. Verify accepted guest reservation before signing a
   start, including guest queue changes accepted before start.
3. Connect set selection to Host/Join and rematches. Busy sets must offer wait,
   another available set, or an explicitly approved alternate creation.
4. Implement the initial creation countdown, manual alternate creation, and Wallet
   list/create/top-up/withdraw/close controls. Persist first-creation history even
   if its submission is uncertain. Recheck exact rematch conditions automatically.
5. Switch new games to direct funding and remove automatic consolidation from
   that path. Preserve recovery of existing consolidation authorizations and add
   a separately approved manual consolidation action.
6. Finish fee/reserve previews, capped-priority reporting and session recovery
   tests before making reusable games the default. Back up the public queue
   inventory along with wallet recovery metadata.

The new inventory and interpreter are building blocks for these steps. They are
not currently called by the production game coordinator. Legacy auto-consolidation
therefore remains active in the current menu flow until that switch is completed.
