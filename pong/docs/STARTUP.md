# How a match starts

```text
Host saves an offer locally
    → Guest reservation creates a Lobby (guest's set is active)
    → Host start spends the Lobby into a Court (both sets are active)
```

Let `W` be the wager per player. An idle set consists of one Bank and two Lanes. See [Game Design](GAME-DESIGN.md) for the actors.

## 1. Prepare an offer

1. Load the queue journal for the selected wallet and network. Reconcile pending signed spends and queue outputs against the accepted chain.
2. Select an idle host set, or create one if needed.
3. Save a local offer containing the invite identity and the host set's covenant ID and generation.
4. Share the [invite code](INVITE-CODE.md). The host's Bank and Lanes remain unspent.

## 2. Guest reservation

| | Structure |
| --- | --- |
| Signed by | Guest |
| Consumes | Guest's Bank and two Lanes, plus ordinary funding inputs if needed |
| Creates | A Lobby holding `W / 2` and three guest Lanes with the other `W / 2` marked as wager backing |
| Lobby state | Guest and host public keys, guest pool and generation, match terms |
| Payload | Invite identity, guest set descriptor, and funded amount |

The host matches the accepted reservation's key and tag to a saved offer. It validates the spend and checks that the outputs form the expected Lobby and guest set. The reservation transaction ID identifies the local session. The Lobby's covenant ID identifies the game on-chain.

## 3. Host start

| | Structure |
| --- | --- |
| Signed by | Host |
| Consumes | The Lobby, host's Bank and two Lanes, plus ordinary funding inputs if needed |
| Creates | A Court holding `W` and three host Lanes with `W / 2` marked as wager backing |
| Court state | Both sets and their generations, player keys, match terms, opening score and ball state |
| Payload | Host set descriptor, tied by the covenant to the set actually spent |

The guest Lanes are carried forward as they are. After accepted replay, both clients have the Court and both sets of Lanes. The opening serve clock starts at the Court's acceptance DAA plus 25. The game is live once the start is accepted.

See [match setup](../src/queue_launch.rs), [reservation and start payloads](../src/node/queue_codes.rs), [Lobby covenant](../contracts/reusable.ag#L291), [accepted replay](../src/node/queue_tracker.rs).
