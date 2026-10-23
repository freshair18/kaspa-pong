# Hosted invitation recovery

Unjoined hosted invitations expire after one hour of local wall-clock age.
`games/hosted-NETWORK-PLAYER.json` stores public invitation paths and creation
times, including custom host directories. Existing current-format invitations
under `games/*/invitation.json` are discovered using file modification time.
There is no on-chain expiry rule, compatibility layer or migration.

Recovery uses the selected player's and fee payer's signer locks. It refunds
only an exact, currently accepted Lobby owned by those keys. `Lobby.cancel`
returns the entire original principal to its committed refund key; a separate
ordinary owned input pays the fee. The ABI dispatch tag, both VM executions,
principal, fee change, acceptance metadata and current fee limits are checked.
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
Waiting for background recovery before Host/Join now has a visible status screen
and a menu request; an in-flight recovery operation still finishes safely first.

Wallet funding also excludes durable pending inputs from other known sessions.
Public `games/pending-NETWORK-SIGNER.json` indexes exact state paths for both
keys, including custom guest/direct-run paths. Register paths with
`remember_state` before signing. In-memory snapshots belong to each journal;
release follows a durable withdrawal or saved reconciliation, never an RPC
error or timeout. Predicted recovery fee change is not available for funding.
Public genesis inputs remain conservatively reserved, including invitations
whose creation submission outcome is unknown. Records and user files are not
deleted. Missing/corrupt required records fail closed; unavailable fee funding
or uncertain/conflicting inputs can defer recovery.

Startup integration owns the shared qualified Node and both signer locks.
`needs_recovery(a, config, player, payer)` performs local discovery and restores
reservations even when false. `recover_with_node(..., &Node)` borrows that
connection without disconnecting it. Automatic menu recovery must complete
before another signing session or exit; no recovery signing runs during play.
The standalone `recover` wrapper owns its connection only when expired records
exist. Recovery is driven by startup/reconnection, not an active-game timer.

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
