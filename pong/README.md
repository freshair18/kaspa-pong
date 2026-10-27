# Kaspa Pong — multiball development

Two players, each running a node-connected terminal client. Argent covenants
validate every command and game update. State lives in accepted game UTXOs;
there is no game server or referee.

The `multiball` branch builds on the standalone `master` snapshot.
Games support one through six independently colored balls, four preset speeds,
and a configurable winning score. See [multiball rules](docs/MULTIBALL.md).
It uses lightweight movement actors and atomic
paddle-prefix/contact resolution, but narrows the physical court to 58x32 to
reduce repeating angled rallies under the earlier paddle rules. The wider current
catch area permits stationary angled cycles; see [geometry notes](docs/CONTACT_DESIGN.md).
Create fresh matches with this version. Historical measurements in the design
notes are background, not branches or tags included in this repository.
Current rules retain the removal of mutual live-game cancellation. Leaving an unjoined Lobby
lets its creator recover the stake; started games end by score or response
timeout only. No extra collateral is required.

## Start

Normal play hides the gray accepted-position paddle markers. For diagnostics,
use `.\pong alice --debug-paddles`; white gameplay paddles and red local targets
remain visible without the flag. Red stays until the white paddle catches up.
Normal play uses a compact score/controls/fees display with recent transaction
details at the bottom; the debug flag adds technical chain overlays. Cyan ball sparkles, brief
paddle-contact bursts and a green goal-edge ripple are cosmetic and do not
change game speed or physics.

On Windows, interactive matches play short retro effects for paddle hits, wall
bounces, serves, goals and match results. Press **M** during a match to mute or
unmute; the choice lasts until the client exits. Adjust volume in the Windows
volume mixer. Effects are synthesized locally with no music or downloaded assets;
headless clients and other platforms remain silent. No rules or fees change.

From the `pong` folder, select an existing wallet explicitly:

```powershell
.\pong alice
# Or select an exact file:
.\pong --wallet wallets/alice/wallet.json
```

Bare `.\pong` prints usage and exits. Names resolve to `wallets/NAME/wallet.json`
or an existing `NAME.wallet.json`; if both exist, use `--wallet FILE`.
No wallet comes from a default, environment key or saved session. The selected
path is printed before the menu. No existing wallets are moved or regenerated.

Press **1 Host**, **2 Join**, or **q Quit**—no Enter for menu options.
Host plays RIGHT; Join plays LEFT, independently of the wallet's name.
Join asks for the host's checksummed game code. No offer file or partially
signed transaction is exchanged, and the two players can use separate machines
and game directories. q or Esc returns from setup to the main menu.
Release the previous menu key before selecting on the next screen. Public-key
and game-code text entry still use Enter.

Use one terminal per player:

```powershell
.\pong alice host
.\pong bob join GAME_CODE
```

Host begins watching accepted chain updates and displays a code containing its
normal public player key plus a fresh public rendezvous tag. The guest decodes the code,
locks its own five-KAS stake in the normal Lobby transaction, and publishes it
to Kaspa. The host sees that accepted Lobby live, validates it, contributes the
matching stake, and starts the game. Both clients enter play automatically.
No invitation directory, shared `node.json`, offer file, or partially signed
transaction is exchanged. The selected wallet key signs, pays fees, and receives
payouts for the whole match. The public rendezvous tag only distinguishes games
hosted by that wallet and does not create a key or an address.

Press **Up/Down arrows** or **W/S** to move your own paddle one cell. Only the focused
terminal receives keys. **Escape** leaves play and returns to the main menu;
`q` does nothing during play and still exits at the main menu.
Leaving does not cancel/refund an active match and can
result in a timeout loss. **R** optionally dismisses a reorg notice during play.
Routine game reorgs use a quiet activity line; substantial game-state rollbacks
get stronger wording. Both disappear automatically within two seconds, with
details retained in the diagnostic log.

