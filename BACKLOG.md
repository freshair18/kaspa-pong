# Pong backlog

## Contact delays

### Try two chain observers

Broadcast already reaches several nodes, but Pong follows accepted state through one observer at a time. A second observer could tell us sooner that a Court spend landed, especially when the first node is behind. Start by measuring when each node reports the same acceptance. If it helps, have both follow the same verified chain cursor and check each update against the local state before applying it. Two nodes can disagree during a reorg, and following both adds bandwidth and replay work.

### Rebroadcast a contact proof that was evicted

A node can acknowledge a signed contact proof and later evict it when an opponent's checkpoint takes its place in that node's mempool. If the same proof still validates against the current inputs, try sending the same signed transaction sooner, especially to a peer that hasn't accepted the checkpoint. A node that still holds the checkpoint can reject it again. Once the checkpoint lands on-chain, rebuild against the successor Court. The tricky part is distinguishing eviction from a spend or a change in input acceptance timing. Keep the existing pending record and reservations while checking.

### Let a contact proof replace an optional checkpoint

This would be a new RBF rule. When a node names a conflict, fetch the transaction and verify that it is the opponent's optional checkpoint spending the same Court. Then try at most one fee-capped replacement per unresolved contact. Reserve that permission for a proof of the actual bounce or goal. A checkpoint gets no corresponding permission to replace a valid contact proof. Checkpointing helps free queue space, but a contact is what moves the game forward. The main risks are misclassifying a transaction and paying for a replacement race that can keep repeating across peers.

### Resolve two due contacts in one transaction

When two balls reach the same player's side close together, the second proof may have to wait for the first Court spend to clear. Try proving two already-due contacts for the same receiver in one transaction. The second contact must use the ball and paddle state left by the first. Check tied crossings, intervening moves, a winning first goal, and the next player's timeout window. Measure the resulting transaction mass before raising the limit.

## Conflict details from Kaspa

### Read the conflict ID from a structured error

Replace the English-text parser when Kaspa exposes the conflicting transaction ID as a field. This would let node error messages change without breaking Pong. Check how nodes advertise support over gRPC and wRPC, and keep a compatibility path while older nodes are in use.

### Get conflict details with the rejection

If Kaspa returns the fee, normalized mass, and outpoint it used for the conflict, use those details for pricing instead of making another RPC call. This saves a round trip and avoids a lookup racing with mempool eviction. Fee and mass are enough to price an RBF attempt. Classifying the conflict as an opponent's checkpoint still needs the actual transaction or enough verified data to make that distinction. An extended text response could save the round trip before a structured response is available, at the cost of more dependence on the wording.

### Look up the spender instead of parsing the error

As an alternative for older nodes, query the rejecting node's mempool and find the transaction spending the exact Court or lane outpoint. This avoids relying on the English message. It costs an extra RPC, and an address-based lookup may scan a large mempool. The result can also change between rejection and lookup. If we can't identify the spender unambiguously, wait for fresh chain evidence.

## Contract ideas that depend on upstream work

### Give transactions a DAA expiry

If Kaspa adds a consensus-enforced expiry, give selected spends a real closing time. A late contact proof could then stop competing with a timeout claim. Expiring moves would also make it possible to alternate between two sets of command lanes. One could close to new moves while the other receives them. Check how expiry behaves across reorgs and verify whether a move was accepted before expiry before submitting it again on another lane.

### Reveal only the covenant branch being used

Native P2MR could let a contact reveal the Court's resolve branch without also carrying timeout code. Lane spends could likewise reveal the branch they use. If native support stays out of reach, revisit a covenant-based version with a transaction that selects the branch and a dependent spend that runs it. That is only attractive if dependent transactions in successive blocks are reliable enough to keep the extra step off the critical path. Compare total fees and time to acceptance, and check that the next output still commits to the right rules. Smaller scripts may leave fees unchanged when storage mass dominates. [Kaspa P2MR proposal](https://github.com/kaspanet/kips/pull/37)

### Carry a shared template once

If Argent supports sharing template bytes across inputs, carry one Lane template for the three lane inputs in a contact. The same idea could let collection restore both Bank outputs directly, saving the later restoration spends. Each input still needs to verify that the shared bytes match its own template commitment. This saves repeated witness bytes. The checks inside the Court and any storage-mass costs still need separate work.

### Revisit queue size and Court code

More lanes could keep paddle input moving while earlier transactions are still pending. Shorter per-lane queues might also make it easier to apply the relevant moves at a contact. Every extra lane adds another output to fund and another input for Court to check. Before raising the count, price a full match with realistic balances. One idea worth testing is to let each Lane check its own replacement and have Court check that the combined move history is complete and in the right order.

## Measurements worth keeping

### Separate chain inclusion from the delay we see

Record submission, the first accepted-chain notification from each observer, replay completion, and the first frame that shows the change. Keep track of batches held before submission too. Use this to compare the 20 and 60 DAA movement holds and to tell whether a second observer improves play. An RPC acknowledgment records receipt. A block timestamp can help investigate, but it doesn't give an exact wall-clock acceptance time.

### Check how playback handles goals and rewinds

We saw jumps near goals and stretches where playback slowed to refill its buffer. Record what was actually drawn before and after a correction, including goals and serves, and measure how long the clock spends catching up or refilling. That should show whether a jump came from a real change in accepted game state or from how we replayed it. Test with real games before changing the buffer again.
