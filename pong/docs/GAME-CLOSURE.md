# How a match closes

```text
Lobby ── guest cancellation ──────► guest set reset ──► Bank restored

Court ── winning contact ──┐
      ── response timeout ─┴─────► Collector ──► payout and both sets reset
                                                         ├─► guest Bank restored
                                                         └─► host Bank restored
```

Each arrow that changes an actor is an accepted chain transition. See [Game Design](GAME-DESIGN.md) for the actor roles.

## Cancel a Lobby

1. Before the guest reservation, withdrawing the host offer releases the local offer reservation.
2. After the guest reservation, `Lobby.cancel` takes the guest's signature and spends the Lobby plus the guest's three Lanes.
3. The replacement Lanes return the Lobby funds to the guest's set, subject to the reserve and fee limits. The primary Lane is marked for Bank restoration.

A host start and guest cancellation compete for the same Lobby. Whichever spend is accepted determines whether the game starts or the guest gets their funds back.

## Decide the result

| Path | Checks | Inputs | Output |
| --- | --- | --- | --- |
| Winning contact | The receiver's proof brings a player to the winning score | Court and receiver's three Lanes | Collector with the final score and winner, plus replacement receiver Lanes |
| Response timeout | Current DAA reaches `max(contact deadline, response anchor) + 110` | Court | Collector with the existing score and the non-receiving player as winner |

The response anchor is the relevant Court's acceptance DAA or the last qualifying advance of the receiver's move queue. A valid late contact and a timeout can still compete for the Court. Closing the client leaves the on-chain state and response window in place.

## Collect and restore

1. **Collect.** Spend the Collector and all six Lanes. The winning set keeps its own remaining half-wager and receives the Collector balance plus the losing set's remaining half-wager. Deduct any fee paid from the winner's set. Clear both move queues and advance both set generations.
2. **Restore each Bank.** Spend each reset set's primary Lane into a Bank with the same owner and set identity. Once a restoration is accepted and verified, that set can be used again.

The tracker records the result when the Collector is accepted and marks the game `Finished` when collection is accepted. Bank restoration is tracked separately for each set. The queue service resumes pending steps from the journal and follows accepted-chain changes after a reorg.

See [cancellation](../contracts/reusable.ag#L360), [timeout and collection](../contracts/reusable.ag#L594), [accepted stages](../src/node/queue_tracker.rs), [recovery worker](../src/node/queue_service.rs#L2731).
