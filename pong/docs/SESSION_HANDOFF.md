# Current Pong handoff

Read for context; do not rebuild or rerun successful checks merely to load context.

## Fresh publication snapshot

`master` is the active standalone snapshot, without earlier Git ancestry.
Previous local branches and user data are preserved, not published. Global ignore
rules exclude wallet/state/session files, logs, local node config, credentials,
build caches and Python bytecode. Current release metadata uses relative source
paths; compiled scripts and pinned artifact identity are unchanged. Earlier
sections describe local development history, not branches bundled in this snapshot.

## Repository outputs and environment cleanup

Runtime wallet/session/game-state examples are already ignored and untracked;
none were opened or deleted. The tracked `artifacts/` bundle is retained as the
sole current covenant release required by the client, embedded tests, pinned
identity and inspection tools; scratch compiler outputs remain under `target`.
See TOOLING.md for the distinction.

PowerShell wrappers now default to this crate's `target`, not an automatically
discovered sibling `argent/target`. Existing caches are usable through explicit
`CARGO_TARGET_DIR`. Optional VS Code extension linking also requires explicit
`PONG_ARGENT_DIR`; setup no longer probes a sibling checkout. No cache removal,
runtime source change, artifact regeneration or client installation is needed.
Historical shared-cache commands below refer to the earlier environment only.

## Test-suite cleanup (no runtime change)

Fourteen scenario integration files are now five explicit Cargo targets:
`contract` (tooling), `node` (default node feature), `gameplay`, `ui` (node),
and opt-in `consensus`. Domain modules preserve existing case coverage and all
previous uncommitted invitation/goal-notice changes. There are 158 retained
cases versus 160 before: only the two ignored comparisons that loaded old Git
artifacts were deleted. Current artifact reproducibility/pinned identity remain;
the v1.0 size assertion is gone. Lane scheduling now compares current workload
fees/state rather than a hardcoded historical fee. A duplicated menu q-transition
assertion block was dropped in favor of the stronger gameplay-decoder case.

`check-win.ps1` and active documentation use the new targets. `check rpc` selects
only the ignored `node` target's `rpc` module; disposable native-node and live
network cases remain opt-in. See TESTING.md. Historical handoff commands below
describe earlier layouts and are not current instructions.

Four non-consensus targets compiled successfully. Eight focused checks passed:
four controls, two rendered/VM collision, one forged-state rejection and one
current-workload lane/fee comparison. Only the changed node target was rebuilt
after replacing its historical baseline. No broad suite, native
consensus/live RPC tests, covenant regeneration or client installation was done.
Runtime source, installed executable, wallet/game files and processes were not
changed by this cleanup. The removed historical harnesses remain in Git history.

Next requested work is still pending: make hosted-invitation recovery genuinely
nonblocking. Main currently drains menu recovery before Host/Join/exit and also
performs a second recovery gate after the wallet banner. Escape merely requests
menu after the operation completes. The visible recovery screen is not a guest
approval wait. Removing these gates safely requires shared input reservations,
durable pending cancellation records and independently owned recovery/signing
work; simply detaching the existing worker could conflict with new funding.

## Silent startup recovery wait: verified and installed

User's host stopped after the wallet banner, before creating any new invitation.
Public startup logs showed connection readiness at 10.6 seconds, then recovery
held the launch gate. A cached idle Lobby was replaying hours of unrelated chain
history. Recovery now reanchors only an exact twice-checked unspent original
Lobby with no pending authorization or observed non-genesis game transition.
The new pre-snapshot cursor preserves subsequent Full replay; fee/budget/reorg
counters are retained. Pending/advanced states still replay. The launch gate
now displays a recovery screen; q/Esc requests menu after recovery finishes.
Two focused checks passed: open-Lobby fast path/pending authorization protection,
and preservation of advanced state/history. One affected-crate build; no broad
tests, funded launch, wallet inspection, process termination or protocol change.
Installed hash matches the build:
01A271A6168A1649457CC1202DE46B20C31FB52D09B99D20B1847E36B059B34E.
Previous client preserved as
target/debug/pong-node-before-recovery-startup-wait-fix.exe. Existing processes
keep the old code; restart the waiting client to load this fix. Live startup
latency of the fix is unmeasured; slow RPC/pending recovery can still delay it.

## Obsolete-record filter hotfix: verified and installed (previous build)

Startup recovery previously deserialized Invitations before filtering their
version/artifact, and parsed schema-2 Caches without filtering their artifact.
Old pre-stake records could therefore fail with missing `stake_per_player`.
The public envelope is now filtered first; registered obsolete invitations are
also excluded from recovery work. Malformed current records still fail closed
with their file path. No migration, default stake, game deletion or wallet access.
Only two focused regression checks ran and passed. One selected-target crate
build completed; no other tests, cargo check, funded launch or protocol change.
Previous filter-hotfix client hash:
8240633DF48FC019512B7E3BB9E861F6C111264BF578B8A59231ABB2A20CD13A.
Previous executable preserved as
target/debug/pong-node-before-old-game-filter-20261023.exe. Running processes
were preserved; restart the client outside an active match to load the fix.