The opening wait is 25 DAA (2.5 nominal seconds), without a numeric countdown. A small colored arrow
appears beside the ball only while waiting to serve, showing its launch direction.
The yellow ball uses a 4x4-dot Braille sprite, two game cells tall. Paddle
contacts include its one-cell vertical radius, so an overlapping edge can bounce
even when the ball's center is outside the paddle. Its drawn horizontal edge
touches the paddle face without penetrating it. Paddles are nine game cells
long, one cell longer at each end, with centers limited to 4..27.
Goal playback follows the incoming angle through the missed post rather than
freezing before it or switching to a horizontal slide. A scored ball waits at
least twenty DAA, then launches on its original phase modulo the court period;
its ball and direction arrow remain visible during the wait. Other balls continue playing.
The yellow ball uses Braille dots (2x4 positions per character) for finer motion.
White collision paddles share the ball's playback time. Gray markers outside
those paddles show latest accepted positions when they differ; your red target
appears inside. The visual court expands with the terminal; the physical field
remains 58x32. Fractional playback interpolation changes only the graphics, not physics.
The ball and white paddles use buffered playback of accepted game state. The
reserve defaults to two DAA (about 0.2 seconds), grows with observation jitter
up to fifteen DAA, and shrinks gradually when observations stabilize. It replaces
the fixed two-second delay; late movement evidence can still correct a displayed
trajectory. RPC bursts do not jump the ball to the latest frame. Short catch-up
periods keep your inputs queued without expiry. Dropped movement transactions
are rebuilt after a fresh chain check; accepted moves are not sent twice.
Opposite unsent keypresses cancel when their removal preserves the displayed
target; individually ineffective boundary presses are also removed. Fresh drafts
can still be withdrawn while being saved, with durable withdrawal recorded before
their reservations are released. Once handed to the network, movements and their
retries cannot be cancelled. Each player has three four-batch input queues,
allowing three transactions in flight. A new burst waits up to 250 ms initially;
its still-unsent batch can incorporate later same-direction presses, up to 27
steps, before the final atomic input check and network handoff. Repeats never
restart the initial timer. Full batches may send early; issued retries keep their
original membership without another collection delay. Local target previews
remain immediate. Contact proofs do not wait for movement collection.
Long holds can still produce multiple batches; the bounded window does not wait
indefinitely for key release, and terminal autorepeat may start after it expires.
Each batch occupies one slot and applies at its own acceptance DAA. Appending
another batch preserves earlier timestamps. Local previews do
not remove network latency. One atomic resolve processes the receiving player's
three lanes and validates the ball contact, leaving all opponent lanes untouched.
Each receiver resolves its own paddle-plane crossing automatically, whether hit
or goal. Intermediate prefix checkpoints free space under queue pressure and
require that paddle owner's signature. If a receiver fails to prove its contact
within 150 DAA (fifteen nominal seconds), the opponent can claim the entire match
by response timeout instead of proving that contact. Accepted receiver prefixes
that consume pre-contact batches renew the grace period; empty updates,
non-receiver compaction and later movement do not. A new flight is not forfeitable
until 150 DAA after both its contact time and its actual confirmation. This
protects active catch-up without allowing unbounded no-progress renewals.
A late proof and an eligible timeout claim still race for the
same Court output; 150 DAA is chain time, not an exact wall-clock guarantee.
Winning contacts automatically authorize a separate collector, which gathers
the six queues and pays the full ten-KAS pot to the winner's player address.
There is no additional collateral or collateral refund output. This keeps the heavy
collection logic out of routine contacts without adding a transaction per rally.
Paddle hits flash cyan with a rebound cue; wall contacts show a brief spark.
A missed ball crosses its post, then a green GOAL marker identifies that end.
Once observed, the score message and missed-end highlight remain visible for
three seconds even if playback skipped the serve pause. They do not delay the
next serve or replay an old outgoing ball; corrected provisional goals clear.
Live flight and goal exit share the same playback clock. Spatial Braille
coverage gives quarter-dot motion without frame-to-frame flicker. Where a slow
exit cannot fully clear the screen before the unchanged serve deadline, it
fades over the final eight DAA. These effects are visual only.
The final goal keeps animating through settlement; the final court stays briefly
before results, and a visible goal notice can extend that cosmetic hold.
The completion screen says whether you won or lost, including which side failed
to respond on a timeout. Observers see neutral results. Accepted outcome and
pending/accepted payout status remain distinct; leaving is not a refund.

Serves start at one of 28 heights on the center line, including the opening
serve. Shots have five slopes: flat, shallow up/down and steep up/down. Paddle
contact selects the slope: center is flat, inner cells shallow, edges steep.
During the serve wait, each colored ball remains at its chosen position with
a direction arrow; the numeric countdown is hidden.
Serve variation is derived from chain time and player keys, not a random beacon.

## Wallets and node configuration

Create a new wallet only if you do not already have one:

```powershell
.\pong --network testnet-10 keygen --out wallets/alice/wallet.json
.\pong alice --network testnet-10 address
```

Key generation is offline and refuses to overwrite a file. Back up the wallet;
it contains one private key. Fund the printed address with a sufficiently large
mature UTXO. Small UTXOs are not consolidated automatically. Each player uses
their own wallet. Winnings and cancelled-match stake refunds return to that same
address. To return ordinary unused funds, use `send ADDRESS AMOUNT_SOMPI`.

