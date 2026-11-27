# How Pong works on Kaspa

Each player runs a client that follows Kaspa and draws the game locally. The client can show where the ball is headed between transactions, but the accepted covenant outputs decide which moves count, whether a ball hit the paddle, and who gets paid. In Argent, each actor defines the state an output carries and how a transaction can spend it.

## The actors

| Actor | What it represents |
| --- | --- |
| **Bank** | The first output of an idle player's reusable set. It lets the owner put the set into a game, resize it, or close it. |
| **Lane** | One of the three outputs in a player's set. During play, each lane holds a queue of paddle moves and part of the player's funds. |
| **Lobby** | A guest's funded reservation. It records the players and match terms while waiting for the host to start. |
| **Court** | The live match. It holds the score, the ball trajectories, the next contact, and the part of the wager moved out of the players' sets. |
| **Collector** | A finished match with a winner. It distributes the pot and resets both players' queues. |

## From game code to match

The host shares a code with their public key and the terms of the match. When the guest joins, their transaction reserves their queue set and creates the Lobby. The host starts by bringing in their own set and spending the Lobby into a Court. Each player puts half their wager in the Court and leaves the other half in their lanes as backing.

## What happens when the ball reaches a paddle

A move adds a batch of steps to one of the player's lanes. Three lanes let the player have several moves in flight. Which moves count at a contact depends on when Kaspa accepted them.

When a ball reaches a paddle, the receiving player's client submits a contact proof. It spends the Court and that player's three lanes, applies the moves accepted before the crossing, and works out the bounce or goal. The other player's lanes are independent of that proof. With several balls, their flights share the same Court, so each proof resolves the next crossing in order. The result of one contact is the starting point for the next.

## How a match ends

A winning goal turns the Court into a Collector. If the receiving player leaves a contact unresolved long enough, a timeout spend can also create a Collector and award the match to the other player. Collection pays the pot into the winner's set and clears the old moves from both sets. A follow-up spend restores each set's first output to a Bank so it can be used again.

The [Argent covenant](../contracts/reusable.ag) defines the on-chain rules. The [Rust engine](../src/engine.rs) follows the same rules when it reconstructs and draws the game.