## Startup/recovery integration: verified and installed (before filter hotfix)

Persistent client runtime and qualified connection now span menu, hosted
recovery, Host/Join and gameplay. Background recovery runs while the main menu
is open, and launch/exit drain it before funding/lock release. Both signer locks,
durable reservations and custom state-path discovery remain enforced. Guest
bootstrap replaces the redundant general Full setup sync with bounded own-node
exact-UTXO/hash-only continuation checks; ongoing Full covenant execution remains.
See STARTUP_LATENCY.md and HOSTED_RECOVERY.md. No protocol/artifact/version change.
Combined source includes Descartes' menu public-key, goal-border/reflection and
recovery changes. Twenty-three focused checks passed: startup owner/bootstrap
module (5), hosted recovery module (3), menu public-key/error module (2), binary
menu-return module (4), two exact goal-border/reflection cases, and seven exact
invitation cases (guest funding/signatures/VM, durable fee budget/reservations,
exact unspent checks, competing joins/reorgs, cold root retiming into Live,
cancel/join race and hosted expiry/refund).

The final selected-target affected-crate build passed; compiler integration
required converting a boxed error to String before an awaited disconnect and
using a manual generic owner Default. Two test fixtures were repaired: covenant
builder flags for rebuilding a tampered witness, and a reflected-coordinate
expectation (2.18, not 3.18). Only the failed goal target was rebuilt after its
test-only correction. Two synthetic journal cases initially failed sandbox
temporary persistence, then passed with permission using the same executables.
No broad suite, cargo check, funded game, private wallet inspection, real-wallet
client launch or covenant/version regeneration occurred. Startup fake timings
were sub-millisecond (reported 0 ms); these are not network latency evidence.
Accepted-start/opening/render timings are instrumented, not measured live here.

Previously installed startup/recovery build:
85667D12B772D1BC9E3E78183594D67888EF86E9D4D13BA4D057DD758A4CD1C7.
Offline installed `--help` passed. Previous hash
7B1540687BCFE220A99D9B60410B83E0ED273A8B590F3B8FB51F35F46F9C42AE
is preserved in `target/debug/pong-node-before-startup-recovery-menu-goal-20261023.exe`.
Windows blocked direct replacement because the old image was in use; a same-
directory rename preserved it as `target/debug/pong-node-running-before-startup-20261023.exe`,
then installation and matching SHA256 verification succeeded. No process was
stopped, including the existing invitation directory. Existing processes retain
their old image; newly launched clients use this build. No user data was deleted.

## Current release: version 19

Guest duplicate-file follow-up: host and guest default to the same
`games/<genesis-id>` directory when run in one checkout. Guest previously used
exclusive creation for `invitation.json`, so the host's already-saved public
record (or a prior pre-funding guest attempt) caused Windows error 183. Guest now
accepts an existing record only if its complete parsed JSON equals the freshly
authenticated invitation. Conflicting/unreadable records remain errors and are
never overwritten. Host creation, state locks and the no-existing-player-state
guard are unchanged. No covenant/artifact/version changes or file deletions.
Two focused storage tests passed (shared host record/retry and conflicting record
preservation); the initial sandboxed attempt could not persist temporary files,
so the same tests passed with temporary-file permission. No broad suite was run.
The matching client is installed; restart only the guest to retry the same code.

Setup hotfix: Host/Join previously returned to the menu because no invitation
directory was configured; stderr was hidden by the next alternate-screen menu.
The menu now retains a wrapped last-error banner. Local ignored `pong/node.json`
selects testnet-10 and http://127.0.0.1:17720. A hidden, wallet-free directory
is running and answers HTTP 200. A sandboxed process query could not see it;
a subsequent elevated listener check confirmed the ORIGINAL PID 23116 remains
alive. There is no evidence that it crashed. Public state is
`pong/target/invitation-directory-v19.json`. Its initial coverage starts after sink
60e19c80b36ef05640cd1723fccce5d6a073dd05fa69830897fede04fed31d3d;
only fresh invitations after that anchor are covered. `pong.cmd` now restarts
this explicitly configured loopback directory from its existing state before
ordinary client launches; the helper uses no wallet and preserves diagnostic
logs per launch. Explicit --config/operator commands remain operator-managed.
No boot auto-start was installed. Manual restart uses the same config/state,
WITHOUT supplying a new anchor. An initial approval-review quota failure was
later resolved: the elevated helper invocation detected the live service and
a synthetic-public-key lookup returned HTTP 200. The helper's cold-start branch
is syntax-checked, not exercised by stopping the existing service.
Guest lookup now displays a cancellable auto-refresh screen for up to 60 seconds
when the directory returns no match. Nonempty invalid/ambiguous responses remain
errors, and own-node UTXO validation is unchanged. Transport errors remain visible
in the main menu rather than silently disappearing. Long errors are bounded so
the menu remains usable.
Other computers need an explicitly reachable shared directory, not their own
loopback address. Probe now reports its sink for explicit fresh-directory setup.
Verification: 1 error-banner test, 4 menu-return tests and 6 menu/input tests
passed. Live read-only testnet probe confirmed synced node, UTXO index and Full
API; loopback synthetic-public-key lookup returned HTTP 200 with an empty list.
The guest follow-up rebuilt once for its additional source changes, then passed
the bounded error-banner check and three focused invitation checks: pending to
indexed lookup plus wrong-player rejection, absent configuration/wrong network,
and independent-client-directory HTTP discovery. The first sandboxed localhost
attempt failed connection restrictions; all three passed with network permission.
No broad test suite, cargo check, funded game or covenant regeneration was run.
No wallets were opened, funds spent or user processes stopped.

