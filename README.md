# Kaspa Pong

Two-player terminal Pong, with wagers and payouts on Kaspa.

**Kaspa Pong is in alpha. Start on Testnet-10. Mainnet wagers use real KAS. I recommend against transferring large sums of money to the addresses used for the game.**

## Get the game

Check [GitHub Releases](https://github.com/freshair18/kaspa-pong/releases) for your platform’s archive. Open a terminal in the extracted folder. The examples use `.\pong.exe` on Windows. On Linux or macOS, use `./pong`.

<details>
<summary>Build from source</summary>

Install Git, Rust 1.94 or newer, and the Protocol Buffers compiler (`protoc` on your PATH). On Windows, install the Visual Studio C++ Build Tools as well.

```powershell
git clone https://github.com/freshair18/kaspa-pong.git
cd kaspa-pong/pong
cargo build --locked --release --bin pong-node --target-dir target
```

Stay in `pong/` and replace `.\pong.exe` in the examples with `.\target\release\pong-node.exe` on Windows or `./target/release/pong-node` on Linux or macOS.

</details>

## Play a match on Testnet-10

Use the same client release on both computers. Each player creates a wallet once, funds the printed `kaspatest:` address with TKAS, and opens the menu.

```powershell
.\pong.exe --network testnet-10 keygen --out wallets/player/wallet.json
.\pong.exe player --network testnet-10
```

1. The host presses **1** and shares the displayed game code.
2. The guest presses **2**, pastes the code, and presses **Enter**. Review the terms and fee. By default, the offer is approved after five seconds. Press **Enter** to approve early or **Esc** to cancel. Set **Advanced settings → Invitation auto-accept** to **0** for manual approval.
3. Move with **↑ / ↓** or **W / S**. The host plays on the right. On Windows, **M** toggles sound. **Esc** opens the leave prompt. Leaving a live match can cost you the wager.

### Game settings

The host can press **3** before hosting. The guest uses the rules in the game code.

| Setting | Choices |
| --- | --- |
| Speed | Super Fast, Fast (default), Medium, Slow, or **0** for manual speed. Choose 1–9 in manual mode, with 1 the fastest. |
| Ball count | 1–6 on the menu, 3 by default. |
| First to win | 1–9 goals, or **0 Auto** to scale with the ball count. The default match is first to 6. |
| Wager | 5–254 whole KAS per player (TKAS on testnet). The default is 5. |

## Command line

```powershell
.\pong.exe player --network testnet-10 host
.\pong.exe player --network testnet-10 join GAME_CODE
.\pong.exe player --network testnet-10 address
```

Put the wallet name first and flags before the command. Use `--wallet FILE` to select an exact wallet path. `--network mainnet` uses real KAS. The client defaults to mainnet if you omit the flag and have no local `node.json`. Use `--config FILE` for a different node config, `--endpoint ENDPOINT` for a specific RPC node, and `probe` to check the connection. Run `--help` for the full list.

## Funds and recovery

The winner gets both wagers in **Wallet → Reusable queues**. Each player also needs a queue reserve (5 KAS or TKAS by default) and funds for network fees. You can withdraw from or close an idle queue set.

Keep your wallet file private and back it up along with `games/`. Pong uses the queue journal there to resume pending transactions. Open **Wallet → Game status / fee budget** to check recovery.

[ISC license](pong/LICENSE)
