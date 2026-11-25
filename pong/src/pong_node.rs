//! Wallet-selected node client. See README.md.
mod queue_launch;

#[cfg(test)]
mod broadcast_config_tests {
    use super::*;

    #[test]
    fn mainnet_is_the_default_and_testnet_uses_its_own_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("node.json");
        for command in ["host", "address", "probe"] {
            let options = Options::try_parse_from(["pong-node", command]).unwrap();
            let config = selected_config(&options, &local).unwrap();
            assert_eq!(config.network, "mainnet");
            assert_eq!(config.endpoint, "resolver");
            assert!(config.fallback_endpoints.is_empty());
        }

        let options = Options::try_parse_from(["pong-node", "--network", "testnet-10", "host"]).unwrap();
        let config = selected_config(&options, &local).unwrap();
        assert_eq!(config.network, "testnet-10");
        assert_eq!(config.endpoint, "resolver");
        assert_eq!(config.fallback_endpoints.len(), 2);

        node::save(
            &local,
            &serde_json::json!({"network": "testnet-10", "endpoint": "resolver",
            "fallback_endpoints": config.fallback_endpoints}),
        )
        .unwrap();
        let options = Options::try_parse_from(["pong-node", "address"]).unwrap();
        assert_eq!(selected_config(&options, &local).unwrap().network, "testnet-10");
        let options = Options::try_parse_from(["pong-node", "--network", "mainnet", "address"]).unwrap();
        let config = selected_config(&options, &local).unwrap();
        assert_eq!(config.network, "mainnet");
        assert!(config.fallback_endpoints.is_empty());
    }

    #[test]
    fn normal_configuration_uses_five_rpc_targets_and_explicit_control_uses_one() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("node.json");
        let options = Options::try_parse_from(["pong-node", "host"]).unwrap();
        let config = selected_config(&options, &local).unwrap();
        assert_eq!(config.broadcast.nodes, 5);
        let control = Options::try_parse_from(["pong-node", "--broadcast-nodes", "1", "host"]).unwrap();
        assert_eq!(selected_config(&control, &local).unwrap().broadcast.nodes, 1);
        assert!(Options::try_parse_from(["pong-node", "--broadcast-nodes", "9", "host"]).is_err());
        assert!(Options::try_parse_from(["pong-node", "--p2p-peers", "100", "host"]).is_err());
    }
}
use clap::{Parser, Subcommand};
use engine::*;
use kaspa_pong::{engine, menu, network_ui, node};
use node::{Config, rpc::Node};
use std::{io::IsTerminal, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(about = "Pong on a configurable Kaspa gRPC/Borsh node. Select a wallet with NAME, --player NAME or --wallet FILE.")]
struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    endpoint: Option<String>,
    /// Select the Kaspa network (defaults to the configured network, otherwise mainnet).
    #[arg(long)]
    network: Option<String>,
    /// Total RPC submission targets, including the observer (default 5; 1 disables fan-out).
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..=8))]
    broadcast_nodes: Option<u16>,
    /// Append per-node submission timings to this JSONL file.
    #[arg(long, global = true)]
    broadcast_log: Option<PathBuf>,
    /// Explicit local wallet file containing the player key.
    #[arg(long)]
    wallet: Option<PathBuf>,
    /// Explicit wallet name: wallets/NAME/wallet.json (or existing NAME.wallet.json).
    #[arg(long, conflicts_with = "wallet")]
    player: Option<String>,
    /// Show the gray accepted-position paddle markers for diagnostics.
    #[arg(long, global = true)]
    debug_paddles: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Create a fresh game code, wait for the guest's on-chain Lobby, then play.
    Host {
        #[arg(long)]
        json: bool,
    },
    /// Use a host game code to create the Lobby, then play.
    Join {
        code: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Manage persistent reusable queue sets.
    Queues,
    /// Rewrite the current queue journal and optionally run automatic recovery.
    RecoverQueues {
        /// Seconds to follow the chain and attempt existing recovery actions.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=600))]
        seconds: u64,
    },
    /// Manually merge available ordinary wallet coins.
    Consolidate,
    /// Show automatic recovery status and adjust the game fee allowance.
    Resume,
    /// Generate a local wallet and print its public keys/addresses. No node needed.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Read-only node/transport and accepted-chain Full API compatibility check.
    Probe,
    /// Check RPC broadcast connections without signing or sending funds.
    BroadcastProbe {
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=300))]
        wait_secs: u64,
    },
    /// Print the public player key and receiving address. Does not connect or spend.
    Address,
    /// Send sompi from the selected wallet to a Kaspa address.
    Send { address: String, amount_sompi: u64 },
    #[command(skip)]
    MenuBroadcast { nodes: usize },
    #[command(skip)]
    MenuReserveTarget,
    #[command(skip)]
    MenuSend { address: String, amount_sompi: Option<u64>, fee: node::wallet::SendFee },
}
fn keys(wallet: Option<&std::path::Path>) -> Result<(secp256k1::Keypair, secp256k1::Keypair)> {
    let wallet = node::wallet::Wallet::load(wallet.ok_or("select an explicit wallet with NAME, --player NAME or --wallet FILE")?)?;
    Ok((wallet.player, wallet.player))
}
const CLIENT_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<()> {
    // Windows' 1 MiB entry stack leaves too little headroom for nested debug
    // gameplay polls and covenant verification. Keep the entire client lifecycle
    // on this thread, including runtime polling and terminal cleanup.
    let client = std::thread::Builder::new()
        .name("pong-client".into())
        .stack_size(CLIENT_STACK_BYTES)
        .spawn(|| client_main().map_err(|error| error.to_string()))?;
    match client.join() {
        Ok(result) => result.map_err(Into::into),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn client_main() -> Result<()> {
    // A leading wallet name is shorthand for --player NAME, not a court side.
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()).is_some_and(|arg| {
        !arg.starts_with('-')
            && !matches!(
                arg,
                "host"
                    | "join"
                    | "keygen"
                    | "probe"
                    | "broadcast-probe"
                    | "address"
                    | "send"
                    | "queues"
                    | "recover-queues"
                    | "consolidate"
                    | "resume"
                    | "help"
            )
    }) {
        args.insert(1, "--player".into());
    }
    let mut o = Options::parse_from(args);
    let local_config = std::path::Path::new("node.json");
    network_ui::set_debug_paddles(o.debug_paddles);
    if let Some(name) = &o.player {
        o.wallet = Some(menu::named_wallet(std::path::Path::new("."), name)?);
    }
    // Reject missing identity before any menu, session/config lookup, wallet
    // loading or RPC. The read-only node probe uses no key.
    let needs_wallet = !matches!(&o.command, Some(Command::Keygen { .. } | Command::Probe | Command::BroadcastProbe { .. }));
    if needs_wallet && o.wallet.is_none() {
        return Err("select an explicit wallet before starting Pong.\nUsage: .\\pong WALLET_NAME [host|join]\n   or: .\\pong --wallet FILE [host|join]\nWallets are never selected from defaults, environment variables or saved sessions.".into());
    }
    if let Some(path) = &o.wallet {
        eprintln!("Selected wallet: {}\nHost plays RIGHT; Join plays LEFT. Wallet names select identity.", path.display());
    }
    if let Some(Command::RecoverQueues { seconds }) = &o.command {
        let config = selected_config(&o, local_config)?;
        let (player, payer) = keys(o.wallet.as_deref())?;
        let _signer_locks = node::reservations::signer_locks(&config.network, &player, &payer)?;
        node::queue_service::Service::rewrite_saved_journal(&config, &player)?;
        if *seconds > 0 {
            let runtime = tokio::runtime::Runtime::new()?;
            let connection = node::rpc::startup::Connection::default();
            runtime.block_on(async {
                let _fees = node::fee_policy::start(connection.clone(), config.clone());
                let service = node::queue_service::Service::open(&config, player, connection)?;
                tokio::time::sleep(Duration::from_secs(*seconds)).await;
                service.edit(|_| Ok(())).await?;
                let view = service.snapshot();
                println!(
                    "{}",
                    serde_json::json!({
                        "status": view.status,
                        "available": node::queue_service::Service::available(&view).len(),
                        "pending": view.pending.iter().map(|p| &p.purpose).collect::<Vec<_>>(),
                        "games": view.sessions.iter().filter(|s| !s.finished || s.label == "Recovery needs retained chain history")
                            .map(|s| (s.id, &s.label)).collect::<Vec<_>>(),
                    })
                );
                Ok::<_, Box<dyn std::error::Error>>(())
            })?;
            runtime.shutdown_timeout(Duration::from_secs(1));
        }
        return Ok(());
    }
    let menu_mode =
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && returns_to_menu(o.command.as_ref(), o.wallet.is_some());
    let mut next_command = o.command.take();
    let mut menu_error = None;
    let mut menu_player = None;
    let mut menu_lookup = None;
    // Menu preferences persist across matches, resetting on every client start.
    let mut settings = menu::GameSettings::default();
    let mut _signer_locks = Vec::new();
    let mut signer_network = None;
    // Transport tasks live on this runtime across every menu and match. Each
    // match still explicitly drains its own workers before returning.
    let runtime = tokio::runtime::Runtime::new()?;
    let mut shared = node::rpc::startup::Connection::default();
    let mut fee_worker = None;
    let mut fee_network = None;
    let outcome = (|| -> Result<()> {
        if menu_mode
            || matches!(
                &next_command,
                Some(
                    Command::Host { .. }
                        | Command::Join { .. }
                        | Command::Queues
                        | Command::Consolidate
                        | Command::Resume
                        | Command::Send { .. }
                        | Command::MenuSend { .. }
                )
            )
        {
            let (player, payer) = keys(o.wallet.as_deref())?;
            if menu_mode {
                let menu_config = selected_config(&o, local_config)?;
                let identity = node::wallet::public_identity(&menu_config.network, &player)?;
                menu_player = Some(menu::MenuIdentity {
                    wallet_name: o
                        .wallet
                        .as_ref()
                        .map(|path| {
                            let name = if path.file_name().is_some_and(|name| name == "wallet.json") {
                                path.parent().and_then(|parent| parent.file_name())
                            } else {
                                path.file_stem()
                            };
                            name.map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "Selected wallet".into())
                        })
                        .unwrap_or_else(|| "Selected wallet".into()),
                    public_key: player.x_only_public_key().0.to_string(),
                    address: identity["address"].as_str().ok_or("wallet address unavailable")?.to_owned(),
                    network: menu_config.network.clone(),
                    balance: std::sync::Arc::new(std::sync::Mutex::new(menu::Balance::Loading)),
                });
                menu_lookup = Some((menu_config, pubkey(&player)));
            }
            let mut c = selected_config(&o, local_config)?;
            c.game_fee_cap = true;
            c.fee_sompi = 50_000_000;
            _signer_locks = node::reservations::signer_locks(&c.network, &player, &payer)?;
            signer_network = Some(c.network.clone());
            let _entered = runtime.enter();
            shared.warm(&c);
            fee_worker = Some(node::fee_policy::start(shared.clone(), c.clone()));
            fee_network = Some(c.network.clone());
            let _ = node::queue_service::Service::open(&c, player, shared.clone())?;
        }
        loop {
            let balance_worker = if next_command.is_none() {
                if let (Some(player), Some((config, public))) = (&menu_player, &menu_lookup) {
                    if let Ok(mut state) = player.balance.lock() {
                        *state = menu::Balance::Loading;
                    }
                    let state = player.balance.clone();
                    let config = config.clone();
                    let public = public.clone();
                    let mut connection = shared.clone();
                    Some(runtime.spawn(async move {
                        loop {
                            let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                                let node = connection.get(&config).await.map_err(|e| e.to_string())?;
                                node.utxos(&[p2pk(&public)]).await.map_err(|e| e.to_string())
                            })
                            .await;
                            if let Ok(mut balance) = state.lock() {
                                *balance = match result {
                                    Ok(Ok(coins)) => menu::Balance::Coins(coins),
                                    _ => menu::Balance::Unavailable,
                                };
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }))
                } else {
                    None
                }
            } else {
                None
            };
            let broadcast_nodes = o.broadcast_nodes.map(usize::from).unwrap_or_else(|| {
                menu_lookup.as_ref().map(|(c, _)| c.broadcast.nodes).unwrap_or_else(|| node::rpc::broadcast::Settings::default().nodes)
            });
            let selected =
                prompted_command(next_command.take(), menu_error.as_deref(), menu_player.as_ref(), &mut settings, broadcast_nodes);
            if let Some(worker) = balance_worker {
                worker.abort();
            }
            let selected = selected?;
            let command_received = std::time::Instant::now();
            let Some(command) = selected else {
                return Ok(());
            };
            match &command {
                Command::Host { .. } => node::rpc::startup::phase("host_start", command_received),
                Command::Join { .. } => node::rpc::startup::phase("join_start", command_received),
                _ => {}
            }
            if let Command::MenuBroadcast { nodes } = command {
                shared.set_broadcast_nodes(nodes)?;
                o.broadcast_nodes = Some(nodes as u16);
                if let Some((config, _)) = &mut menu_lookup {
                    config.broadcast.nodes = nodes;
                }
                let config = selected_config(&o, local_config)?;
                let mut connection = shared.clone();
                // Reconfigure in the background so returning to the menu stays responsive.
                runtime.spawn(async move {
                    let _ = connection.get(&config).await;
                });
                menu_error = None;
                continue;
            }
            if let Command::MenuReserveTarget = command {
                let config = selected_config(&o, local_config)?;
                let config_path = o.config.as_deref().unwrap_or(local_config);
                let _entered = runtime.enter();
                menu_error = queue_launch::configure_reserve_target(&config, config_path, o.wallet.as_deref(), &shared)
                    .err()
                    .map(|error| error.to_string());
                continue;
            }
            let mut c = selected_config(&o, local_config)?;
            if matches!(
                &command,
                Command::Host { .. } | Command::Join { .. } | Command::Queues | Command::Consolidate | Command::Resume
            ) {
                c.game_fee_cap = true;
                c.fee_sompi = 50_000_000;
            }
            if matches!(
                &command,
                Command::Host { .. }
                    | Command::Join { .. }
                    | Command::Queues
                    | Command::Consolidate
                    | Command::Resume
                    | Command::Send { .. }
                    | Command::MenuSend { .. }
            ) && fee_network.as_deref() != Some(&c.network)
            {
                if let Some(worker) = fee_worker.take() {
                    worker.abort();
                }
                let _entered = runtime.enter();
                fee_worker = Some(node::fee_policy::start(shared.clone(), c.clone()));
                fee_network = Some(c.network.clone());
            }
            if matches!(
                &command,
                Command::Host { .. }
                    | Command::Join { .. }
                    | Command::Queues
                    | Command::Consolidate
                    | Command::Resume
                    | Command::Send { .. }
                    | Command::MenuSend { .. }
            ) && signer_network.as_deref() != Some(&c.network)
            {
                let (player, payer) = keys(o.wallet.as_deref())?;
                let locks = node::reservations::signer_locks(&c.network, &player, &payer)?;
                _signer_locks = locks;
                signer_network = Some(c.network.clone());
                let _entered = runtime.enter();
                let _ = node::queue_service::Service::open(&c, player, shared.clone())?;
            }
            if let Command::Keygen { out } = &command {
                if o.wallet.is_some() {
                    return Err("keygen uses --out FILE; --wallet selects an existing wallet".into());
                }
                let wallet = node::wallet::Wallet::generate()?;
                wallet.save_new(out)?;
                let mut identity = node::wallet::public_identity(&c.network, &wallet.player)?;
                identity["wallet"] = serde_json::json!(out);
                println!("{}", identity);
                return Ok(());
            }
            if matches!(command, Command::Address) {
                let (player, _) = keys(o.wallet.as_deref())?;
                println!("{}", node::wallet::public_identity(&c.network, &player)?);
                return Ok(());
            }
            if matches!(command, Command::Host { .. } | Command::Join { .. })
                && let Some(path) = &o.wallet
            {
                let identity = node::wallet::Wallet::load(path)?;
                eprintln!("Wallet: {}\nYour public player key: {}", path.display(), identity.player.x_only_public_key().0);
            }
            let result = runtime.block_on(Box::pin(execute(
                command,
                c,
                o.wallet.as_deref(),
                &mut shared,
                command_received,
                settings,
                menu_mode,
            )));
            if !menu_mode {
                return result;
            }
            menu_error = result.err().map(|error| error.to_string());
            if let Some(error) = &menu_error {
                eprintln!("{error}");
            }
            eprintln!("Back to main menu. Started matches remain active and may be lost by timeout.");
        }
    })();
    if let Some(task) = fee_worker.take() {
        task.abort();
    }
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(30), shared.close()).await;
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    outcome
}