Host/Join use `node.json` if present, otherwise the bundled public Testnet-10
Resolver configuration. It supplies only that client's node connection; players
may use different compatible nodes on the same network. Global options
precede the subcommand:

```powershell
.\pong alice --config config/public-node.example.json host
.\pong --endpoint wss://HOST/wrpc/borsh --network testnet-10 probe
```

`probe` checks node/network, synchronization, UTXO index and accepted-chain API
support without selecting a wallet or spending. Native gRPC and Borsh WebSocket
are supported. Private-node example: `config/node.example.json`.

## Game pace and match lifetime

Each player bets five KAS; the winner receives ten KAS, including a response-
timeout winner. There is no client stake selector. Network fees are additional,
and neither player provides additional collateral. The six one-KAS queues and
four-KAS Court are allocations from the ten-KAS pot, not extra deposits.
Started games finish by winning score or response timeout; quitting alone is
not a refund.

The client starts at **Medium**, with one ball and Auto winning goals. Choose
**3 Configure** to change speed, ball count, or winning goals. Speeds are Super
Fast (5 DAA per movement tick), Fast (6), Medium (7), and Slow (8).
Speed option **0 Manual configure** lets you choose **1–9** DAA ticks per
movement step. Choose one through six balls. Winning goals accepts **1–9**,
or **0 Auto**, which selects 3/5/6/7/8/9 respectively.
Auto follows later ball-count changes; an explicit
score stays fixed. Settings last for the running client. The host's game code
carries the actual speed, number of balls, winning score, and five-KAS stake.
The guest's local settings do not replace the host's terms.

After entering a host code, the guest sees the ball count, speed, winning score,
stake, pot, and network. Transaction preparation starts immediately, but a fresh
Enter is required before the joining transaction can be authorized and sent.
Esc/q cancels before submission.

As soon as the game result is accepted, the host's completion screen offers a
fresh rematch code with identical game settings. The offer stays visible while
payouts process in the background. Enter accepts hosting the rematch. The guest
can paste that code on the completion screen and review it before confirming.
Esc returns to the main menu. Every rematch requires both players' approval;
a selected rematch waits for the current payout to finish before proceeding.

Matches are short, single-run sessions. There is no Resume menu, command,
automatic rejoining, or saved player-session record. Leaving play ends that
client's participation in the match, then returns to its menu with the same
explicit wallet selected. Leaving does not settle the match or refund its deposit.
Pending submission/recovery records are saved and each match's workers shut down
before another match starts. Expired-invitation recovery runs independently
through menus and gameplay; it does not gate another invitation, including to
the same opponent, when other unreserved funds are available. Setup q/Esc
cancels read-only waits promptly; an in-flight funding submission is still
durably tracked and finishes its bounded request before leaving.
Accepted transactions, fee accounting and diagnostic caches remain
on disk; existing user files are preserved. Joining a previously opened game is
rejected. Start a fresh match each time.

## Development and retained recovery tools

Rust 1.94+; SDK revisions are pinned in `Cargo.toml`/`Cargo.lock`.
The launcher builds `pong-node` if missing; install a verified new binary after
source changes. Running clients keep their previous executable until restarted.

Collection/refund verification is now separate from routine contact scripts.
The script-split benchmark measured 40.4% cheaper contacts, but 21.4% dearer
movements and 11.0% dearer endings than the previous atomic version. Three
lanes per player remain; total match fees depend on workload. These are
controlled comparisons, not public-network latency measurements. See
[the design and tradeoffs](docs/CONTACT_DESIGN.md).

- [Development tooling and focused checks](docs/TOOLING.md)
- [Five focused test layers](docs/TESTING.md)
- [Protocol and recovery reference](docs/PROTOCOL.md)
- [Concurrent pipelines and timing diagnostics](docs/RESPONSIVENESS.md)
- [Wager funding, payout addresses and cancellation](docs/WAGERING.md)
- [Current handoff](docs/SESSION_HANDOFF.md)
- [Historical first alpha](docs/ALPHA_1.md)

The active game interface is Host/Join, plus wallet utilities and `probe`.
`run` remains for observer/developer inspection of current games. The local
demo, JSON transaction executable and manual create/prepare/approve/broadcast
commands are retired; their source is recoverable from the alpha tag.

Only the current game rules/artifact are supported. Start fresh matches after
this rules change; there are no legacy clients, artifact paths or cache migrations
to maintain. Historical code remains in Git. Saved wallets/game files are not deleted.