Three requested workers share this checkout with disjoint write scopes:
adaptive visual playback (clock policy/tests), on-chain authenticated invitations
and guest-completed joins (contracts/setup/discovery/menu/CLI), and a serial UI
worker for persistent goal notices then Escape-only gameplay exit.
The user then requested a fourth small UI worker for explicit local-side
winning/losing/timeout messages and neutral observer results. All workers finished
their source/test edits without building; parent integrated and verified the
combined release, regenerated the sole artifact and installed the matching client.
No compatibility, wallet/game migration, funded tests or user-process stopping.
GAME_VERSION is 19; five-KAS stakes and seven-DAA pace remain unchanged.

Playback reserve starts at five DAA and adapts up to fifteen using observation
gaps; stable observations lower it one DAA every two seconds. It replaces the
fixed twenty-DAA delay, with 0.8x reserve refill, bounded 2x catch-up, fractional
continuity and no invented observed progress. Late validated trajectories can
still correct graphics; no measured public-network latency improvement is claimed.
Goal notices persist three wall-clock seconds separately from outgoing motion,
retain the scored end/known crossing height and survive skipped serve pauses or
rapid settlement. Reorgs/corrections clear contradicted notices; exact goal
identities suppress replay without hiding different later goals at the same
score. Only Escape leaves gameplay; setup/menu q and movement repeats remain.
Completion uses explicit local-side/observer perspective, with win/loss and
own/opponent timeout wording. Fees and projected/accepted/payout status remain.

External offer/signature exchange is removed. A compact host-player-signed
creation manifest rides the existing Lobby transaction and binds its network,
rules, participants, terms and exact funding source. Funding authorization is
separate. Manifest authentication is client-side; Argent still enforces locked
terms, guest ALL signature, initial outputs and financial transitions. The guest
constructs/signs/funds locally, bootstrapping an authenticated old unspent Lobby
from its own node's exact UTXO view. All subsequent game transitions retain
independent covenant execution. Root retiming and bootstrap tip reorgs are covered.
Guest-start enqueue now respects cumulative fee authorization instead of bypassing
it. Three-lane gameplay, safe unsubmitted cancellation and pending durability stay.

Checksummed short codes use sixty identity bits and twenty checksum bits, with
full-code fallback for ambiguity; full identities are retained internally.
The new wallet-free `index` command provides persistent read-only HTTP discovery,
including cold guests with no shared files/cache. It serves between live chain
batches, bounds undo to 2,048 blocks and reads fragmented requests correctly.
An explicitly configured `invitation_index` service is REQUIRED before Host/Join;
none was deployed or selected for the user. Operators must provide history
coverage and a trusted network/tunnel (built-in HTTP has no TLS/public-service
hardening). See ONCHAIN_INVITATIONS.md for deployment, research and limitations.

Sixty-seven focused checks passed: thirteen invitation/security/discovery cases;
nine existing setup/funding/retry cases; nine adaptive-clock cases; five goal
notice, four goal-animation, four Escape, five result-message, six menu-code,
four menu-return and eight existing clock/completion regressions. New invitation
tests used real loopback HTTP with synthetic accepted-chain/UTXO data and separate
temporary directories; two-live-node/two-machine end-to-end play was NOT run.
The native scenario was adapted to independent client directories and grpc/wrpc
endpoints and compiled, not executed. No broad suite, cargo check, funded wallet
test or repeated successful rebuild ran. One combined affected-crate build took
2m16s; one old clock assertion assumed a two-second reserve, so that expectation
was corrected to the observed-tip cap, its test target rebuilt (19s), and only
the failed case rerun. The production executable was unchanged by this test-only
repair. Offline installed --help, index --help and join --help passed.

