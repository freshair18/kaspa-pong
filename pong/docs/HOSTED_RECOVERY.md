# Hosted invitation recovery

Leaving a hosted setup durably queues immediate background cancellation of its
unjoined Lobby and wakes the recovery worker. Restarting the client also queues
abandoned current-rule hosted invitations: there is no resume flow. One-hour
local expiry remains a fallback. Cancellation cannot spend a started game;
competing guest join and Lobby refund consume the same output, so only one wins.
Closing the application retains the request for the next run rather than
keeping a hidden signer alive after exit.
`games/hosted-NETWORK-PLAYER.json` stores public invitation paths and creation
times, including custom host directories. Existing current-format invitations
under `games/*/invitation.json` are discovered using file modification time.
There is no on-chain expiry rule, compatibility layer or migration.

Recovery uses the selected player's and fee payer's signer locks. It refunds
only an exact, currently accepted Lobby owned by those keys. `Lobby.cancel`
returns the entire original principal to its committed refund key; a separate
ordinary owned input pays the fee. The ABI dispatch tag, both VM executions,
principal, fee change, acceptance metadata and current fee limits are checked.
Under version 21 that principal is only the host's five-KAS stake. Prior-rule
pending source outpoints remain conservatively unavailable to new funding,
without replaying old game state. Use the preserved previous executable with
its matching artifact to recover an older funded match; see SESSION_HANDOFF.md.
A guest join can win the same-input race; an accepted live game is never
treated as an unjoined Lobby.

The signed cancellation and exact inputs are saved in `left-state.json` before
submission. Unknown outcomes retain the same transaction and reservations;
retries never create a replacement fee spend. Missing Lobby/input UTXOs are not
proof of acceptance. Full independently validated replay reconciles acceptance
and reorgs, preserving existing history and cumulative fee authorization. A cold
accepted-Lobby bootstrap seeds the creation fee ledger without charging principal.
For an exact twice-checked unspent original Lobby with no pending authorization
or observed game transition, recovery now reanchors at the pre-snapshot current
cursor instead of replaying hours of unrelated empty chain history. Counters,
fee ledger and authorization budget remain intact. Pending or advanced states
still require replay; subsequent Full validation starts from that captured cursor.
Recovery is an independent client-lifetime pipeline, not a launch/menu gate.
It scans while the menu and matches run, wakes on cancellation requests, and
otherwise repeats after thirty seconds.
Each invitation uses its own state lock; an active local match is deferred.
Another invitation to the same opponent is allowed using other available coins.

Wallet funding also excludes durable pending inputs from other known sessions.
Public `games/pending-NETWORK-SIGNER.json` indexes exact state paths for both
keys, including custom guest/direct-run paths. Register paths with
`remember_state` before signing. In-memory snapshots belong to each journal;
release follows a durable withdrawal or saved reconciliation, never an RPC
error or timeout. Predicted recovery fee change is not available for funding.
`node::reservations` atomically selects and claims candidate inputs across
hosting, guest approval, gameplay and recovery. Preparation leases release when
discarded before exposure; authorizations transfer to journal-specific claims
until successful persistence. An older disk snapshot cannot release a newer
authorization. The ordered writer publishes durable reservation changes before
acknowledging submission readiness. Guest exhaustive coin selection temporarily
claims its candidate set, then retains only the actual transaction inputs.
Host creation also saves exact source entries in `creation-authorization.json`.
Public genesis inputs remain conservatively reserved, including invitations
whose creation submission outcome is unknown. Records and user files are not
deleted. Missing/corrupt required records fail closed; unavailable fee funding
or uncertain/conflicting inputs can defer recovery.

Startup integration owns the shared qualified Node and both signer locks.
`needs_recovery(a, config, player, payer)` performs one local-only discovery and
reservation restoration before enabling funding, even when it returns false.
No RPC/history recovery is awaited there. Later scans do not overwrite live
journal reservations. The background worker clones the shared connection handle;
connection qualification/opening is single-flight, with no state mutex held
across network requests. Each scan uses a cloned peer, not exclusive ownership.
Recovery can sign concurrently with gameplay, but only for separately claimed
inputs and while holding that old invitation's state lock.
The standalone `recover` wrapper owns its connection only when expired records
exist. One hour remains local expiry policy, not an on-chain deadline.

Escape cancels read-only setup waits at 25 ms polling boundaries and restores
the menu without draining recovery. Already-authorized match submissions still
finish their bounded RPC and retain required durable records. Application exit
aborts recovery reads/submission waits: any possibly exposed cancellation was
saved with its exact inputs before sending, so a later run reconciles/retries
the same transaction. No backlog replay is required before exit. Transport
shutdown waits at most two seconds, followed by one second of runtime shutdown;
local required writes are synchronous and are not interrupted mid-write.

Menu/setup/gameplay show compact checking, pending, accepted or deferred status.
Detailed phase timings append to the startup log, not over terminal rendering.
Missing or reserved funds report insufficient AVAILABLE funds explicitly.
Actual network acceptance time and live latency improvements remain unmeasured.

The main menu shows the complete selected player public key. Goal notices keep
their outgoing trajectory after the sprite expires and project its reflected
path to the actual visible edge at the current terminal width.

Source verification handoff: no independent Cargo/build/install. Sagan owns the
combined build and installation. Run `node::hosted::recovery_tests` first, then
`hosted_expiry_and_refund_preserve_principal_authority_and_reorgs` in
the `node` target's `invitations` module, library `menu::tests`, and these
`ui` target's `goal_notices` cases:
`angled_goal_highlight_tracks_the_border_intersection_not_paddle_height` and
`goal_highlight_reflects_at_the_wall_before_reaching_either_screen_edge`.
Final integration: the combined build and all listed focused cases passed.
The tampered-witness test uses covenant-enabled builder flags; the reflected
goal test's lower-wall expectation was corrected to 2.18. Temporary public
journal persistence needed sandbox permission; the same test binaries passed.
The matching client is installed; see SESSION_HANDOFF.md for hashes/backups.
No funded wallet, live cancellation or two-machine end-to-end validation was
performed, and no user process was stopped.
