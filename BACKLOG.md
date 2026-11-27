# Pong backlog

## Work I want to do

### Check slow pending transactions sooner

A node can acknowledge a transaction, then drop it from its mempool without telling the client. The case we discussed was a contact proof that appeared to vanish. An opponent's checkpoint replacing it is one possible explanation. This can happen to other transactions too, but a missing proof holds up the next crossing.

Pong journals the signed transaction and reserves its inputs before sending it. If the transaction remains pending after an acknowledgment, the submission worker waits five seconds and tries again. The retry checks the inputs and any pending parents it needs. It doesn't ask whether each peer still has the transaction itself. [Submission loop](pong/src/node/queue_service.rs) · [Retry input checks](pong/src/node/rpc.rs)

Consider a sooner probe for time-sensitive spends. If the transaction is gone and still validates, resend the same signed bytes to peers that can take it. If a checkpoint has landed, rebuild against the new Court. A reorg can change an input's acceptance DAA, so recheck the proof before resending. Keep the journal entry and reservations while the outcome is uncertain.

### Stop parsing conflict IDs from error text

Pong pulls a transaction ID out of the node's rejection message, then looks it up. A wording change can break the first step. One alternative is to ask the rejecting node which transaction spends the exact input. An address-based mempool lookup could work if the client verifies the outpoint, though it adds a round trip and may make the node scan its mempool. The conflict can change before it replies. [Current conflict handling](pong/src/node/rpc/broadcast.rs)

A structured conflict ID would avoid both the text parser and the search for a spender. If the rejection also carries the outpoint, fee, and normalized mass, it could eliminate the separate pricing lookup. If we pursue contact-over-checkpoint RBF, there's a further question: what transaction data would we need to establish which kind of spend it was?

### Make mainnet play feel more responsive

Reduce input lag, freezes, rewinds, and stretches where the paddle feels unresponsive. Use feedback from mainnet players to tune the DAA render delay, choose the default ball speed and ball count, and find out which stalls spoil play. Use logs to check whether a reported hitch came from delayed input, a gap in chain updates, or a playback correction. The buffer should smooth short gaps without making normal play feel late.

### Make wallet creation easier to find

A new player currently needs to know about `keygen` before they can get to the menu. Put **Create a wallet** and **Open an existing wallet** at the start. Show the network and receiving address before funding, and make it easy to reopen the same wallet. Keep the explicit CLI path. Watch for accidental duplicate wallets, overwrites, and mainnet/testnet confusion.

### Make controls and menus easier to learn

Players missed that they could hold a movement key. Even after finding it, they were unsure when to hold and when to tap. Explain the current controls in the game. Holding a direction can group repeated steps into fewer batches. A burst of separate taps can leave all three movement lanes waiting on transactions, which makes the paddle feel less responsive.

Review option names and grouping for queue creation, consolidation, game and advanced settings, and wallet actions. Give players a short Help option they can find from the menu. A single relevant tip on the completion screen could cover something they just encountered. For example, explain when to hold a movement key, or what happens to an idle queue set when it is included in consolidation.

### Include reusable queue setup in the main menu fee total

The main menu shows total fees, but it leaves out the network fees paid to create reusable queue sets. Include those fees when the sets are created, and count each setup fee only once even if the set is used in several matches.

## Ideas to investigate

### Let a contact proof replace an optional checkpoint

A checkpoint makes space in a player's move queues. A contact proof advances the ball. If they compete for the same Court, should the contact get priority? The proposed RBF exception would let a valid bounce-or-goal proof replace an opponent's optional checkpoint. Pong's client would give checkpoints no reciprocal permission. A modified client could still submit any checkpoint the covenant and node accept. Today the ordinary RBF path covers known stale or equivalent spends, with separate treatment for timeouts. [Current replacement rules](pong/src/node/rpc/broadcast.rs)