Current artifact fe67363b9aabedd89083ab2f24d973d54ca52d7d735d04f4891dfbf380d0d55d.
Lobby is 5,696 B; Player 2,977 B, Court 12,759 B and Collector 9,120 B are unchanged.
Creation adds a 324-byte devnet manifest; there is no extra setup transaction.
Setup fees may change; no measured fee savings or gameplay-cost reduction is
claimed. Restart BOTH clients, configure the directory and create fresh matches.

## Previous release: version 18

The user authorized fixes from the two diagnostic agents, then a third agent
for immediate menu option keys. Animation owns ascii.rs/animation tests;
cancellation owns input-preview APIs, spend coordination, client/persistence
and their tests; menu owns menu.rs/terminal.rs/menu tests. All three saved edits
before hitting their usage limits. Parent completed cancellation regression
coverage, fee-parent reservation safety, frontend integration, version/docs,
focused verification and matching install.

Targets: common live playback time for ordinary flight/goal exit; sub-dot
Braille motion; smooth outgoing fade where the unchanged twenty-DAA serve
schedule cannot allow full clipping. Input work targets atomic input capture,
safe durable withdrawal of known-never-submitted drafts, current-intent batch
membership after initial 250 ms readiness, and target-preserving boundary-noop
removal. Previously exposed/uncertain/accepted transactions remain protected.
Menu option selection should be immediate; actual text entry keeps Enter.
No compatibility layer, wallet/game migration, funded tests or process stopping.
GAME_VERSION advances to 18; stake stays five KAS/player and artifact unchanged.

Forty-seven focused checks passed: eight collection/atomic capture checks,
nine concurrency/persistence checks (including four new withdrawal/recovery
cases), four cancellation checks, four movement batching checks, four goal
animation checks, two fractional-motion checks, six menu hotkey checks, three
picker refresh checks, three visual collision checks and four menu-return checks.
One affected-crate build compiled selected targets with two jobs. Four temporary
file persistence tests initially failed with Windows sandbox access denial;
the same binaries passed outside the sandbox, without rebuilding or modifying
filesystem semantics. No broad suite, cargo check, covenant regeneration or
funded/live-wallet test ran. Installed client passed offline --help.

Cache schema is now 2. A runtime-only capability distinguishes never-exposed
drafts; recovered unmarked pending transactions remain protected. Withdrawal
markers must persist before reservations, authorized fees or queued intent can
be released/rebatched. Losing an exposed fee parent invalidates its fresh child
without releasing that child before durable withdrawal. Final input drain and
commit use the same mutex as enqueue; issued retries keep original membership.
Boundary-clipped unissued no-ops are removed without changing the target.

Goal exit uses fractional playback time, not a separate wall clock. Opaque
sprites retain sixteen distinct dots with quarter-dot centroid motion. Some
slow exits fade over the final eight DAA of the unchanged serve pause. Final
settlement continues cosmetics from the last playback coordinate. Menu choices
are immediate; p explicitly enters a path editor with Enter to submit. Held
keys cannot select another screen's option; redirected input remains line-based.

No measured network-latency improvement is claimed. Terminal rasterization and
late validated trajectory corrections remain visible limitations; live visual
feedback is still needed. Restart BOTH clients and create fresh offers/matches.
No process, private wallet, saved game or user analysis was changed/deleted.

## Previous release: version 17

Two requested agents implemented the fixed client stake reduction from
ten to five KAS per player (ten-KAS winner pot) and continuous goal rendering.
Operational collateral stays seven KAS: host Lobby twelve KAS, live Court
eleven KAS plus six one-KAS queues, seventeen KAS escrow total. No stake
selector, compatibility layer, wallet/game migration or artifact change is
needed: the existing bounded covenant stake parameter already supports five.
The client version advances to 17 and requires fresh offers/matches.

Goal paint now interpolates the last approach tick even when it scores. It
retains incoming pre-miss motion across accepted checkpoints, including the
one-cell last segment possible on leftbound flights, and continues that screen
velocity/slope past the post. The interior horizontal mapping is extended
linearly and vertical wall reflection is retained; the sprite clips naturally
at the court edge. After exit the chosen serve position is restored. A compact
goal/countdown footer preserves the 24-line court/status layout. Cached frames
and final settlement cannot restart the same goal effect; resumed serves take
precedence over stale goal cues. The effect is at most two wall-clock seconds,
not a new game delay. The animation worker changed ascii.rs/new goal tests;
the stake worker changed the fixed amount/launch/validation and wager fixtures.
Parent integrated version/docs and installed the matching focused-test build.
Scoring, contact DAA, seven-DAA pace and twenty-DAA serve pause stay unchanged.

