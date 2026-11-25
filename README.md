# Kaspa Pong

Kaspa Pong is an online, two-player terminal game. Players connect to Kaspa
from their own computers and share a game code to start a match. Argent
covenants enforce gameplay and settle wagers on-chain.

**Alpha software. Avoid funding a game wallet with large amounts of KAS.**

## Build from source

On Windows, install Git, Rust 1.94 or newer with Cargo, the Visual Studio C++
Build Tools, and the Protocol Buffers compiler (`protoc`) on your `PATH`.
[Rust installation](https://doc.rust-lang.org/stable/book/ch01-01-installation.html) ·
[Protocol Buffers releases](https://github.com/protocolbuffers/protobuf/releases)

```powershell
git clone https://github.com/freshair18/kaspa-pong.git
cd kaspa-pong/pong
cargo build --locked --release --bin pong-node --target-dir target
```

Run the client from the `pong/` directory so it can find the included
covenant artifact. Cargo resolves the Argent SDK revision from `Cargo.lock`.
The Windows source launcher, `.\pong`, builds and runs the debug client.
The source-build commands below use the release executable you just built.

### Prebuilt packages

Extract the archive for your platform from [GitHub Releases](https://github.com/freshair18/kaspa-pong/releases).
Run commands from the extracted folder alongside `artifacts/`. In the examples
below, use `.\pong.exe` on Windows or `./pong` on Linux and macOS in place of
`.\target\release\pong-node.exe`. Prebuilt packages run without Cargo or `protoc`.

## Play on mainnet

Pong defaults to mainnet through the public resolver. A local `node.json` can
override the default.
Each player runs the same release or source revision on their own computer.
Create a wallet once. `keygen` saves the private key in the wallet file and
prints the public receiving address. Run `address` any time to display it again.

```powershell
.\target\release\pong-node.exe probe
.\target\release\pong-node.exe keygen --out wallets/player/wallet.json
.\target\release\pong-node.exe player address
```

Use the `address` field that starts with `kaspa:` to fund the wallet on mainnet.
The default wager and queue reserve target are 5 KAS each per player, with
network fees in addition.

On the host's computer, start hosting and send the displayed game code to the guest.

```powershell
.\target\release\pong-node.exe player host
```

On the guest's computer, join with that code.

```powershell
.\target\release\pong-node.exe player join GAME_CODE
```

Both players connect to the same Kaspa network and can use different compatible
nodes. The host plays on the right and the guest on the left. Move with **↑ / ↓**
or **W / S**. Winnings settle into the winner's reusable queue balance.

## Play on Testnet-10

Both players add `--network testnet-10` before the subcommand when creating
wallets, hosting, or joining. Fund each printed `kaspatest:` address with TKAS.
[Kaspa Testnet-10 faucet directory](https://kaspa.aspectron.org/faucets-mining.html#testnet-10)

```powershell
.\target\release\pong-node.exe --network testnet-10 keygen --out wallets/test-player/wallet.json
.\target\release\pong-node.exe test-player --network testnet-10 address
# Host's computer
.\target\release\pong-node.exe test-player --network testnet-10 host
# Guest's computer
.\target\release\pong-node.exe test-player --network testnet-10 join GAME_CODE
```

## Documentation

- [Player guide, controls, and node configuration](pong/README.md)
- [Protocol, wallet reserves, and recovery](pong/docs/reusable-queues.md)
- [Development and build tooling](pong/docs/TOOLING.md)
- [Test suites](pong/docs/TESTING.md)
- [ISC license](pong/LICENSE)