fn selected_config(o: &Options, local_config: &std::path::Path) -> Result<Config> {
    let mut c: Config = if let Some(path) = &o.config {
        node::load(path)?
    } else {
        match std::fs::read(local_config) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                serde_json::from_str(include_str!("../config/public-node.example.json"))?
            }
            Err(error) => return Err(error.into()),
        }
    };
    if let Some(network) = &o.network {
        if network != &c.network {
            if let Some(profile) = c.network_endpoints.get(network) {
                c.endpoint = profile.endpoint.clone();
                c.resolver_urls = profile.resolver_urls.clone();
                c.fallback_endpoints = profile.fallback_endpoints.clone();
            } else {
                // A custom primary endpoint can still be valid on the target
                // network and is identity-checked at connect time. Resolvers
                // and fallbacks are scoped to the old network, though.
                c.resolver_urls.clear();
                c.fallback_endpoints.clear();
            }
        }
        c.network = network.clone();
    }
    if let Some(endpoint) = &o.endpoint {
        c.endpoint = endpoint.clone();
    }
    if let Some(nodes) = o.broadcast_nodes {
        c.broadcast.nodes = nodes as usize;
    }
    if let Some(log) = &o.broadcast_log {
        c.broadcast.log = log.clone();
    }
    c.validate()?;
    Ok(c)
}