Twenty-two focused checks passed: five grouped goal-animation tests; three
existing visual-collision and three visual-court checks; three wager setup,
four wager payout and four launch-storage checks. These cover continuous flat/
angled approach and exits, wall reflection, clipping, chosen serve restoration,
compact countdown/status layout, accepted-checkpoint history, cached/final goals,
the actual guest-funded five-KAS approval path (including a six-KAS coin),
rejection of signed ten-KAS offers, external fees, normal/timeout winner payout,
mutual cancellation and storage-mass selection. One affected-crate Cargo
invocation compiled the selected targets with two jobs; no repeat build, broad
suite, cargo check, covenant regeneration or funded/live-wallet test ran.
The installed executable passed an offline --help smoke check. No process was
stopped; wallets, saved games and user analysis remain unchanged. Restart BOTH
clients and create fresh offers/matches. Live visual feedback is still useful;
these checks compare deterministic rendered frames, not a funded terminal game.

## Previous release: version 16

Three requested agents handled ball/paddle overlap, less-eager batching and
paddle-size integration in disjoint write scopes. Paddles grow from seven to
nine cells (+one each end), with centers 4..27. Paddle contacts include the
four-dot-high ball's one-cell vertical radius: absolute center offset <=5 hits;
offset six misses. Rebound slopes still clamp to -2..2. Horizontal contact
planes/times, vertical wall period, 58x32 field and seven-DAA pace are unchanged.
Client collection grows from 100 to 250 ms, still measured from the first press,
without repeat-triggered extension, retry delay or waiting for key release.
Local preview/cancellation stay immediate and due Court proofs bypass collection.
Long holds and late terminal autorepeat can still generate multiple batches.

One affected-crate Cargo invocation built the combined library/client and selected
test targets. Twenty-seven focused checks passed without rebuilding: two actual
sprite/client/VM edge-and-forgery checks; three paddle silhouette/clamp/contact
checks; six collection policy checks; three visual contact/fractional/goal checks;
the geometry check and 160-case independent tick oracle; four batching and four
cancellation integration checks; immediate-preview reconciliation; guest-funded
start validation; both score winners' pot/collateral payouts. No broad suite,
cargo check, native consensus scenario or live funded match ran. The installed
client passed offline --help. No running process or wallet/game file was altered.
Restart BOTH clients and create fresh matches; there is no compatibility/migration.

Wider reach changes stationary-cycle results. The original experiment was run
in memory with legal centers 4..27 and hit offsets <=5, without modifying its
files: 129,024 cases, 4.5805% angled cycles overall; 5,376 aligned cases, 19.6801%;
224 centered cases, 22.7679%. The old zero-aligned-cycle result is historical,
not a property of version 16. No extra geometry/rebound change was made.

## Previous client-only input experiment

New unsent movement batches use a fixed 100 ms collection window from the first
press, instead of immediately submitting a singleton. Same-direction repeats
grow the batch without restarting the timer. Full 27-step batches can send early;
previously issued retries keep their original grouping and have no new delay.
Readiness is nonblocking and applies only to movement; due Court proofs remain
prioritized. Local previews/cancellation remain immediate. Ball pace stays seven
DAA. That experiment was client-only and did not change protocol 15's artifact.
Twelve focused checks passed: four deterministic movement_collection_tests,
four batched_movement client checks (including a due-contact bypass), and four
input_coalescing checks preserving unsent cancellation and issued retries.
Selected targets were compiled once; no broad suite or covenant rebuild ran.
The matching executable is installed; restart clients to try the collection
policy. No running process was stopped, and no wallet/game data was changed.

## Previous release: version 15

Four agents started accepted-progress timeout robustness, seven-DAA pace,
responsiveness audit and invitation refresh/menu return. Speed and audit finished;
the other workers stopped before handoff, and the parent completed their saved
edits, added focused tests and integrated the approved payer-first funding fix.
Version 15 was compiled, verified and installed. No backwards compatibility
or saved-match resume was added. Restart both clients and create fresh matches.
q returning to a menu does not cancel/refund a funded match or create a new one.

Expiry is max(deterministic contact time, accepted flight/progress anchor) + 50
DAA. Only a new flight or a receiver prefix consuming pre-contact batches renews
the anchor, bound to real Court acceptance. Other replacements carry the old
anchor. Watermark-only/noop/nonreceiver/late-movement work cannot renew it. At
most twelve pre-contact batches remain once due, so renewals are finite. Severe
network delays can still cause a timeout; this is not an unlimited liveness
guarantee, and a late valid proof can still race an eligible timeout.

Due contacts pause new movement authorization/selection without discarding
unissued intent or existing reservations. The invitation picker refreshes local
public files every second, with r + Enter, preserving draft paths/stable numbers
and rechecking on selection. q/Esc returns from play/nested screens to the menu;
main-menu q exits. Per-match runtime/terminal/worker/writer/lock cleanup precedes
another session; current setup operations and authorized submissions finish
before leaving. JSON and bounded developer commands keep noninteractive exits.

