//! Wallet-selected node client. See README.md.
mod launch;
use argent_runtime::Artifact;
use clap::{Parser, Subcommand};
use engine::*;
use kaspa_pong::{engine, menu, network_ui, node};
use node::{
    Config,
    client::{Cache, build_action, now_ms},
    rpc::Node,
    setup,
    tracker::{Invitation, Stage},
};
use std::{collections::VecDeque, io::IsTerminal, path::PathBuf, sync::atomic::Ordering, time::Duration};

#[derive(Parser)]
#[command(about = "Pong on a configurable Kaspa gRPC/Borsh node. Select a wallet with NAME, --player NAME or --wallet FILE.")]
struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    network: Option<String>,
    /// Explicit local wallet file containing the player key.
    #[arg(long)]
    wallet: Option<PathBuf>,
    /// Explicit wallet name: wallets/NAME/wallet.json (or existing NAME.wallet.json).
    #[arg(long, conflicts_with = "wallet")]
    player: Option<String>,
    #[arg(long, default_value = "artifacts/artifact.json")]
    artifact: PathBuf,
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
        directory: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Use a host game code to create the Lobby, then play.
    Join {
        code: Option<String>,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Run a persistent read-only invitation directory. Does not open a wallet.
    Index {
        #[arg(long)]
        state: PathBuf,
        #[arg(long, default_value = "127.0.0.1:17720")]
        listen: String,
        /// Begin after this retained chain hash; defaults to the current tip for fresh invitations.
        #[arg(long)]
        anchor: Option<kaspa_consensus_core::Hash>,
    },
    /// List authenticated on-chain invitations addressed to the selected wallet.
    Invitations,
    /// Generate a local wallet and print its public keys/addresses. No node needed.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Read-only node/transport and accepted-chain Full API compatibility check.
    Probe,
    /// Print the public player key and receiving address. Does not connect or spend.
    Address,
    /// Send sompi from the selected wallet to a Kaspa address.
    Send {
        address: String,
        amount_sompi: u64,
    },
    /// Play with immediate ASCII controls in a terminal; --json selects headless output.
    Run {
        #[arg(long)]
        invitation: PathBuf,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        watch_only: bool,
        #[arg(long)]
        once: bool,
        #[arg(long)]
        rescan: bool,
        #[arg(long)]
        polls: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}
fn keys(wallet: Option<&std::path::Path>) -> Result<(secp256k1::Keypair, secp256k1::Keypair)> {
    let wallet = node::wallet::Wallet::load(wallet.ok_or("select an explicit wallet with NAME, --player NAME or --wallet FILE")?)?;
    Ok((wallet.player, wallet.player))
}
fn invitation(path: &std::path::Path, a: &Artifact, c: &Config) -> Result<Invitation> {
    let i: Invitation = node::load(path)?;
    i.validate(a)?;
    if i.network != c.network {
        return Err("invitation network differs from configuration".into());
    }
    Ok(i)
}
fn save_new<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&serde_json::to_vec_pretty(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(path).map_err(|e| e.error)?;
    Ok(())
}
// Public discovery records can already exist when both players share a folder
// or a guest retries before funding. Never replace a different record.
fn save_public_invitation<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    match save_new(path, value) {
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) => {
            let existing: serde_json::Value = node::load(path)?;
            if existing != serde_json::to_value(value)? {
                return Err(format!("{} contains a different invitation; choose another game directory", path.display()).into());
            }
            Ok(())
        }
        result => result,
    }
}
async fn synchronize(cache: &mut Cache, a: &Artifact, i: &Invitation, n: &Node, c: &Config, ui: &network_ui::Setup) -> Result<bool> {
    let until = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if ui.cancelled() { return Ok(false); }
        let Some(result) = ui.read(cache.sync(a, i, n, c)).await else { return Ok(false); };
        if result?.0 && !matches!(cache.tracker.stage, Stage::AwaitingLobby) {
            return Ok(!ui.cancelled());
        }
        if tokio::time::Instant::now() >= until {
            return Err("acceptance not observed within 60 seconds; check mining/sync and retry".into());
        }
        tokio::time::sleep(Duration::from_millis(c.poll_ms)).await;
    }
}
fn main() -> Result<()> {
    // A leading wallet name is shorthand for --player NAME, not a court side.
    let mut args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| !arg.starts_with('-') && !matches!(arg, "host" | "join" | "index" | "invitations" | "keygen" | "probe" | "address" | "send" | "run" | "help"))
    {
        args.insert(1, "--player".into());
    }
    let mut o = Options::parse_from(args);
    network_ui::set_debug_paddles(o.debug_paddles);
    if let Some(name) = &o.player {
        o.wallet = Some(menu::named_wallet(std::path::Path::new("."), name)?);
    }
    // Reject missing identity before any menu, session/config lookup, wallet
    // loading or RPC. Read-only observation uses no key.
    let needs_wallet = !matches!(&o.command, Some(Command::Keygen { .. } | Command::Probe | Command::Index { .. } | Command::Run { watch_only: true, .. }));
    if needs_wallet && o.wallet.is_none() {
        return Err("select an explicit wallet before starting Pong.\nUsage: .\\pong WALLET_NAME [host|join]\n   or: .\\pong --wallet FILE [host|join]\nWallets are never selected from defaults, environment variables or saved sessions.".into());
    }
    if let Some(path) = &o.wallet {
        eprintln!("Selected wallet: {}\nHost plays RIGHT; Join plays LEFT. Wallet names select identity.", path.display());
    }
    let menu_mode = std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
        && returns_to_menu(o.command.as_ref(), o.wallet.is_some());
    let mut next_command = o.command.take();
    let mut menu_error = None;
    let mut menu_player = None;
    // Menu-only preference: it intentionally resets on every client start.
    let mut game_speed = GameSpeed::Medium;
    let mut _signer_locks = Vec::new();
    let mut signer_network = None;
    // Transport tasks live on this runtime across every menu and match. Each
    // match still explicitly drains its own workers before returning.
    let runtime = tokio::runtime::Runtime::new()?;
    let mut shared = node::rpc::startup::Connection::default();
    let mut recovery = None;
    let outcome = (|| -> Result<()> {
    if menu_mode || matches!(&next_command, Some(Command::Host { .. } | Command::Join { .. } | Command::Run { watch_only: false, .. })) {
        let (player, payer) = keys(o.wallet.as_deref())?;
        menu_player = Some(player.x_only_public_key().0.to_string());
        let public_default = next_command.is_none() || matches!(&next_command, Some(Command::Host { .. } | Command::Join { .. }));
        let c = selected_config(&o, public_default)?;
        _signer_locks = node::hosted::signer_locks(&c.network, &player, &payer)?;
        signer_network = Some(c.network.clone());
        let _entered = runtime.enter();
        shared.warm(&c);
        let a: Artifact = node::load(&o.artifact)?;
        node::hosted::needs_recovery(&a, &c, &player, &payer)?;
        recovery = Some(node::hosted::background(a, c, player, payer, shared.clone()));
    }
    loop {
    let selected = prompted_command(next_command.take(), menu_error.as_deref(), menu_player.as_deref(), &mut game_speed)?;
    let command_received = std::time::Instant::now();
    let Some(command) = selected else {
        return Ok(());
    };
    let c = selected_config(&o, matches!(&command, Command::Host { .. } | Command::Join { .. } | Command::Invitations))?;
    if matches!(&command, Command::Host { .. } | Command::Join { .. } | Command::Run { watch_only: false, .. })
        && signer_network.as_deref() != Some(&c.network) {
        let (player, payer) = keys(o.wallet.as_deref())?;
        let locks = node::hosted::signer_locks(&c.network, &player, &payer)?;
        if let Some(task) = recovery.take() { task.abort(); let _ = runtime.block_on(task); }
        _signer_locks = locks;
        signer_network = Some(c.network.clone());
        let a: Artifact = node::load(&o.artifact)?;
        node::hosted::needs_recovery(&a, &c, &player, &payer)?;
        let _entered = runtime.enter();
        recovery = Some(node::hosted::background(a, c.clone(), player, payer, shared.clone()));
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
    if matches!(command, Command::Host { .. } | Command::Join { .. }) {
        if let Some(path) = &o.wallet {
            let identity = node::wallet::Wallet::load(path)?;
            eprintln!("Wallet: {}\nYour public player key: {}", path.display(), identity.player.x_only_public_key().0);
        }
    }
    let a: Artifact = node::load(&o.artifact)?;
    let result = {
        runtime.block_on(async {
            execute(command, &a, c, o.wallet.as_deref(), &mut shared, command_received, game_speed).await
        })
    };
    if !menu_mode { return result; }
    menu_error = result.err().map(|error| error.to_string());
    if let Some(error) = &menu_error { eprintln!("{error}"); }
    eprintln!("Back to main menu. Any still-open Lobby you created is queued for background cancellation. Started matches remain active and may be lost by timeout.");
    }
    })();
    // Recovery RPCs are cancellable: every exposed transaction already has
    // a durable exact authorization. Never drain the old invitation backlog.
    if let Some(task) = recovery.take() {
        task.abort();
        let _ = runtime.block_on(task);
    }
    runtime.block_on(async { let _ = tokio::time::timeout(Duration::from_secs(2), shared.close()).await; });
    runtime.shutdown_timeout(Duration::from_secs(1));
    outcome
}