Pong already works out what an opponent's **accepted** Court spend did. The chain tracker decodes the side and cutoff from `Court.resolve`, replays it against the current Court and lanes, and checks the successor. The two cases share the same entry point. A spend can name the contact deadline and still only process part of the move queue. [Accepted replay](pong/src/node/queue_tracker.rs#L255) · [Contact check](pong/contracts/reusable.ag#L508)

That replay isn't used today to decide whether to replace an opponent's **pending** checkpoint. If the rule goes ahead, one option is to fetch the conflicting transaction from the rejecting node, match it to the current Court and lane inputs, and reuse the same replay logic to check what it would actually do. Skip ambiguous cases. At most one automatic fee bump per contact across all peers would keep a stall from turning into a bidding war. First check how often the situation occurs and whether earlier probing and rebroadcasting are enough.

### One press, one transaction

One press moves the paddle one cell and sends one transaction. No held moves, no autorepeat.

The original design gave each move its own acceptance time on Kaspa. That matters when a ball reaches the paddle between two moves. If one move is accepted before the crossing and the other afterward, only the first counts. A batch gives both steps the same timestamp. Separate transactions let the chain decide which individual moves made it in time, without the client choosing which presses to group.

A low-rate trial is possible with today's contract. At normal play speed, each pending move occupies a lane, and a contact proof needs the receiver's lanes with accepted timestamps. [More lanes and a smaller Court](#more-lanes-and-a-smaller-court) would help absorb bursts, but every extra output adds funding and storage costs.

Sustaining that pace likely needs changes in both Argent and Kaspa. [Sharing the Lane template](#carry-one-template-for-several-inputs) would keep each extra input from carrying another copy of the code. [P2MR or reliable same-block composition](#reveal-only-the-covenant-branch-being-used) could make routine spends smaller or let setup and complex actions fit into several transactions without a separate wait for each one.

[DAA expiry](#give-transactions-a-daa-expiry) could let a contact resolve a closed bank of moves while another bank takes new presses. Compare a limited trial with batching on mainnet, including what players feel, how often contacts wait, and what a match costs.

### More lanes and a smaller Court

Extra lanes let more moves be in flight. Extra entries in each lane give already accepted moves more room before a contact. Each new lane is another output to fund. Splitting the same reserve into smaller outputs can raise storage mass, especially when creating the set.

Options include funding each lane more generously or creating the set in stages. Staging can ease the cost of an individual transaction while making setup slower or more expensive overall. It also needs a way to tie the outputs to one set and keep incomplete sets out of play. Same-block composition would make that option more attractive.

Every extra lane also adds work and bytes to contacts. One idea is to have each Lane check its own replacement and give Court a compact merge proof. Court still needs to check that all eligible moves appear once, in order, and that later moves stay queued. Price creation, moves, and contacts at realistic balances before picking a new lane count.

## Set aside for now

### Multiple live chain observers

Pong broadcasts to several nodes and follows accepted state through one. A second observer might see a Court spend sooner, but that benefit hasn't been shown. A read-only comparison could measure the difference without changing how the client plays.

Even two live observers would take substantial work and leave plenty of room for error. They can disagree on the tip or on a transaction's acceptance DAA after a reorg. Acting on the first reply could rewind the court or build a proof against stale inputs. The design would need one verified chain cursor and clear rules for changing sources during lag, reconnects, and disagreement. Full chain reads and replay also cost bandwidth and CPU. Keep this set aside unless the comparison shows a worthwhile gain.

### Resolve two contacts in one transaction

A combined proof could save an acceptance wait when two balls reach the same receiver close together. That case looks relatively rare, and the change would make the Court more complex. Ordinary contacts could end up paying for a heavier script.

A combined proof would have to handle moves between crossings, tied contacts, a winning first goal, and the next player's timeout window. Revisit it if mainnet logs show that this particular backlog happens often enough to justify heavier contact proofs.

## Fair play

### Checkpoints can starve contact proofs

An opponent's optional checkpoint can compete with a contact proof for the same Court. If this happens repeatedly, the receiver may have to rebuild their proof against several successive Courts while play stalls. The working assumption is that sustaining this with accepted checkpoints eventually costs the attacker more than the wager. That's an economic deterrent. A short disruption can still spoil a match, and a checkpoint that only displaces a proof in a mempool may never incur an on-chain fee.

A [contact-over-checkpoint replacement rule](#let-a-contact-proof-replace-an-optional-checkpoint) could help the honest client recover from a particular mempool conflict. The opponent can ignore that policy and keep submitting valid checkpoints. An enforceable protection would have to change what the covenant allows. Two possibilities are a cutoff for optional checkpoints using [DAA expiry](#give-transactions-a-daa-expiry), or a way to compact lanes without spending Court. Either would need to preserve the receiver's ability to catch up on queued moves.

### Late contact proofs can throw off the opponent's client

A delayed proof can leave the other client drawing a provisional trajectory. When the proof finally lands, the ball may change course or appear somewhere the player wasn't preparing for. Deliberate delays could give a player an advantage over a few rallies. How reliably that works is open, and ordinary network delays can look the same.

Compare the ball the opponent actually saw with the accepted correction. Consider making unresolved contacts clearer and smoothing the change when play resumes. A hard proof deadline could limit the window, but it would also leave less room for a player whose transaction is genuinely slow.

## Waiting for Kaspa or Argent support

### Give transactions a DAA expiry

The covenant can say when a spend becomes eligible. An expiry would also give it an upper bound. Kaspa could enforce a transaction expiry field that the covenant can constrain, or expose a covenant primitive for the upper bound. Consensus has to enforce it, since signed bytes can be kept and sent later. The change would need to specify the exact acceptance boundary and how to handle reorgs.

- **Contact deadlines.** Make the proof expire at the same boundary that makes a timeout eligible. Today a late save can still race the timeout for the Court.
- **Movement queues.** Rotate between two banks of lanes. Commands for one bank expire when it closes, so a contact can resolve a fixed set of moves while the other bank takes new input. Both banks need funding and a shared rule for when they switch. Before resending a move elsewhere, check whether it already landed before expiry.

### Reveal only the covenant branch being used

Pay to Merkle Root (P2MR) lets an output commit to a tree of scripts. A spend shows its chosen script and a proof that it belongs to the tree. A routine contact could carry the resolve code by itself, and a lane move could leave restoration code out. That gives us room to add specialized actions without ordinary transactions carrying every branch and running up transient mass. Measure the savings where transaction size actually sets the fee. [Kaspa P2MR proposal](https://github.com/kaspanet/kips/pull/37)

A covenant could approximate this in two steps. The first verifies the branch and creates an output restricted to that action. The second spends it and executes the branch. If Kaspa allows the parent and child to be **accepted in the same block**, the second step could avoid a separate acceptance wait. Check how the pair is relayed, whether the child's timing checks work, and what happens if the selector lands by itself. Compare the total cost and delay of both transactions. [Kaspa transaction ordering](https://github.com/kaspanet/kips/blob/master/kip-0015.md)

### Carry one template for several inputs

A contact spends three Lane inputs that use the same code. Carrying the template once could reduce the witness. The same idea could let collection restore both Bank outputs directly and avoid separate restoration spends.

The hard part is designing this as a general Argent feature. It should work for any mix of actors and covenant groups. Changing the order or indices of inputs and outputs must preserve the meaning of each reference. Each input must authenticate the shared template against its own commitment and enforce its own entry, successor, and state checks. A template reference should identify the code being shared without giving one participant authority over another's checks.

How Argent could support this in general is an open design problem.

### Switch to an Argent release

Pong pins a Git revision through `Cargo.lock`. When a release includes the compiler and runtime features we use, try that release and compare the generated covenant artifact. If the scripts change, check what happens to funded queues and games before making it the default.