Audit found no rollback: pipeline.rs/rpc.rs match 570ab48. Public timing samples
show slower observation in one newer client, not a controlled version comparison.
See RESPONSIVENESS.md. Gameplay now uses an available payer coin immediately,
querying a different player address only when necessary; pending fee-change
bypass and exhaustive launch storage-mass selection are preserved. Guest launch
VM/candidate preparation runs off the async runtime without dropping validation.

Observed version-14 incident, public game 98389a731aee: right contact deadline
603785029, timeout eligible 603785079, receiver partial checkpoint accepted at
603785066. Final receiver proof submitted at 1792713048950 ms and acknowledged;
left timeout submitted 192 ms later but accepted first at DAA 603785098. Both
players were active. Use only public game state/diagnostics when investigating;
do not read wallet contents or alter/delete these files. Actual node acceptance
wall-clock time remains unavailable; timing logs contain observation timestamps.

## Retained version-14 features

Version 14 introduced receiver-owned proofs, wager funding/payouts and quieter
reorg presentation. Later versions retain these features with the improvements
above. No user process was stopped; wallets, saved games and analysis are preserved.

Version 14 originally used ten KAS per player (twenty-KAS pot),
without a client amount selector. Wager funding and payout are implemented;
the existing seven-coin operational collateral is separate from the wager.

Reorg presentation is implemented, library-tested and installed: routine events
use a quiet activity line, while three distinct lost/retimed game actions or a
game acceptance shift of at least twenty DAA get stronger wording. Every event
expires after two seconds from first display; cached repetitions cannot renew
it. Detection, recovery and detailed logs are unchanged. Three focused tests
passed (severity, exact two-second expiry, and unique-event/dismissal behavior).

## Versions and scope

- Stable baseline: v1.0, commit 0bd10da.
- Development: codex/contact-resolution, package 2.0.0-dev, GAME_VERSION=19.
- Sole current artifact:
  fe67363b9aabedd89083ab2f24d973d54ca52d7d735d04f4891dfbf380d0d55d.
- Previous atomic version: 97023f9. This handoff accompanies the integrated update.
- Argent remains pinned to 9a9f4b107116d0b259fae629f200fa3b663e7e9d;
  Rusty Kaspa to a41a333b08848f41bf737b72592e463a6011b8ac.
  The independently modified sibling Argent checkout was not changed.

The user explicitly requested three coordinated agents for collection sharding,
58x32 geometry, and concurrent gameplay pipelines, then a fourth for the visual
paddle collision. Those earlier four agents' integrated release was version 13.
No backwards compatibility was added: old games/artifacts
are not supported by the current client, and no cache migration runs.

Preserve three lanes/player, four batches/lane, seven operational coins, explicit
wallet selection, Host/Join, batching, buffering, reorg recovery and fee caps.
No Resume, compatibility layer, migration or deletion of wallets/game files.
Never read private wallet contents or use funded user wallets for verification.
Keep checks targeted; do not routinely run every test or repeat successful builds.
Preserve the user's original analysis/ simulation and results.

## Collection split

Court.resolve consumes Court and the receiving player's three Player queues,
folding at most four timestamp groups strictly before the requested boundary.
Its relative DAA lock enforces the boundary. At a fully reached contact deadline,
the SAME transaction computes hit/miss and outgoing flight. Opponent queues stay
unspent. Original command timestamps survive; partial prefixes retain leftovers.
Queue pressure still needs bounded compaction; quiet tails wait for contact.
Each prefix/contact requires that paddle owner's signature. A checkpoint must
advance the watermark, consume a batch or prove contact; empty no-progress Court
replacements are forbidden. All partial prefixes preserve the contact deadline.

Only the receiver proves hit/miss. At the accepted-progress expiry above, its
opponent can sign Court.timeout and win the whole match, without changing the
scores or supplying the receiver's queues. Opening time uses actual Court
acceptance metadata. Fifty DAA is five nominal seconds, not exact wall time.
A valid late contact proof still races an eligible timeout for the same UTXO.

Court.settle checks the winning score and emits Collector, consuming only Court.
Court.cancel does the same with both signatures; timeout also emits Collector.
Collector.collect consumes itself plus all six latest Players and directly
returns operational collateral separately from the wager payout. There is
no Finished actor and no dispatcher before contacts. Ending remains two
transactions. Late in-flight commands after authorization are tracked before
collection uses their actual successors.

This uses Argent's existing typed leader/delegate authentication, not selector
families. The earlier selector prototype failed, but no compiler limitation was
established; the pinned SDK does support selectors. No SDK changes were made.

Version-16 scripts: Court 12,759 B, Player 2,977 B, Collector 9,120 B,
Lobby 5,722 B. Routine contact scripts total 21,690 B, excluding witnesses.