fn returns_to_menu(command: Option<&Command>, has_wallet: bool) -> bool {
    has_wallet && matches!(command, None | Some(Command::Host { json: false, .. } | Command::Join { json: false, .. }))
}

trait CommandPrompts {
    fn action(&mut self, settings: menu::GameSettings) -> Result<Option<menu::Action>>;
    fn configure(&mut self, settings: &mut menu::GameSettings) -> Result<menu::Prompt<()>>;
    fn code(&mut self) -> Result<menu::Prompt<String>>;
    fn amount(&mut self) -> Result<menu::Prompt<Option<u64>>>;
    fn address(&mut self) -> Result<menu::Prompt<String>>;
    fn fee_policy(&mut self) -> Result<menu::Prompt<node::fee_policy::FeePolicy>>;
    fn send_fee(&mut self) -> Result<menu::Prompt<node::wallet::SendFee>>;
    fn playback_base(&mut self) -> Result<menu::Prompt<u64>>;
    fn broadcast_nodes(&mut self) -> Result<menu::Prompt<usize>>;
    fn auto_accept(&mut self) -> Result<menu::Prompt<u64>>;
}
struct ConsolePrompts<'a> {
    interactive: bool,
    error: Option<&'a str>,
    player: Option<&'a menu::MenuIdentity>,
    broadcast_nodes: usize,
}
impl CommandPrompts for ConsolePrompts<'_> {
    fn action(&mut self, settings: menu::GameSettings) -> Result<Option<menu::Action>> {
        Ok(if self.interactive {
            menu::interactive_action(self.error, self.player, settings, self.broadcast_nodes)?
        } else {
            menu::action(&mut std::io::stdin().lock(), &mut std::io::stdout().lock(), settings, self.broadcast_nodes)?
        })
    }
    fn configure(&mut self, settings: &mut menu::GameSettings) -> Result<menu::Prompt<()>> {
        Ok(if self.interactive {
            menu::interactive_configure(settings)?
        } else {
            menu::configure(&mut std::io::stdin().lock(), &mut std::io::stdout().lock(), settings)?
        })
    }
    fn code(&mut self) -> Result<menu::Prompt<String>> {
        Ok(if self.interactive {
            menu::interactive_code()?
        } else {
            menu::code(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
                .map(menu::Prompt::Selected)
                .unwrap_or(menu::Prompt::Exit)
        })
    }
    fn amount(&mut self) -> Result<menu::Prompt<Option<u64>>> {
        Ok(if self.interactive {
            menu::interactive_send_amount()?
        } else {
            menu::send_amount(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
    fn address(&mut self) -> Result<menu::Prompt<String>> {
        Ok(if self.interactive {
            menu::interactive_send_address()?
        } else {
            menu::send_address(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
                .map(menu::Prompt::Selected)
                .unwrap_or(menu::Prompt::Back)
        })
    }
    fn fee_policy(&mut self) -> Result<menu::Prompt<node::fee_policy::FeePolicy>> {
        Ok(if self.interactive {
            menu::interactive_fee_policy()?
        } else {
            menu::fee_policy(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
    fn playback_base(&mut self) -> Result<menu::Prompt<u64>> {
        Ok(if self.interactive {
            menu::interactive_playback_base()?
        } else {
            menu::playback_base(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
    fn auto_accept(&mut self) -> Result<menu::Prompt<u64>> {
        Ok(if self.interactive {
            menu::interactive_auto_accept()?
        } else {
            menu::auto_accept(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
    fn broadcast_nodes(&mut self) -> Result<menu::Prompt<usize>> {
        Ok(if self.interactive {
            menu::interactive_broadcast_nodes()?
        } else {
            menu::broadcast_nodes(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
    fn send_fee(&mut self) -> Result<menu::Prompt<node::wallet::SendFee>> {
        Ok(if self.interactive {
            menu::interactive_send_fee()?
        } else {
            menu::send_fee(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
        })
    }
}
fn prompted_command(
    command: Option<Command>,
    error: Option<&str>,
    player: Option<&menu::MenuIdentity>,
    settings: &mut menu::GameSettings,
    broadcast_nodes: usize,
) -> Result<Option<Command>> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    prompted_command_with(command, &mut ConsolePrompts { interactive, error, player, broadcast_nodes }, settings)
}
fn prompted_command_with(
    mut command: Option<Command>,
    prompts: &mut impl CommandPrompts,
    settings: &mut menu::GameSettings,
) -> Result<Option<Command>> {
    loop {
        let mut selected = if let Some(command) = command.take() {
            command
        } else {
            match prompts.action(*settings)? {
                Some(menu::Action::Queues) => Command::Queues,
                Some(menu::Action::ConfigureReserveTarget) => Command::MenuReserveTarget,
                Some(menu::Action::Consolidate) => Command::Consolidate,
                Some(menu::Action::Resume) => Command::Resume,
                Some(menu::Action::Host) => Command::Host { json: false },
                Some(menu::Action::Join) => Command::Join { code: None, json: false },
                Some(menu::Action::Send) => {
                    let amount_sompi = match prompts.amount()? {
                        menu::Prompt::Selected(value) => value,
                        menu::Prompt::Back => continue,
                        menu::Prompt::Exit => return Ok(None),
                    };
                    let address = match prompts.address()? {
                        menu::Prompt::Selected(value) => value,
                        menu::Prompt::Back => continue,
                        menu::Prompt::Exit => return Ok(None),
                    };
                    let fee = match prompts.send_fee()? {
                        menu::Prompt::Selected(value) => value,
                        menu::Prompt::Back => continue,
                        menu::Prompt::Exit => return Ok(None),
                    };
                    Command::MenuSend { address, amount_sompi, fee }
                }
                Some(menu::Action::Configure) => match prompts.configure(settings)? {
                    menu::Prompt::Selected(()) | menu::Prompt::Back => {
                        settings.validate()?;
                        continue;
                    }
                    menu::Prompt::Exit => return Ok(None),
                },
                Some(menu::Action::ConfigureFee) => match prompts.fee_policy()? {
                    menu::Prompt::Selected(policy) => {
                        node::fee_policy::set_policy(policy);
                        continue;
                    }
                    menu::Prompt::Back => continue,
                    menu::Prompt::Exit => return Ok(None),
                },
                Some(menu::Action::ConfigureAutoAccept) => match prompts.auto_accept()? {
                    menu::Prompt::Selected(seconds) => {
                        network_ui::set_auto_accept_seconds(seconds);
                        continue;
                    }
                    menu::Prompt::Back => continue,
                    menu::Prompt::Exit => return Ok(None),
                },
                Some(menu::Action::ConfigureBroadcast) => match prompts.broadcast_nodes()? {
                    menu::Prompt::Selected(nodes) => Command::MenuBroadcast { nodes },
                    menu::Prompt::Back => continue,
                    menu::Prompt::Exit => return Ok(None),
                },
                Some(menu::Action::ConfigurePlayback) => match prompts.playback_base()? {
                    menu::Prompt::Selected(value) => {
                        network_ui::set_playback_base(value);
                        continue;
                    }
                    menu::Prompt::Back => continue,
                    menu::Prompt::Exit => return Ok(None),
                },
                None => return Ok(None),
            }
        };
        match &mut selected {
            Command::Join { code, json, .. } if code.is_none() => {
                if *json {
                    return Err("join --json requires a game code".into());
                }
                match prompts.code()? {
                    menu::Prompt::Selected(value) => *code = Some(value),
                    menu::Prompt::Back => continue,
                    menu::Prompt::Exit => return Ok(None),
                }
            }
            _ => {}
        }
        return Ok(Some(selected));
    }
}
async fn execute(
    command: Command,
    c: Config,
    wallet: Option<&std::path::Path>,
    shared: &mut node::rpc::startup::Connection,
    command_received: std::time::Instant,
    settings: menu::GameSettings,
    menu_mode: bool,
) -> Result<()> {
    let (command, send_all, send_fee) = match command {
        Command::MenuSend { address, amount_sompi, fee } => {
            (Command::Send { address, amount_sompi: amount_sompi.unwrap_or(0) }, amount_sompi.is_none(), fee)
        }
        command => (command, false, node::wallet::SendFee::Estimated(node::fee_policy::policy())),
    };
    let command = match command {
        Command::Host { json } => {
            return queue_launch::host(&c, wallet, json, shared, settings, command_received).await;
        }
        Command::Join { code, json } => {
            return queue_launch::join(&c, wallet, code.as_deref().ok_or("missing game code")?, json, shared, command_received).await;
        }
        Command::Queues => return queue_launch::wallet(&c, wallet, shared, settings).await,
        Command::Consolidate => return queue_launch::consolidate(&c, wallet, shared).await,
        Command::Resume => return queue_launch::resume(&c, wallet, shared, false).await,
        Command::Send { address, amount_sompi } => {
            let (player, _) = keys(wallet)?;
            if menu_mode {
                let ui = network_ui::Setup::transfer()?;
                ui.set_status("Preparing transfer. q/Esc: cancel before submission.");
                let Some(node) = ui.read(shared.get(&c)).await else { return Ok(()) };
                let n = node?;
                let Some(transaction) = ui
                    .read(async {
                        if send_all {
                            node::wallet::prepare_send_all_with_fee(&n, &c, &player, &address, send_fee).await
                        } else {
                            node::wallet::prepare_send_with_fee(&n, &c, &player, &address, amount_sompi, send_fee).await
                        }
                    })
                    .await
                else {
                    return Ok(());
                };
                let (transaction, fee) = transaction?;
                if ui.finish() {
                    return Ok(());
                }
                if !menu::interactive_send_preview(&address, transaction.outputs[0].value, fee)? {
                    return Ok(());
                }
                let txid = n.submit(&transaction).await?;
                println!("{txid}");
                menu::interactive_send_result(&txid.to_string())?;
                return Ok(());
            }
            let n = shared.get(&c).await?;
            if send_all {
                let (tx, fee) = node::wallet::prepare_send_all_with_fee(&n, &c, &player, &address, send_fee).await?;
                eprintln!("Transaction fee: {} KAS", menu::format_kas(fee));
                println!("{}", n.submit(&tx).await?);
                return Ok(());
            }
            let (tx, fee) = node::wallet::prepare_send_with_fee(&n, &c, &player, &address, amount_sompi, send_fee).await?;
            eprintln!("Transaction fee: {} KAS", menu::format_kas(fee));
            let txid = n.submit(&tx).await?;
            println!("{txid}");
            return Ok(());
        }
        command => command,
    };
    if let Command::BroadcastProbe { wait_secs } = command {
        let n = shared.get(&c).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        loop {
            let status = n.broadcast.as_ref().map(|p| p.status());
            if status.as_ref().is_none_or(|s| s["connected_nodes"].as_u64() >= Some(c.broadcast.nodes as u64))
                || tokio::time::Instant::now() >= deadline
            {
                println!(
                    "{}",
                    serde_json::json!({"mode":"rpc-broadcast", "network":c.network,
                    "primary":n.current_endpoint(), "pool":status, "log":c.broadcast.log, "transactions_sent":0})
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        return Ok(());
    }
    if let Command::Probe = command {
        let n = Node::connect(&c).await?;
        let server = n.server_info().await?;
        let (dag, delta) = n.recent_delta().await?;
        let transactions: usize = delta.added.iter().map(|b| b.transactions.len()).sum();
        let inputs: usize = delta.added.iter().flat_map(|b| &b.transactions).map(|t| t.inputs.len()).sum();
        println!(
            "{}",
            serde_json::json!({"endpoint":n.current_endpoint(),"network":server.network_id,"version":server.server_version,
            "synced":server.is_synced,"utxo_index":server.has_utxo_index,"daa":dag.virtual_daa_score,"sink":dag.sink,
            "accepted_chain_v2_full":true,"transactions_inspected":transactions,"historical_inputs_inspected":inputs})
        );
        n.disconnect().await?;
        return Ok(());
    }
    unreachable!("offline commands are handled before connecting")
}
