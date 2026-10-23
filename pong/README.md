# Kaspa Pong — contact-based development

Two players, each running a node-connected terminal client. Argent covenants
validate every command and game update. State lives in accepted game UTXOs;
there is no game server or referee.

This standalone development snapshot is tracked on `master`.
It uses lightweight movement actors and atomic
paddle-prefix/contact resolution, but narrows the physical court to 58x32 to
reduce repeating angled rallies under the earlier paddle rules. The wider current
catch area permits stationary angled cycles; see [geometry notes](docs/CONTACT_DESIGN.md).
Create fresh matches with this version. Historical measurements in the design
notes are background, not branches or tags included in this repository.

## Start

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
Host plays LEFT; Join plays RIGHT, independently of the wallet's name.
Join asks for the host's checksummed game code. No offer file or partially
signed transaction is exchanged, and the two players can use separate machines
and game directories. q or Esc returns from setup to the main menu.
Release the previous menu key before selecting on the next screen. Public-key
and game-code text entry still use Enter.

Use one terminal per player:

```powershell
.\pong alice host GUEST_PUBLIC_KEY
.\pong bob join GAME_CODE
```

The host obtains its own public key from its selected wallet, invites the guest's
public key, locks its five-KAS stake plus seven KAS of operational collateral,
and publishes a compact player-signed invitation in the lobby transaction.
Share its game code. The guest retrieves it from the configured invitation
directory, verifies the host's signature and its own node's accepted Lobby,
contributes its own five-KAS stake, and signs the join locally. Both clients
enter play automatically with the same chain countdown. No second host signature
or file transfer is required.
The guest selects its actual launch funding by minimum storage mass and checks
feasibility before authorizing its contribution. The host cannot preflight an
unknown guest coin before locking Lobby; invalid guest funding is rejected
before start. See [wager funding and payouts](docs/WAGERING.md).

An invitation directory must be running and explicitly configured as
`invitation_index` in each client's node configuration. It is a locator, not a
game server or spending authority. Host checks reachability before locking
funds; guests independently verify its results. Run the wallet-free
`index --state invitation-index.json --listen 127.0.0.1:17720` command with
the intended node/network configuration. For another machine use a reachable
service address through a trusted tunnel. Coverage, deployment and cold-start
joining are described in [on-chain invitations](docs/ONCHAIN_INVITATIONS.md).

Press **Up/Down arrows** or **W/S** to move your own paddle one cell. Only the focused
terminal receives keys. **Escape** leaves play and returns to the main menu;
`q` does nothing during play and still exits at the main menu.
Leaving does not cancel/refund an active match and can
result in a timeout loss. **R** optionally dismisses a reorg notice during play.
Routine game reorgs use a quiet activity line; substantial game-state rollbacks
get stronger wording. Both disappear automatically within two seconds, with
details retained in the diagnostic log.

The opening countdown is 25 DAA (2.5 nominal seconds). A small yellow arrow
appears beside the ball only while waiting to serve, showing its launch direction.
The yellow ball uses a 4x4-dot Braille sprite, two game cells tall. Paddle
contacts include its one-cell vertical radius, so an overlapping edge can bounce
even when the ball's center is outside the paddle. Its drawn horizontal edge
touches the paddle face without penetrating it. Paddles are nine game cells
long, one cell longer at each end, with centers limited to 4..27.
Goal playback follows the incoming angle through the missed post rather than
freezing before it or switching to a horizontal slide. The twenty-DAA pause
before the next serve remains intentional and has an explicit countdown.
The yellow ball uses Braille dots (2x4 positions per character) for finer motion.
White collision paddles share the ball's playback time. Gray markers outside
those paddles show latest accepted positions when they differ; your red target
appears inside. The visual court expands with the terminal; the physical field
remains 58x32. Fractional playback interpolation changes only the graphics, not physics.
The ball and white paddles use buffered playback of accepted game state. The
reserve starts at five DAA (about half a second), grows with observation jitter
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
within fifty DAA (five nominal seconds), the opponent can claim the entire match
by response timeout instead of proving that contact. Accepted receiver prefixes
that consume pre-contact batches renew the grace period; empty updates,
non-receiver compaction and later movement do not. A new flight is not forfeitable
until fifty DAA after both its contact time and its actual confirmation. This
protects active catch-up without allowing unbounded no-progress renewals.
A late proof and an eligible timeout claim still race for the
same Court output; fifty DAA is chain time, not an exact wall-clock guarantee.
Winning contacts automatically authorize a separate collector, which gathers
the six queues, pays the ten-KAS pot to the winner's player address, and
returns the seven-KAS operational collateral to the host refund address. This keeps the heavy
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
During the countdown, `o` marks the chosen serve position; the flying ball is `@`.
Serve variation is derived from chain time and player keys, not a random beacon.

## Wallets and node configuration

Create a new wallet only if you do not already have one:

```powershell
.\pong --network testnet-10 keygen --out wallets/alice/wallet.json
.\pong alice --network testnet-10 address
```

Key generation is offline and refuses to overwrite a file. Back up the wallet;
it contains private keys. Share the public player key, and fund the printed fee
address with a sufficiently large mature UTXO. Small UTXOs are not consolidated
automatically. Each player uses their own wallet. `--separate-fee-key` is optional
at key generation; otherwise that wallet's player key also pays its fees.
Winnings and cancelled-match stake refunds arrive at the player address. The
client can spend from either owned key, including when the fee key is separate.

Host/Join use `node.json` if present, otherwise the bundled public Testnet-10
Resolver configuration. Both players must use the same network. Global options
precede the subcommand:

```powershell
.\pong alice --config config/public-node.example.json host GUEST_PUBLIC_KEY
.\pong --endpoint wss://HOST/wrpc/borsh --network testnet-10 probe
```

`probe` checks node/network, synchronization, UTXO index and accepted-chain API
support without selecting a wallet or spending. Native gRPC and Borsh WebSocket
are supported. Private-node example: `config/node.example.json`.

## Game pace and match lifetime

Each player bets five KAS; the winner receives ten KAS, including a response-
timeout winner. There is no client stake selector. Network fees are additional,
and the host separately provides seven KAS of refundable operational collateral.
Mutual cancellation returns each stake; quitting alone is not a refund.

The game has one fixed ball pace: seven DAA per movement tick. Speed-selection code is commented out;
there is no speed prompt or CLI option. All five shot angles remain available.

<!-- Speed selection disabled.
After choosing Host, select **Slow**, **Normal**, or **Fast** ball speed. Fast
matches the previous Slow pace; Normal (default) runs at half Fast, and Slow at
half Normal. Each preset is one quarter of its previous speed. All five shot angles are preserved. Paddle
movement and countdown timing are independent of ball speed. Both clients show
the signed setting in the lobby and on the board.

For direct commands, use `host GUEST_PUBLIC_KEY --ball-speed slow` (or `normal`
and `fast`). `host --json` defaults to Normal when the flag is omitted.
-->

Matches are short, single-run sessions. There is no Resume menu, command,
automatic rejoining, or saved player-session record. Leaving play ends that
client's participation in the match, then returns to its menu with the same
explicit wallet selected. Leaving does not settle the match or refund its deposit.
Pending submission/recovery records are saved and background work shuts down
before another match starts. Setup q/Esc finishes the current operation before
returning; an in-flight funding submission is not silently abandoned.
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