The pre-geometry script-split comparison against 97023f9 passed with identical
funding/relay rate and VM-validated constructions:
movement 543,000 -> 659,400 sompi (+21.4%);
contact 7,115,400 -> 4,238,600 (-40.4%);
both ending transactions 8,605,500 -> 9,551,200 (+11.0%).
One movement + contact + ending: 16,263,900 -> 14,449,200 (-11.2%).
These historical comparisons predate the new signatures, timeout and escrow
fields. Do not present them as exact current fees. More movement can offset
contact savings; fees are not shared equally automatically.

## Wager funding and payout

The client fixes five KAS per player; no selector/flag/config override. The
bounded covenant stake parameter allows future rules/client changes. Host locks
twelve KAS in Lobby: its five-KAS stake and seven-KAS operational collateral,
plus an external fee. Its public offer is an explicitly unfunded proposal signed
SINGLE|ANYONECANPAY for Lobby input zero and Court output zero. Covenant checks
fix that position and all seven game outputs. No host fee coin is authorized.

Guest adds its own five KAS plus start fee, signs ALL, and selects a real owned
coin by actual storage mass. Canonical outputs, signatures, exact sources and
independent VM execution are checked. Only the completed start is spendable;
the proposal makes no mineability claim. Host cannot preflight an unknown guest
coin before locking Lobby. Fee pricing measures VM-assigned compute budgets.

Court holds eleven KAS and each of six lanes one KAS. On normal/timeout win,
Collector output zero returns seven KAS to the host refund key; output one pays
ten KAS to the winner's player key. Mutual cancellation pays five KAS to each
player key instead. Pre-start cancellation returns all twelve KAS to host's
refund key without spending any guest coin. All fees require separate funding.

Wallets with separate fee/player keys can spend either owned address directly
with its corresponding key, so winnings remain reusable without transfers or
wallet migration. Fee totals aggregate both keys without double counting and
exclude principal. Guest pays start fee; host pays lobby fee; other actions are
paid by their submitting wallet. See WAGERING.md for details and limitations.

## Geometry and visuals

Physical field: 58x32; contact planes x=2 and x=55; center-line serves x=29.
Paddles are nine cells, centers 4..27; ball speed two horizontal cells/tick,
seven DAA/tick, opening 25 DAA, goal pause 20 DAA, five slopes, first to three.
Narrowing naturally shortens crossing time.

The historical seven-cell point-ball simulation at 58x32 had 151,424 cases,
1.1702% angled cycles overall, zero in 5,824 aligned-paddle cases and 224 centered
cases. These figures do not apply to version 16; see the current results above.
Indefinitely horizontal rallies
are intentionally out of scope. Original analysis files were not overwritten.

The four-by-four-dot sprite now maps its visible horizontal edge to the inner
paddle face instead of centering the sprite on that face. It touches and rebounds
without overwriting the collision paddle, across widths 32..128. Goal animation
continues past the posts to the court edge. That earlier horizontal presentation
fix did not alter physics. Version 16 separately adds vertical ball-edge overlap.

## Pipelines and limitations

The implementation separates bounded fetch-ahead, ordered independent validation,
preparation/submission and ordered persistence. The coordinator alone owns local
pending/reservations/authorization. It merges validated chain fields without
overwriting newer local spends. Exact draft inputs, fee-parent dependencies,
exclusive actions, reorg generations and mandatory durable pre-submission records
remain enforced. Non-required cache snapshots coalesce; outboxes drain on shutdown.

See RESPONSIVENESS.md for phase diagnostics, actual versus observed acceptance,
and the retained Full-RPC path. The pinned API has no direct historical
transaction-by-ID lookup with exact spent-input acceptance metadata. A lighter
prefilter is not implemented. Actual node acceptance wall-clock time is not
provided: node_acceptance_unix_ms is null, not a block timestamp. No measured
public-network latency improvement is claimed.

## Historical verification: version 15

Version-15 focused checks passed (32 checks, no broad suite):
- pong_timeout_robust: seven VM/client checks reproducing the observed 027/030/041
  prefix incident, both receivers draining all twelve batches, nonreceiver/noop/
  late-input nonrenewal, early/disabled sequence and claimant restrictions,
  delayed outgoing confirmation, forged anchors, due-contact priority, exact
  metadata reorg/cache replay, late commands and final wager/collateral payout.
- menu::refresh_tests: three checks for discovery after refresh, stable numbers/
  typed paths, r-prefixed paths, manual refresh/back commands, changed or started
  offers rejected on selection, and no automatic joining.
- fee_funding_tests: three async checks proving payer success skips optional
  lookup, player fallback/errors, no-funds behavior and equal-key deduplication.
- menu_return_tests: four binary checks for nested/direct-command navigation,
  EOF exit, headless/JSON exclusions, and finishing an authorized submit before
  returning to the menu.
