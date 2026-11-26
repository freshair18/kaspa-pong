# Advanced Pong guide

## Connect to a specific Kaspa node

Give Pong the node's RPC address. Replace the example with your node's address. Use `grpc://HOST:PORT` for native gRPC or your node's full `ws://` or `wss://` Borsh RPC URL.

```powershell
.\pong.exe --network mainnet --endpoint grpc://127.0.0.1:16110 probe
.\pong.exe player --network mainnet --endpoint grpc://127.0.0.1:16110
```

`probe` needs no wallet. It reports the connected endpoint and network and checks synchronization, the UTXO index, and full accepted-chain RPC support. Both players must use the same network. Each can connect to a different compatible node.

For a saved setup, put a `node.json` in the folder you run Pong from, or name a different file with `--config FILE`. This example uses one node for both chain observation and transaction submission.

```json
{
  "network": "mainnet",
  "endpoint": "grpc://127.0.0.1:16110",
  "broadcast": { "nodes": 1 }
}
```

```powershell
.\pong.exe player --config my-node.json
```

- Use `fallback_endpoints` for backup nodes on the same network. Use `network_endpoints` for separate network profiles. See the [public-node example](pong/config/public-node.example.json).
- `--network`, `--endpoint`, and `--broadcast-nodes` override the corresponding configuration. The default is up to five submission nodes. Set `--broadcast-nodes 1` to submit only through the current observer.
- In **Advanced settings**, adjust transaction fees, playback buffering, invitation auto-accept, broadcast nodes, or the queue reserve target. The reserve target is saved to your node configuration.

## Recover a queue or game

Pong recovers pending moves, timeouts, and settlements in the background when you reopen the same wallet on the same network. Keep the wallet and its `games/` directory together. Check **Wallet → Game status / fee budget** for progress and increase the allowance there if recovery needs more fees.

- **Recovery needs retained chain history.** Connect to a node that still has the game's history and restart Pong with the same wallet and journal. Another idle set can still be used to play.
- **Game started under an older release.** Use a client that supports that game's covenant version with the matching wallet and journal.

## Consolidate wallet coins

Open **Wallet → Consolidate available wallet coins** to combine mature, unreserved ordinary wallet outputs. Choose **Consolidate ordinary wallet coins only** to keep your reusable sets. Choose **Include idle queue sets** to close eligible idle sets and return their balances to the ordinary wallet. Active, reserved, offered, and pending sets are excluded. Review the total fee before confirming.

## Diagnose slow play

- `target/broadcast.jsonl` records per-node submissions and acknowledgments. Use `--broadcast-log FILE` to choose another path.
- `target/startup-PID.jsonl` records connection and gameplay diagnostics in an interactive terminal. Join `reusable_gameplay_timing` records to the broadcast log by `txid`. With redirected streams, these diagnostics go to stderr.

An acknowledgment means a node received the submission. The `accepted` gameplay event is when the client observed and replayed chain acceptance. The gap includes both inclusion time and observation delay.
