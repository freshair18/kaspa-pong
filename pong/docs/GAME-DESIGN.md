# Pong game design

A live match has one Court output and three Lane outputs per player. The Court contains the ball state, the score, and **both paddles' Y coordinates**. The Lanes hold movement commands that can advance a paddle beyond the position recorded in the Court.

[Startup](STARTUP.md) covers the Lobby-to-Court transaction. See [Invite code](INVITE-CODE.md) for the wire format and [Match closure](GAME-CLOSURE.md) for cancellation, payout, and queue restoration.

## Actors and state

| Actor | What it holds or controls |
| --- | --- |
| Bank | The first output of an idle player's reusable set. The owner can use the set in a match, resize it, or close it. |
| Lane | A player's queue of movement batches, part of the wager backing and reserve, and the set's identity. Lane 0 also holds the paddle checkpoint. |
| Lobby | The guest's reservation, including the players and match terms. Holds half of the guest's wager. |
| Court | The live state shared by both players. Holds half of each wager and commits to both sets. |
| Collector | The result, including the winner and final score. Authorizes payout and reset of both sets. |

The Court stores the following game state.

| Field | Meaning |
| --- | --- |
| Players and sets | Both public keys, queue set IDs, and set generations. |
| Match terms | Wager, ball count, ball speed in DAA per step, and goals to win. |
| Paddle positions | `left_y` and `right_y` in `progress`. These are the center Y coordinates used at each side's last resolved crossing. Both start at 16. |
| Score | Goals for each side and the count of resolved crossings. |
| Ball flights | Each recorded flight's origin DAA, position, direction, slope, and serve time. Opening flights can be derived from the opening epoch until their first crossing is resolved. |
| Next contact | The ball index, receiving side, and crossing DAA, derived from the flights. |

For the same paddle, Lane 0 holds a checkpoint `(y, through)`. Its `y` includes the moves already applied strictly before DAA `through`. The Court's Y can be older than the Lane 0 checkpoint. For example, an early checkpoint can advance the paddle from Y 16 to Y 20 in Lane 0 while the Court still records Y 16. At that side's next completed crossing, the Court records the Y actually used, including any eligible moves still queued.

See [Court and Lane state](../contracts/reusable.ag#L21) and [Rust game state](../src/engine.rs#L161).

## Record a move

1. Spend one of the player's Lanes with the owner's signature.
2. Append a signed displacement to its movement queue. Each Lane can hold four batches. The batch takes effect at the DAA at which Kaspa accepts the move transaction.
3. Recreate that Lane. The Court and both stored paddle coordinates stay as they were.

The client can show movement immediately as a preview. The contact calculation uses accepted moves. See [`Lane.command`](../contracts/reusable.ag#L259).

## Advance a paddle checkpoint

`Court.resolve` consumes the Court and all three Lanes for the side being updated. The other side's Lanes stay in place. The same entry can advance a paddle checkpoint or resolve the next ball crossing.

1. Select a cutoff at or before the next crossing's DAA, and at or after the existing Lane 0 checkpoint.
2. Start with Lane 0's `y`. Merge queued moves from the three Lanes by acceptance DAA. For ties, apply Lane 0, then Lane 1, then Lane 2.
3. Apply up to four groups of moves with distinct timestamps, strictly before the cutoff. Clamp the paddle's center to the court after each batch.
4. Save the new `(y, through)` in Lane 0 and retain the remaining moves. The checkpoint reaches the cutoff once every earlier move is accounted for.

A spend resolves the crossing only if it updates the receiving side and the checkpoint reaches the crossing's DAA. Otherwise it must advance the Lane checkpoint or consume a batch. In that case both Court paddle coordinates and the ball flights stay as they were. See [queue processing and the contact check](../contracts/reusable.ag#L479).

## Resolve the crossing and write the next flight

The board is 58 by 32 cells. The paddles sit at fixed X coordinates and move along Y. The ball's flight gives its position at any game tick, including reflections off the top and bottom walls.

1. Take the earliest scheduled crossing across the active balls. Ties go to the lower ball index.
2. Project that ball to its crossing DAA. Use the receiver's Y calculated from the Lane checkpoint and eligible moves. A move accepted at the crossing DAA stays queued for later.
3. On a hit, reverse the horizontal direction and set the new slope from the hit's offset from the paddle center. On a miss, give the other side a goal and schedule that ball's next serve.
4. If play continues, create a new Court. Replace the receiver's stored Y with the Y used for the crossing, keep the other paddle's stored Y, and record the changed flight and next crossing. The remaining balls continue on their existing flights.
5. If the goal wins the game, create a Collector instead. See [Match closure](GAME-CLOSURE.md) for the winner and timeout paths.

The Rust engine reconstructs these transitions from accepted state. The renderer uses that state to draw the court between updates. See [the contact calculation](../contracts/reusable.ag#L524) and [the matching Rust plan](../src/engine.rs#L498).
