# First working alpha — v0.1.0-alpha.1

Baseline recorded on 2026-10-21. The annotated Git tag preserves the working
Pong implementation before experiments with different input/resolve protocols.
The Cargo package version remains 0.1.0; the Git tag identifies this alpha.

## Included implementation

- Covenant-enforced two-player Pong, with real-node Host/Join/Resume clients.
- Explicit named-wallet selection, ASCII play, local paddle preview, chain-clock
  projection, durable state, transaction/reorg logs and personal fee accounting.
- One Player queue UTXO per side, with up to four commands in each queue.
- One pending transaction per player. Queue updates depend on their previous
  output's actual acceptance time.
- Resolves consume the Court and both Player queues. Normal resolver duty
  alternates; timeout takeover continues the game and does not award a win.
- No routine empty resolves. The projected result alone does not prove on-chain
  settlement or deposit refund.
- Pinned SDK dependencies, compiled current/legacy artifacts, local development
  tooling and the isolated private-consensus demo.

Alpha covenant artifact ID:
`555d1eebb28b9589c426f829257ed7baa04df90a3d4f2432a3161aa0f04a7228`.

## Verification boundary and limitations

The current implementation has passed the previously recorded focused client
tests and isolated native-node integration, including Host/Join/Resume and
accepted movements. Tooling checks reproduced the shipped artifact ID. This
release marker did not trigger any rebuild or test rerun.

Public play has exposed substantial input latency. Local paddle preview is not
an authoritative collision or score update. Queue dependencies, shared-input
resolve races and the idle-goal settlement limitation remain in this baseline.
There is no claim of production readiness or a completed visual UI verification.

Parallel queues, independent input slots, sender-owned resolution, prebuilt
dependent transactions and timeout victories have been discussed but are not
implemented in this alpha.

## Starting experiments

Create each experiment from the preserved tag, for example:

```powershell
git switch -c codex/parallel-queues v0.1.0-alpha.1
```

Keep the alpha tag fixed. This is historical source, not a supported alternate
client. Active development supports only current rules; rule changes replace the
current artifact and require fresh games. The Git
snapshot excludes private wallets, live sessions/caches/logs, build outputs and
the independent sibling Argent checkout.