- Existing focused checks: four contract and two client goal_response_timeout,
  four wager_payout, three wager_setup and two fixed_pace checks, updated to the
  current rules. The actual guest-funded public-offer approval/VM/signature path
  was checked against the new artifact, not only the headless start builder.

One affected-crate Cargo invocation compiled the selected library/binary and
test targets; focused modules then used those executables without rebuilding.
No cargo check, broad suite, native/direct-consensus test, funded-wallet test or
live match was run. Non-fatal Windows incremental hard-link/copy warnings only.
Installed executable passed an offline --help smoke check. UI navigation is
covered by deterministic model/key-path tests, not a live funded terminal match.
Final diff whitespace checks passed; prior source/version/guard reversions were
restored from the committed baseline before final compilation.

Historical version-14 focused checks:
- wager_payout: four VM checks for pre-start host-only refund, immutable live
  escrow, both score winners, both timeout winners and mutual-cancel returns;
  altered amounts/destinations are rejected.
- wager_setup: three client/VM checks for explicitly unfunded proposals, guest
  contribution/change, canonical ordering, authorized sighash flags, no host fee
  input exposure, fixed amount, owned-key selection and fee attribution.
- goal_response_timeout: four covenant checks and two client checks, covering
  early/disabled sequence rejection, correct claimant, receiver-only proofs,
  stable deadlines across partial prefixes/opening, no-progress rejection,
  late-proof race and timeout -> late command -> payout -> reorg/cache replay.
- launch_storage: four focused checks for actual guest funding, minimum storage
  mass, tiny-change rejection and bounded relay-fee convergence.
- fixed_pace: two invitation/menu/CLI checks enforcing six DAA and current offers.
- spending_and_motion: three fee-ledger/results-screen/clock checks, including
  separate host/guest setup fees and stake principal excluded from fees.
- Three deterministic reorg-notice library checks passed before final artifact
  integration; no reorg presentation source changed afterward.

One affected-crate Cargo invocation built the combined client and contract/client
test targets. Subsequent focused modules used those executables without rebuilds.
Only non-fatal Windows incremental hard-link/copy warnings were emitted. No
broad suite, live funded game or native/direct-consensus scenario was run for
version 14. The new timeout's VM guards are verified; these are not measurements
of public-network wall-clock acceptance latency. The installed client passed an
offline --help smoke check without opening any wallet or connecting to a node.

Previous version-13 focused checks (historical, not rerun as a broad suite):
- Collection authorization/contact/refund security and opt-in fee comparison
  (two collection_split checks, before the geometry update).
- Current 58x32 walls, both contacts, scoring, center serves and covenant/client
  agreement; the 160-case independent tick oracle; artifact reproduction.
- Three visual collision checks covering both sides/angles, widths 32..128,
  fractional approach/rebound and goal crossing.
- Original exhaustive geometry simulation, without changing its files.
- All five concurrent_pipelines client checks: validated publication versus newer
  authorization, exact metadata/draft invalidation, fee-parent/reservation and
  exclusive-action safety, persistence ordering/failure/clean drain, and
  closing -> late command -> refund -> reorg/cache validation.

Earlier version-11 broad/native successes must not be described as current
pipeline or wagering tests. Version-13 temporary-write restrictions were handled
by running its already-built focused client test executable outside the sandbox,
not by changing filesystem semantics. Version-14 focused checks needed no such
workaround. Final diff whitespace checks passed.

Previously installed client (before the startup/recovery integration above):
7B1540687BCFE220A99D9B60410B83E0ED273A8B590F3B8FB51F35F46F9C42AE.
The pre-duplicate-file fix is retained as
pong/target/debug/pong-node-before-shared-invitation-fix.exe, hash
FBF9D373412CE8C1FCEF0EB2BC5BE528290D50AEA9EDAED0A7B211E470DA12C9.
The interim pre-guest-wait hotfix is retained as
pong/target/debug/pong-node-before-guest-directory-wait.exe, hash
7C6D772DD3782F1E7B241D896DB7923F68FDC9579D2F6B087AA485CE305F1BB1.
The pre-hotfix executable is retained as
pong/target/debug/pong-node-before-directory-setup-fix.exe, hash
0A8457F7138FF5848607938C6AD300C2FA11B3DB0A7A1CFB7B718847CA1C992E.
The previous pre-v19 executable is retained as
pong/target/debug/pong-node-before-onchain-ui-v19.exe, hash
B75A178D4B2937E709D54322ECD2767A891C15DE662B818C33019352DD70DB2D.
The older before-animation-input-menu-v18, before-five-kas-goal-animation,
before-ball-paddle-batching, before-input-collection,
before-timeout-refresh, before-wagers-timeouts and
before-shards-58x32-pipelines backups remain.
No running user process was stopped. Restart BOTH clients and create fresh
matches to use version 19; no existing match is migrated. Wallets, saved game
files and original analysis files were not changed or deleted.