fn selected_config(o: &Options, public_default: bool) -> Result<Config> {
    let mut c = if let Some(path) = &o.config {
        node::load(path)?
    } else if public_default {
        if std::path::Path::new("node.json").exists() {
            node::load(std::path::Path::new("node.json"))?
        } else {
            serde_json::from_str(include_str!("../config/public-node.example.json"))?
        }
    } else {
        Config::default()
    };
    if let Some(endpoint) = &o.endpoint {
        c.endpoint = endpoint.clone();
    }
    if let Some(network) = &o.network {
        c.network = network.clone();
    }
    c.validate()?;
    Ok(c)
}

fn returns_to_menu(command: Option<&Command>, has_wallet: bool) -> bool {
    has_wallet && matches!(command,
        None | Some(Command::Host { json: false, .. } | Command::Join { json: false, .. }
            | Command::Run { watch_only: false, once: false, polls: None, json: false, .. }))
}

trait CommandPrompts {
    fn action(&mut self, speed: GameSpeed) -> Result<Option<menu::Action>>;
    fn speed(&mut self) -> Result<menu::Prompt<GameSpeed>>;
    fn code(&mut self) -> Result<menu::Prompt<String>>;
}
struct ConsolePrompts<'a> { interactive: bool, error: Option<&'a str>, player: Option<&'a str> }
impl CommandPrompts for ConsolePrompts<'_> {
    fn action(&mut self, speed: GameSpeed) -> Result<Option<menu::Action>> {
        Ok(if self.interactive { menu::interactive_action(self.error, self.player, speed)? }
            else { menu::action(&mut std::io::stdin().lock(), &mut std::io::stdout().lock(), speed)? })
    }
    fn speed(&mut self) -> Result<menu::Prompt<GameSpeed>> {
        Ok(if self.interactive { menu::interactive_speed()? } else {
            menu::speed(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
                .map(menu::Prompt::Selected).unwrap_or(menu::Prompt::Back)
        })
    }
    fn code(&mut self) -> Result<menu::Prompt<String>> {
        Ok(if self.interactive { menu::interactive_code()? } else {
            menu::code(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())?
                .map(menu::Prompt::Selected).unwrap_or(menu::Prompt::Exit)
        })
    }
}
fn prompted_command(command: Option<Command>, error: Option<&str>, player: Option<&str>, speed: &mut GameSpeed) -> Result<Option<Command>> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    prompted_command_with(command, &mut ConsolePrompts { interactive, error, player }, speed)
}
fn prompted_command_with(mut command: Option<Command>, prompts: &mut impl CommandPrompts, speed: &mut GameSpeed) -> Result<Option<Command>> {
    loop {
    let mut selected = if let Some(command) = command.take() {
        command
    } else {
        match prompts.action(*speed)? {
            Some(menu::Action::Host) => Command::Host { directory: None, json: false },
            Some(menu::Action::Join) => Command::Join { code: None, directory: None, json: false },
            Some(menu::Action::ConfigureSpeed) => match prompts.speed()? {
                menu::Prompt::Selected(selected) => { *speed = selected; continue; }
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
async fn execute(command: Command, a: &Artifact, c: Config, wallet: Option<&std::path::Path>, shared: &mut node::rpc::startup::Connection, command_received: std::time::Instant, speed: GameSpeed) -> Result<()> {
    let command = match command {
        Command::Host { directory, json } => {
            return launch::host(a, &c, wallet, directory, json, shared, speed).await;
        }
        Command::Join { code, directory, json } => {
            return launch::join(a, &c, wallet, code.as_deref().ok_or("missing game code")?, directory, json, shared, command_received).await;
        }
        Command::Index { state, listen, anchor } => return node::invitations::serve(a, &c, &state, &listen, anchor).await,
        Command::Invitations => {
            let (player, _) = keys(wallet)?;
            for invite in node::invitations::addressed(a, &c, &pubkey(&player)).await? {
                println!("{}  host {}", node::invitations::code(invite.genesis.id()), secp256k1::XOnlyPublicKey::from_slice(&invite.lobby.left)?);
            }
            return Ok(());
        }
        Command::Send { address, amount_sompi } => {
            let (player, _) = keys(wallet)?;
            let n = shared.get(&c).await?;
            let txid = node::wallet::send(&n, &c, &player, &address, amount_sompi).await?;
            println!("{txid}");
            return Ok(());
        }
        command => command,
    };
    if let Command::Run { invitation: invite, state, watch_only, once, rescan, polls, json } = command {
        let play = !json && !once && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        return run(a, &c, invitation(&invite, a, &c)?, state, watch_only, once, rescan, polls, play, wallet, None, None, shared).await;
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
        n.rpc.disconnect().await?;
        return Ok(());
    }
    unreachable!("offline commands are handled before loading the artifact")
}

fn personal_spending(
    cache: &Cache,
    signers: &Option<(secp256k1::Keypair, secp256k1::Keypair)>,
    ready: bool,
) -> Result<Option<network_ui::Spending>> {
    let Some((player, payer)) = signers else {
        return Ok(None);
    };
    let script = p2pk(&pubkey(payer));
    let player_script = p2pk(&pubkey(player));
    let mut pending = 0u64;
    for p in &cache.pending {
        if let Some(payment) = node::fees::Payment::from_transaction(&p.transaction, &p.inputs)?.filter(|p| p.payer == script || p.payer == player_script) {
            pending = pending.checked_add(payment.sompi).ok_or("pending fee overflow")?;
        }
    }
    Ok(Some(network_ui::Spending { paid: cache.fee_ledger.paid_by_wallet(&player_script, &script), pending, complete: cache.fee_ledger.complete && ready }))
}

/// Setup hands ownership directly to gameplay, keeping the signer lock held.
struct PreparedRun {
    lock: std::fs::File,
    cache: Cache,
    connection: Node,
    observed: std::time::Instant,
    code_received: Option<std::time::Instant>,
}

type BuiltAction = (kaspa_consensus_core::tx::Transaction, Vec<kaspa_consensus_core::tx::UtxoEntry>, String, Option<node::reservations::Claim>);
enum SpendWork {
    Preparing {
        task: tokio::task::JoinHandle<std::result::Result<BuiltAction, String>>,
        which: Option<node::client::Action>,
        batch: Vec<network_ui::Movement>,
        started: std::time::Instant,
    },
    Saving {
        id: kaspa_consensus_core::Hash,
        wait: std::sync::mpsc::Receiver<std::result::Result<(), String>>,
        started: std::time::Instant,
    },
    Ready {
        id: kaspa_consensus_core::Hash,
    },
    Withdrawing {
        id: kaspa_consensus_core::Hash,
        wait: Option<std::sync::mpsc::Receiver<std::result::Result<(), String>>>,
        retry_at: std::time::Instant,
    },
    Submitting {
        id: kaspa_consensus_core::Hash,
        task: tokio::task::JoinHandle<std::result::Result<(), String>>,
        started: std::time::Instant,
    },
    Reconciling {
        task: tokio::task::JoinHandle<std::result::Result<Vec<(kaspa_consensus_core::Hash, bool)>, String>>,
        generation: u64,
    },
}
struct FreshMovement {
    authorization: node::client::Unsubmitted,
    batch: Vec<network_ui::Movement>,
}
impl FreshMovement {
    fn current(&self, preview: &network_ui::InputPreview, queue: &VecDeque<network_ui::Movement>, cache: &Cache, side: i64) -> Result<bool> {
        let Some(p) = cache.pending.iter().find(|p| p.transaction.id() == self.authorization.id()) else { return Ok(false) };
        if p.branch_changed || !cache.draft_current(&p.transaction, &p.inputs) { return Ok(false); }
        let Stage::Live(snapshot) = &cache.tracker.stage else { return Ok(false) };
        let world = project(snapshot, cache.tracker.daa)?.world;
        if world.ended() || matches!(node::client::action(&cache.tracker.stage, side, cache.tracker.daa, None)?,
            Some(node::client::Action::Resolve(_) | node::client::Action::Timeout | node::client::Action::Settle | node::client::Action::Reclaim)) {
            return Ok(false);
        }
        let batch = preview.batch_after_cancellation(queue, &world, side, &preview.accepted(&cache.tracker));
        Ok(network_ui::InputPreview::same_batch(&batch, &self.batch))
    }
    fn withdraw(&self, cache: &mut Cache, writer: &mut node::pipeline::Persistence) -> Result<SpendWork> {
        cache.begin_withdrawal(&self.authorization)?;
        let wait = writer.snapshot(cache, true)?.expect("withdrawal requires durable acknowledgement");
        Ok(SpendWork::Withdrawing { id: self.authorization.id(), wait: Some(wait), retry_at: std::time::Instant::now() })
    }
}
impl SpendWork {
    async fn finish(self) {
        match self {
            // A blocking preparation may already be running. Join it before
            // reopening the menu, discarding its unsubmitted result.
            Self::Preparing { task, .. } => { let _ = task.await; }
            // Submission was authorized and recorded. Finish the RPC instead
            // of abandoning it on q; its durable reservation stays intact.
            Self::Submitting { task, .. } => { let _ = task.await; }
            Self::Reconciling { task, .. } => { task.abort(); let _ = task.await; }
            _ => {}
        }
    }
}

fn gather_inputs(ui: &network_ui::Frontend, queue: &mut VecDeque<network_ui::Movement>, writer: &node::pipeline::Persistence) {
    capture_inputs(ui, queue, writer, |_, _| ());
}
fn capture_inputs<R>(
    ui: &network_ui::Frontend, queue: &mut VecDeque<network_ui::Movement>, writer: &node::pipeline::Persistence,
    commit: impl FnOnce(&mut network_ui::InputPreview, &mut VecDeque<network_ui::Movement>) -> R,
) -> R {
    let (result, received) = ui.with_inputs(queue, commit);
    record_inputs(received, writer);
    result
}
fn record_inputs(received: Vec<network_ui::Movement>, writer: &node::pipeline::Persistence) {
    for movement in received {
        let mut record = node::pipeline::Timing::new("keypress", None, None);
        record.observed_unix_ms = now_ms().saturating_sub(movement.pressed.elapsed().as_millis() as u64);
        writer.timing(record);
    }
}

async fn run(
    a: &Artifact,
    c: &Config,
    i: Invitation,
    path: PathBuf,
    watch_only: bool,
    once: bool,
    rescan: bool,
    polls: Option<u64>,
    play: bool,
    wallet: Option<&std::path::Path>,
    mut startup: Option<setup::StartPackage>,
    prepared: Option<PreparedRun>,
    shared: &mut node::rpc::startup::Connection,
) -> Result<()> {
    let code_received = prepared.as_ref().and_then(|p| p.code_received);
    let (_lock, mut cache, mut connection, mut observed) = match prepared {
        Some(p) => (p.lock, p.cache, Some(p.connection), p.observed),
        None => (
            node::lock(&path)?,
            if path.exists() { node::load::<Cache>(&path)? } else { Cache::new(&i) },
            None,
            std::time::Instant::now(),
        ),
    };
    let mut follower: Option<node::pipeline::Follower> = None;
    let mut work: Option<SpendWork> = None;
    let mut clock_work: Option<tokio::task::JoinHandle<std::result::Result<(u64, u64), String>>> = None;
    let result = async {
    // Validate identity even when rebuilding history; a rescan must never erase
    // another game's spending history or an uncertain propagated transaction.
    if rescan {
        cache.rescan(&i);
    }
    cache.validate(a, &i)?;
    // A loaded withdrawal marker is itself the durable acknowledgement. Other
    // recovered pending spends have no runtime capability and stay protected.
    cache.recover_withdrawals()?;

    let reorg_log_path = node::audit::log_path(&path);
    let mut reorg_log = node::audit::ReorgLog::open(&reorg_log_path)?;
    cache.flush_reorg_log(&path, &mut reorg_log)?;
    let transaction_log_path = node::audit::transaction_log_path(&path);
    let mut transaction_log = node::audit::ReorgLog::open(&transaction_log_path)?;
    let recovered_records = cache.flush_transaction_log(&path, &mut transaction_log)?;
    if !play {
        for record in recovered_records {
            println!("{}", serde_json::json!({"type":"transaction","record":record}));
        }
    }
    let signers = if watch_only { None } else { Some(keys(wallet)?) };
    if let Some((player, payer)) = &signers { node::hosted::remember_state(&path, &c.network, player, payer)?; }
    let side = if let Some((player, _)) = &signers {
        let p = pubkey(player);
        if p == i.lobby.left {
            0
        } else if p == i.lobby.right {
            1
        } else {
            return Err("key is not an invited player".into());
        }
    } else {
        0
    };
    let ui = network_ui::Frontend::new(play, !once, side, watch_only, &c.network)?;
    let mut queue: VecDeque<network_ui::Movement> = VecDeque::new();
    let mut delivery = network_ui::MovementDelivery::default();
    let settings = GameSpeed::from_tick_daa(i.lobby.ball_tick_daa)
        .map(|speed| format!("Game pace: {} ({} ticks)", speed.label(), speed.tick_daa()))
        .unwrap_or_else(|| format!("Game pace: {} ticks", i.lobby.ball_tick_daa));
    let lobby_hint = if side == 0 {
        Some(format!(
            "{settings}\nYou created the Lobby as LEFT. Waiting for the host (RIGHT) to fund and start it.\nYour public key:\n{}\nHost game key:\n{}",
            secp256k1::XOnlyPublicKey::from_slice(&i.lobby.left)?,
            secp256k1::XOnlyPublicKey::from_slice(&i.lobby.right)?
        ))
    } else {
        Some(format!("{settings}\nYou are the host (RIGHT). Your matching stake and start transaction are being prepared."))
    };
    // The coordinator alone mutates reservations. Chain replay, transaction
    // preparation/submission and the ordered disk writer do not borrow it.
    drop(reorg_log);
    drop(transaction_log);
    let mut writer = node::pipeline::Persistence::new(path.clone());
    let mut fresh_movement: Option<FreshMovement> = None;
    let mut next_clock = std::time::Instant::now();
    let mut next_connect = std::time::Instant::now();
    let mut synchronized = connection.is_some() && !rescan && observed.elapsed() <= network_ui::SNAPSHOT_LIFETIME;
    let mut clock_epoch = 0u64;
    let mut status = if synchronized { "Following accepted chain" } else { "Synchronizing" }.to_string();
    let mut launch_issue: Option<String> = None;
    let mut diagnostic_issue: Option<String> = None;
    let mut last_checkpoint = std::time::Instant::now();
    let mut last_ui = std::time::Instant::now() - Duration::from_secs(1);
    let mut next_spend = std::time::Instant::now();
    let mut completed_polls = 0u64;
    let mut stop_after_turn = false;
    let mut failure: Option<String> = None;
    let mut start_submission = None;
    let mut start_ui = None;
    writer.snapshot(&cache, false)?;
    loop {
        if ui.quit.load(Ordering::Relaxed) || stop_after_turn {
            break;
        }
        gather_inputs(&ui, &mut queue, &writer);
        if connection.is_none() && std::time::Instant::now() >= next_connect {
            match shared.get(c).await {
                Ok(n) => {
                    connection = Some(n);
                    next_clock = std::time::Instant::now();
                }
                Err(error) => {
                    status = format!("Paused: {error}");
                    if once {
                        failure = Some(error.to_string());
                        break;
                    }
                    next_connect = std::time::Instant::now() + Duration::from_secs(1);
                }
            }
        }
        if follower.is_none() {
            if let Some(n) = &connection {
                follower = Some(node::pipeline::Follower::new(cache.clone(), a.clone(), i.clone(), n.clone(), c.clone()));
            }
        }
        let mut network_error = None;
        let mut accepted_update = false;
        if let Some(following) = &follower {
            loop {
                match following.try_update() {
                    Ok(None) => break,
                    Err(error) => {
                        network_error = Some(error);
                        break;
                    }
                    Ok(Some(update)) => {
                        let ready = update.ready;
                        let game_reorg = update.reorg;
                        let new_records = update.cache.transaction_log_pending.clone();
                        let removed = if game_reorg {
                            update
                                .cache
                                .reorg_log_pending
                                .iter()
                                .flat_map(|event| event.removed_game_transactions.iter().map(|tx| tx.txid))
                                .collect::<Vec<_>>()
                        } else {
                            vec![]
                        };
                        cache.publish_validated(update.cache, game_reorg);
                        synchronized = ready;
                        if ready {
                            observed = std::time::Instant::now()
                                .checked_sub(Duration::from_millis(now_ms().saturating_sub(update.validated_ms)))
                                .unwrap_or_else(std::time::Instant::now);
                        }
                        if game_reorg {
                            clock_epoch = clock_epoch.wrapping_add(1);
                            for (movement, txid) in delivery.reorged(&removed) {
                                ui.restore_movement(&movement, txid);
                            }
                        }
                        // Only coherent replay can release or invalidate a lane.
                        if ready {
                            if fresh_movement.is_none() { cache.discard_conflicting_pending(); }
                            let accepted_moves = delivery.accepted(&cache.tracker);
                            let pending_ids = cache.pending.iter().map(|p| p.transaction.id()).collect::<Vec<_>>();
                            for movement in delivery.reconcile(&accepted_moves, &pending_ids) {
                                ui.requeue_movement(&movement);
                                queue.push_back(movement);
                            }
                            if fresh_movement.is_none() && matches!(cache.tracker.stage, Stage::Collecting(_) | Stage::Reclaimed(_) | Stage::Cancelled) {
                                queue.clear();
                                ui.clear_preview();
                            }
                            if !matches!(cache.tracker.stage, Stage::AwaitingLobby | Stage::Lobby(_)) {
                                startup = None;
                            }
                        }
                        for record in new_records {
                            if !play {
                                println!("{}", serde_json::json!({"type":"transaction","record":record}));
                            }
                            if record.status == node::audit::TransactionStatus::Accepted {
                                let arrival = record.accepting_block.and_then(|b| update.arrivals.get(&b)).copied();
                                if record.event.action == "start" {
                                    start_ui = Some(record.event.txid);
                                    let mut timing = node::pipeline::Timing::new("accepted_start_validated", Some(record.event.txid), None);
                                    timing.observed_unix_ms = update.validated_ms;
                                    timing.accepted_daa = record.event.accepted_daa;
                                    writer.timing(timing);
                                    if let Some(arrived) = arrival {
                                        let mut timing = node::pipeline::Timing::new("accepted_start_arrival", Some(record.event.txid),
                                            start_submission.map(|submitted: u64| arrived.saturating_sub(submitted)));
                                        timing.observed_unix_ms = arrived;
                                        timing.accepted_daa = record.event.accepted_daa;
                                        writer.timing(timing);
                                    }
                                }
                                if let Some(arrived) = arrival {
                                    let mut timing =
                                        node::pipeline::Timing::new("acceptance_data_arrival", Some(record.event.txid), None);
                                    timing.observed_unix_ms = arrived;
                                    timing.accepted_daa = record.event.accepted_daa;
                                    writer.timing(timing);
                                    let mut timing = node::pipeline::Timing::new(
                                        "validation_complete",
                                        Some(record.event.txid),
                                        Some(update.validated_ms.saturating_sub(arrived)),
                                    );
                                    timing.observed_unix_ms = update.validated_ms;
                                    timing.accepted_daa = record.event.accepted_daa;
                                    writer.timing(timing);
                                }
                            }
                        }
                        completed_polls = completed_polls.saturating_add(1);
                        accepted_update = true;
                        status = if ready { "Following accepted chain" } else { "Catching up; controls retained locally" }.into();
                        if ready && matches!(cache.tracker.stage, Stage::Lobby(_)) {
                            status = launch_issue.clone().unwrap_or_else(|| {
                                if side == 0 {
                                    "Waiting for the guest to sign and submit the start"
                                } else if cache.pending.is_empty() {
                                    "Both players signed; preparing start submission"
                                } else {
                                    "Both players signed; waiting for start acceptance"
                                }
                                .into()
                            });
                        }
                    }
                }
            }
        }
        // The independent clock is display-only. It cannot authorize spending.
        if let Some(task) = &clock_work {
            if tokio::task::JoinHandle::is_finished(task) {
                let task = clock_work.take().unwrap();
                if let Ok(Ok((epoch, daa))) = task.await {
                    if epoch == clock_epoch {
                        ui.observe_clock(network_ui::ClockSample { epoch, daa, observed: std::time::Instant::now() });
                    }
                }
            }
        }
        if clock_work.is_none() && std::time::Instant::now() >= next_clock {
            if let Some(n) = &connection {
                let n = n.clone();
                let epoch = clock_epoch;
                clock_work = Some(tokio::spawn(async move {
                    n.info().await.map(|info| (epoch, info.virtual_daa_score)).map_err(|e| e.to_string())
                }));
                next_clock = std::time::Instant::now() + Duration::from_millis(100);
            }
        }
        // Disk completion only acknowledges IDs actually appended. New outbox
        // events, created while that snapshot was writing, are retained.
        for written in writer.completed.try_iter() {
            match written.result {
                Ok(()) => {
                    cache.reorg_log_pending.retain(|e| !written.reorg_ids.contains(&e.id));
                    cache.transaction_log_pending.retain(|e| !written.transaction_ids.contains(&e.id));
                }
                Err(error) => {
                    diagnostic_issue = Some(format!("Diagnostic/cache persistence failed: {error}"));
                    if !play {
                        eprintln!("{}", diagnostic_issue.as_ref().unwrap());
                    }
                }
            }
        }
        // Publish chain-owned state before handling any preparation/disk/RPC
        // completion. None of those jobs can delay a new accepted UI snapshot.
        if accepted_update || last_ui.elapsed() >= Duration::from_millis(33) {
            ui.update(network_ui::Display {
                stage: cache.tracker.stage.clone(),
                daa: cache.tracker.daa,
                ready: synchronized,
                observed,
                clock_epoch,
                pending: cache.pending.len(),
                accepted_previews: ui.accepted_previews(&cache.tracker),
                queued: queue.len(),
                spending: personal_spending(&cache, &signers, synchronized)?,
                reorgs: cache.game_reorgs,
                status: diagnostic_issue.clone().unwrap_or_else(|| status.clone()),
                last_reorg: cache.last_reorg.clone(),
                recent_transactions: cache.recent_transactions.clone(),
                lobby_hint: lobby_hint.clone(),
            });
            last_ui = std::time::Instant::now();
        }
        if accepted_update {
            writer.timing(node::pipeline::Timing::new("ui_published", None, None));
            if let Some(id) = start_ui.take() {
                writer.timing(node::pipeline::Timing::new("accepted_start_ui_published", Some(id), None));
                if let Some(started) = code_received {
                    writer.timing(node::pipeline::Timing::new("code_entry_total", Some(id), Some(started.elapsed().as_millis() as u64)));
                }
            }
            if !play {
                let view = if synchronized {
                    if let Stage::Live(s) = &cache.tracker.stage { Some(project(s, cache.tracker.daa)?) } else { None }
                } else {
                    None
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "ready":synchronized,"accepted":cache.tracker.stage,"view":view,"node_daa":cache.tracker.daa,
                        "pending":cache.pending.iter().map(|p|p.transaction.id()).collect::<Vec<_>>(),
                        "queued_inputs":queue.len(),"reorgs":cache.game_reorgs,"authorized_fees_sompi":cache.authorized_fees,"status":status,
                        "personal_spending":personal_spending(&cache,&signers,synchronized)?.map(|s|serde_json::json!({
                            "paid_sompi":s.paid,"pending_sompi":s.pending,"history_complete":s.complete})),
                        "last_reorg":cache.last_reorg,"reorg_log":reorg_log_path,"transaction_log":transaction_log_path,
                        "endpoint":connection.as_ref().map(Node::current_endpoint)
                    })
                );
            }
        }
        // Fresh members remain queued and unissued until the network handoff.
        // Inspect on a copy: withdrawal failure must not release their intent.
        if let Some(fresh) = &fresh_movement {
            if !matches!(&work, Some(SpendWork::Withdrawing { .. })) {
                let current = capture_inputs(&ui, &mut queue, &writer, |preview, queue| fresh.current(preview, queue, &cache, side))?;
                if !current || !synchronized || observed.elapsed() > network_ui::SNAPSHOT_LIFETIME {
                    work = Some(fresh.withdraw(&mut cache, &mut writer)?);
                }
            }
        }
        if let Some(current) = work.take() {
            work = match current {
                SpendWork::Preparing { task, which, batch, started } if task.is_finished() => {
                    match task.await {
                        Ok(Ok((tx, entries, purpose, mut claim))) => {
                            writer.timing(node::pipeline::Timing::new(
                                "preparation_complete",
                                Some(tx.id()),
                                Some(started.elapsed().as_millis() as u64),
                            ));
                            capture_inputs(&ui, &mut queue, &writer, |preview, queue| -> Result<()> {
                                if let Stage::Live(snapshot) = &cache.tracker.stage {
                                    let world = project(snapshot, cache.tracker.daa)?.world;
                                    preview.cancel_unsent(queue, &world, side, &preview.accepted(&cache.tracker));
                                }
                                let moving = matches!(which, Some(node::client::Action::Move(_)));
                                let same_batch = !moving || network_ui::InputPreview::same_batch(&preview.ready_movement_batch(queue, std::time::Instant::now()), &batch);
                                let live = !moving || matches!(&cache.tracker.stage, Stage::Live(s) if !project(s,cache.tracker.daa)?.world.ended());
                                if !(synchronized && live && same_batch && cache.draft_current(&tx, &entries)) { return Ok(()); }
                                let id = tx.id();
                                if moving && preview.unissued_batch(&batch) {
                                    match cache.authorize_unsubmitted(tx, entries, purpose, c) {
                                        Ok(authorization) => {
                                            if let Some(claim) = claim.take() { claim.promote(&path, id)?; }
                                            fresh_movement = Some(FreshMovement { authorization, batch });
                                        }
                                        Err(error) => status = format!("Spending paused: {error}"),
                                    }
                                    return Ok(());
                                }
                                let authorized = if which.is_none() {
                                    match (&startup, &signers) {
                                        (Some(package), Some((player, _))) => package.enqueue(&mut cache, &pubkey(player), c).map(|_| ()),
                                        _ => Err("launch state changed during validation".into()),
                                    }
                                } else {
                                    cache.authorize(tx, entries, purpose, c)
                                };
                                match authorized {
                                    Ok(()) => {
                                        if let Some(claim) = claim.take() { claim.promote(&path, id)?; }
                                        if moving {
                                            for _ in 0..batch.len() {
                                                let movement = queue.pop_front().expect("authorized movement is still queued");
                                                preview.submitted(movement.pressed, id);
                                                delivery.submitted(movement, id);
                                            }
                                        }
                                    }
                                    Err(error) => status = format!("Spending paused: {error}"),
                                }
                                Ok(())
                            })?;
                        }
                        Ok(Err(error)) => {
                            status = format!("Spending paused: {error}");
                            next_spend = std::time::Instant::now() + Duration::from_secs(1);
                        }
                        Err(error) => {
                            status = format!("Preparation failed: {error}");
                            next_spend = std::time::Instant::now() + Duration::from_secs(1);
                        }
                    }
                    None
                }
                SpendWork::Saving { id, wait, started } => match wait.try_recv() {
                    Ok(Ok(())) => {
                        writer.timing(node::pipeline::Timing::new(
                            "required_persistence_complete",
                            Some(id),
                            Some(started.elapsed().as_millis() as u64),
                        ));
                        Some(SpendWork::Ready { id })
                    }
                    Ok(Err(error)) => {
                        status = format!("Submission blocked: required persistence failed: {error}");
                        // Never submit this authorization until a later durable
                        // retry succeeds. Reservations remain in the cache.
                        if let Some(p) = cache.pending.iter_mut().find(|p| p.transaction.id() == id) {
                            p.attempted_ms = 0;
                        }
                        next_spend = std::time::Instant::now() + Duration::from_secs(1);
                        None
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => Some(SpendWork::Saving { id, wait, started }),
                    Err(_) => {
                        failure = Some("required persistence worker stopped".into());
                        None
                    }
                },
                SpendWork::Withdrawing { id, mut wait, mut retry_at } => {
                    let result = wait.as_ref().map(|rx| rx.try_recv());
                    match result {
                        Some(Ok(Ok(()))) => {
                            capture_inputs(&ui, &mut queue, &writer, |preview, queue| -> Result<()> {
                                cache.complete_withdrawal(id)?;
                                fresh_movement = None;
                                if let Stage::Live(snapshot) = &cache.tracker.stage {
                                    let world = project(snapshot, cache.tracker.daa)?.world;
                                    preview.cancel_unsent(queue, &world, side, &preview.accepted(&cache.tracker));
                                } else if matches!(cache.tracker.stage, Stage::Collecting(_) | Stage::Reclaimed(_) | Stage::Cancelled) {
                                    preview.clear();
                                    queue.clear();
                                }
                                Ok(())
                            })?;
                            writer.timing(node::pipeline::Timing::new("withdrawal_persisted", Some(id), None));
                            None
                        }
                        Some(Ok(Err(_))) | Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            status = "Withdrawal persistence failed; inputs and fee reservation retained".into();
                            wait = None;
                            retry_at = std::time::Instant::now() + Duration::from_secs(1);
                            Some(SpendWork::Withdrawing { id, wait, retry_at })
                        }
                        _ => {
                            if wait.is_none() && std::time::Instant::now() >= retry_at {
                                wait = writer.snapshot(&cache, true)?;
                            }
                            Some(SpendWork::Withdrawing { id, wait, retry_at })
                        }
                    }
                }
                SpendWork::Ready { id } => {
                    let (result, received) = ui.with_inputs(&mut queue, |preview, queue| -> Result<Option<SpendWork>> {
                    if let Some(fresh) = &fresh_movement {
                        if !fresh.current(preview, queue, &cache, side)? {
                            return Ok(Some(fresh.withdraw(&mut cache, &mut writer)?));
                        }
                    }
                    if let Some(p) = cache.pending.iter().find(|p| p.transaction.id() == id) {
                        if p.branch_changed || !cache.draft_current(&p.transaction, &p.inputs) {
                            // Reconcile after a coherent sync, including lost
                            // fee parents, rather than rebuilding on a tip tick.
                            if let Some(p) = cache.pending.iter_mut().find(|p| p.transaction.id() == id) {
                                p.branch_changed = true;
                            }
                            Ok(None)
                        } else if synchronized && observed.elapsed() <= network_ui::SNAPSHOT_LIFETIME {
                            if let Some(n) = &connection {
                                let n = n.clone();
                                let p = p.clone();
                                if let Some(fresh) = fresh_movement.take() {
                                    cache.mark_exposed(fresh.authorization)?;
                                    for movement in fresh.batch {
                                        let queued = queue.pop_front().expect("fresh network handoff retains its queue prefix");
                                        assert_eq!(queued.pressed, movement.pressed);
                                        preview.submitted(movement.pressed, id);
                                        delivery.submitted(movement, id);
                                    }
                                }
                                writer.timing(node::pipeline::Timing::new("submit_request", Some(id), None));
                                if p.purpose == "host start" { start_submission = Some(now_ms()); }
                                let task = tokio::spawn(async move {
                                    if p.purpose == "host start"
                                        && !n.sources_unspent(&p.transaction, &p.inputs).await.map_err(|e| e.to_string())?
                                    {
                                        return Err(
                                            "start inputs unavailable; the lobby was cancelled or the host funding coin was spent"
                                                .to_string(),
                                        );
                                    }
                                    n.submit(&p.transaction).await.map(|_| ()).map_err(|e| e.to_string())
                                });
                                Ok(Some(SpendWork::Submitting { id, task, started: std::time::Instant::now() }))
                            } else {
                                Ok(Some(SpendWork::Ready { id }))
                            }
                        } else {
                            Ok(Some(SpendWork::Ready { id }))
                        }
                    } else {
                        Ok(None)
                    }
                    });
                    record_inputs(received, &writer);
                    result?
                }
                SpendWork::Submitting { id, task, started } if task.is_finished() => {
                    match task.await {
                        Ok(Ok(())) => {
                            writer.timing(node::pipeline::Timing::new(
                                "submit_acknowledged",
                                Some(id),
                                Some(started.elapsed().as_millis() as u64),
                            ));
                            if matches!(cache.tracker.stage, Stage::Lobby(_)) {
                                launch_issue = None;
                                status = "Start submitted; waiting for chain acceptance".into();
                            }
                        }
                        result => {
                            let error = match result {
                                Ok(Err(e)) => e,
                                Err(e) => e.to_string(),
                                _ => unreachable!(),
                            };
                            // A failed acknowledgement is ambiguous: retain the
                            // durable reservation, reconcile against fresh chain
                            // and mempool observations before authorizing retry.
                            if let Some(p) = cache.pending.iter_mut().find(|p| p.transaction.id() == id) {
                                p.branch_changed = true;
                            }
                            network_error = Some(error);
                        }
                    }
                    None
                }
                SpendWork::Reconciling { task, generation } if task.is_finished() => {
                    match task.await {
                        Ok(Ok(results)) if generation == cache.game_reorgs => {
                            for (id, keep) in results {
                                if !keep {
                                    cache.discard_pending(id);
                                } else if let Some(p) = cache.pending.iter_mut().find(|p| p.transaction.id() == id) {
                                    p.branch_changed = false;
                                    p.attempted_ms = now_ms();
                                }
                            }
                        }
                        Ok(Ok(_)) => {} // a newer game reorg superseded these RPC observations
                        result => {
                            network_error = Some(match result {
                                Ok(Err(e)) => e,
                                Err(e) => e.to_string(),
                                _ => unreachable!(),
                            })
                        }
                    }
                    if synchronized && fresh_movement.is_none() {
                        cache.discard_conflicting_pending();
                    }
                    None
                }
                pending => Some(pending),
            };
        }
        if let Some(error) = network_error {
            synchronized = false;
            status = format!("Paused: {error}");
            if matches!(cache.tracker.stage, Stage::Lobby(_)) {
                launch_issue = Some(format!("Start delayed: {error}; reconnecting and retrying"));
            }
            if !play {
                eprintln!("{status}");
            }
            if let Some(mut following) = follower.take() { following.finish().await; }
            if let Some(task) = clock_work.take() {
                task.abort();
                let _ = task.await;
            }
            if !matches!(&work, Some(SpendWork::Withdrawing { .. })) {
                if let Some(pending) = work.take() { pending.finish().await; }
            }
            for pending in &mut cache.pending {
                pending.branch_changed = true;
            }
            if let Some(n) = connection.take() {
                shared.retain(None);
                let _ = n.rpc.disconnect().await;
            }
            next_connect = std::time::Instant::now() + Duration::from_secs(1);
            if once {
                failure = Some(error);
                break;
            }
        }
        if work.is_none()
            && synchronized
            && observed.elapsed() <= network_ui::SNAPSHOT_LIFETIME
            && std::time::Instant::now() >= next_spend
        {
            if let (Some(n), Some((player, payer))) = (&connection, &signers) {
                let overdue = cache
                    .pending
                    .iter()
                    .filter(|p| {
                        let timeout = if p.purpose == "host start" { 5 } else { c.pending_timeout_secs };
                        p.branch_changed || (p.attempted_ms > 0 && now_ms().saturating_sub(p.attempted_ms) > timeout * 1000)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !overdue.is_empty() {
                    let n = n.clone();
                    let parents = cache.pending.iter().map(|p| p.transaction.clone()).collect::<Vec<_>>();
                    let payer = pubkey(payer);
                    let player_public = pubkey(player);
                    let player_script = p2pk(&player_public);
                    work = Some(SpendWork::Reconciling {
                        generation: cache.game_reorgs,
                        task: tokio::spawn(async move {
                            let mut results = Vec::new();
                            for p in overdue {
                                let invalid = p.branch_changed
                                    && !n.sources_available(&p.transaction, &p.inputs, &parents).await.map_err(|e| e.to_string())?;
                                let owner = if p.inputs.iter().any(|u| u.covenant_id.is_none() && u.script_public_key == player_script) { &player_public } else { &payer };
                                let keep = !invalid && n.pending_exists(p.transaction.id(), owner).await.map_err(|e| e.to_string())?;
                                results.push((p.transaction.id(), keep));
                            }
                            Ok(results)
                        }),
                    });
                } else if let Some(index) = cache.pending.iter().position(|p| p.attempted_ms == 0 && !p.branch_changed) {
                    let id = cache.pending[index].transaction.id();
                    cache.pending[index].attempted_ms = now_ms();
                    let wait = writer.snapshot(&cache, true)?.expect("required write has acknowledgement");
                    work = Some(SpendWork::Saving { id, wait, started: std::time::Instant::now() });
                    last_checkpoint = std::time::Instant::now();
                } else if let Some(package) =
                    startup.as_ref().filter(|_| cache.pending.is_empty() && matches!(cache.tracker.stage, Stage::Lobby(_)))
                {
                    let package = package.clone();
                    let a = a.clone();
                    let invite = i.clone();
                    let fee_cap = c.fee_sompi;
                    work = Some(SpendWork::Preparing {
                        task: tokio::task::spawn_blocking(move || {
                            setup::validate(&a, &invite, &package, fee_cap).map_err(|e| e.to_string())?;
                            if !package.complete() { return Err("unfunded proposal cannot be submitted".into()); }
                            let inputs = package.inputs();
                            Ok((package.transaction, inputs, "host start".into(), None))
                        }),
                        which: None,
                        batch: vec![],
                        started: std::time::Instant::now(),
                    });
                } else {
                    let batch = capture_inputs(&ui, &mut queue, &writer, |preview, queue| -> Result<_> {
                        if let Stage::Live(snapshot) = &cache.tracker.stage {
                            let world = project(snapshot, cache.tracker.daa)?.world;
                            preview.cancel_unsent(queue, &world, side, &preview.accepted(&cache.tracker));
                        }
                        Ok(preview.ready_movement_batch(queue, std::time::Instant::now()))
                    })?;
                    let steps = (!batch.is_empty()).then(|| batch.iter().map(|m| m.direction).sum());
                    if let Some(which) = cache.next_action(side, cache.tracker.daa, steps)? {
                        let mut timing = node::pipeline::Timing::new(
                            "batch_selected",
                            None,
                            batch.first().map(|m| m.pressed.elapsed().as_millis() as u64),
                        );
                        timing.accepted_daa = Some(cache.tracker.daa);
                        writer.timing(timing);
                        let a = a.clone();
                        let n = n.clone();
                        let snapshot = cache.clone();
                        let player = *player;
                        let payer = *payer;
                        let config = c.clone();
                        work = Some(SpendWork::Preparing {
                            task: tokio::spawn(async move {
                                build_action(&a, &n, &snapshot, &player, &payer, which, &config).await
                                    .map(|(tx, inputs, purpose, claim)| (tx, inputs, purpose, Some(claim))).map_err(|e| e.to_string())
                            }),
                            which: Some(which),
                            batch,
                            started: std::time::Instant::now(),
                        });
                    }
                }
            }
        }
        if last_checkpoint.elapsed() >= Duration::from_secs(1) {
            writer.snapshot(&cache, false)?;
            last_checkpoint = std::time::Instant::now();
        }
        if failure.is_some() {
            break;
        }
        // Headless bounded runs retain the old "finish this turn's work"
        // behavior; interactive tracking is otherwise never gated by a spend.
        if completed_polls >= polls.unwrap_or(u64::MAX) || (once && completed_polls > 0) {
            if work.is_none() {
                stop_after_turn = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if let Some(mut following) = follower.take() { following.finish().await; }
    if let Some(task) = clock_work.take() {
        task.abort();
        let _ = task.await;
    }
    if let Some(pending) = work.take() {
        pending.finish().await;
    }
    if let Some(fresh) = fresh_movement {
        cache.begin_withdrawal(&fresh.authorization)?;
        let wait = writer.snapshot(&cache, true)?.expect("shutdown withdrawal is durable");
        wait.recv().map_err(|_| "shutdown withdrawal acknowledgement missing")?.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        cache.complete_withdrawal(fresh.authorization.id())?;
    }
    // The last required and diagnostic writes drain before releasing the signer
    // lock. Unknown submission outcomes stay durably reserved for recovery.
    let wait = writer.snapshot(&cache, true)?.expect("final durable checkpoint");
    writer.finish()?;
    wait.recv().map_err(|_| "final cache checkpoint missing")?.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
    }.await;
    // Also drain on every early error: persistent runtime ownership no longer
    // provides a per-match Runtime::drop safety net for detached tasks.
    if let Some(mut following) = follower.take() { following.finish().await; }
    if let Some(task) = clock_work.take() { task.abort(); let _ = task.await; }
    if let Some(pending) = work.take() { pending.finish().await; }
    shared.retain(connection);
    result
}

#[cfg(test)]
mod public_invitation_tests {
    #[test]
    fn guest_reuses_identical_host_record_and_can_retry_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invitation.json");
        let record = serde_json::json!({"network": "devnet", "public_match": 19});
        super::save_new(&path, &record).unwrap();
        let original = std::fs::read(&path).unwrap();
        super::save_public_invitation(&path, &record).unwrap();
        super::save_public_invitation(&path, &record).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn guest_creates_missing_record_but_never_replaces_a_conflicting_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invitation.json");
        let record = serde_json::json!({"public_match": 19});
        super::save_public_invitation(&path, &record).unwrap();
        let original = std::fs::read(&path).unwrap();
        let error = super::save_public_invitation(&path, &serde_json::json!({"public_match": 20})).unwrap_err();
        assert!(error.to_string().contains("different invitation"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(super::save_new(&path, &record).is_err(), "other exclusive writes remain exclusive");
    }
}

#[cfg(test)]
mod menu_return_tests {
    #[test]
    fn debug_paddles_is_opt_in_and_accepted_after_subcommands() {
        use clap::Parser;
        assert!(!super::Options::try_parse_from(["pong", "host"]).unwrap().debug_paddles);
        assert!(super::Options::try_parse_from(["pong", "host", "--debug-paddles"]).unwrap().debug_paddles);
    }
    use super::*;
    struct Scripted {
        actions: VecDeque<Option<menu::Action>>,
        codes: VecDeque<menu::Prompt<String>>,
    }
    impl CommandPrompts for Scripted {
        fn action(&mut self, _speed: GameSpeed) -> Result<Option<menu::Action>> { Ok(self.actions.pop_front().expect("unexpected main menu")) }
        fn speed(&mut self) -> Result<menu::Prompt<GameSpeed>> { panic!("unexpected speed prompt") }
        fn code(&mut self) -> Result<menu::Prompt<String>> { Ok(self.codes.pop_front().expect("unexpected game code")) }
    }
    #[test]
    fn back_from_join_returns_to_main_and_host_needs_no_key_prompt() {
        let mut script = Scripted { actions: VecDeque::from([Some(menu::Action::Join), Some(menu::Action::Host)]),
            codes: VecDeque::from([menu::Prompt::Back]) };
        assert!(matches!(prompted_command_with(None, &mut script, &mut GameSpeed::Medium).unwrap(), Some(Command::Host { .. })));
        assert!(script.actions.is_empty());
        let mut script = Scripted { actions: VecDeque::new(), codes: VecDeque::from([menu::Prompt::Exit]) };
        assert!(prompted_command_with(Some(Command::Join { code: None, directory: None, json: false }), &mut script, &mut GameSpeed::Medium).unwrap().is_none());
    }
    #[test]
    fn back_from_direct_join_can_choose_host_without_restarting() {
        let mut script = Scripted { actions: VecDeque::from([Some(menu::Action::Host)]),
            codes: VecDeque::from([menu::Prompt::Back]) };
        let command = prompted_command_with(Some(Command::Join { code: None, directory: None, json: false }), &mut script, &mut GameSpeed::Medium).unwrap().unwrap();
        assert!(matches!(command, Command::Host { json: false, .. }));
    }
    #[test]
    fn machine_readable_commands_do_not_enter_the_interactive_session_loop() {
        assert!(returns_to_menu(None, true));
        assert!(!returns_to_menu(None, false));
        assert!(returns_to_menu(Some(&Command::Join { code: None, directory: None, json: false }), true));
        assert!(!returns_to_menu(Some(&Command::Join { code: None, directory: None, json: true }), true));
        let mut script = Scripted { actions: VecDeque::new(), codes: VecDeque::new() };
        assert!(prompted_command_with(Some(Command::Join { code: None, directory: None, json: true }), &mut script, &mut GameSpeed::Medium).is_err());
    }
    #[tokio::test]
    async fn menu_return_waits_for_an_already_authorized_submit() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished = done.clone();
        let task = tokio::spawn(async move { tokio::task::yield_now().await; finished.store(true, Ordering::Relaxed); Ok(()) });
        SpendWork::Submitting { id: kaspa_consensus_core::Hash::from_bytes([0; 32]), task, started: std::time::Instant::now() }.finish().await;
        assert!(done.load(Ordering::Relaxed));
    }
}
