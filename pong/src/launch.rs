//! One explicitly selected match: authenticated on-chain invitation and play.
use super::{PreparedRun, engine::*, network_ui, node, run, save_new, save_public_invitation, synchronize};
use argent_runtime::Artifact;
use node::{
    Config,
    client::{Cache, priced_transaction},
    rpc::Node,
    setup,
    tracker::{Invitation, Stage},
};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

struct Session {
    wallet: PathBuf,
    directory: PathBuf,
    side: i64,
    invitation: Invitation,
    start: Option<setup::StartPackage>,
    code_received: Option<std::time::Instant>,
}

fn state_path(s: &Session) -> PathBuf {
    s.directory.join(if s.side == 0 { "left-state.json" } else { "right-state.json" })
}
fn directory(path: Option<PathBuf>, id: kaspa_consensus_core::Hash) -> Result<PathBuf> {
    let path = path.unwrap_or_else(|| PathBuf::from("games").join(id.to_string()));
    std::fs::create_dir_all(&path)?;
    Ok(path.canonicalize()?)
}
fn wallet(path: Option<&Path>) -> Result<PathBuf> {
    let path = path.ok_or("choose your wallet explicitly with a wallet name or --wallet FILE")?;
    path.canonicalize()
        .map_err(|_| format!("wallet {} not found; select an existing funded wallet with --wallet FILE", path.display()).into())
}

pub async fn host(
    a: &Artifact,
    c: &Config,
    keyfile: Option<&Path>,
    opponent: &str,
    dir: Option<PathBuf>,
    json: bool,
    shared: &mut node::rpc::startup::Connection,
    // ball_speed: BallSpeed, // Speed selection disabled.
) -> Result<()> {
    node::invitations::configured(c)?;
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let opponent = node::public(opponent)?;
    if opponent == pubkey(&keys.player) {
        return Err("opponent must use a different player key".into());
    }
    let ui = network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
    let n = shared.get(c).await?;
    let result = async {
    if ui.cancelled() { return Ok(()); }
    // Check that the explicitly configured directory is reachable BEFORE
    // locking funds. Its responses still carry no spending authority.
    node::invitations::addressed(a, c, &opponent).await?;
    let minimum = GAME_DEPOSIT.checked_add(DEFAULT_STAKE as u64).and_then(|v| v.checked_add(c.fee_sompi)).ok_or("fee cap overflow")?;
    let candidates = node::wallet::funding_candidates(&n, &keys.player, &keys.payer, minimum, c.coinbase_maturity_daa, &[]).await?;
    if ui.cancelled() { return Ok(()); }
    let funding = candidates
        .iter()
        .find(|f| f.utxo.amount >= minimum)
        .ok_or("no single mature coin covers the 5 KAS stake, 7 KAS operational collateral and fee cap; fund the selected host wallet")?;
    let lobby = Lobby {
        left: pubkey(&keys.player),
        right: opponent,
        refund: pubkey(&keys.payer),
        control_value: SIDE_DEPOSIT as i64,
        ball_tick_daa: DEFAULT_BALL_TICK_DAA,
        stake_per_player: DEFAULT_STAKE,
    };
    let funding_key = node::wallet::funding_key(funding, &keys.player, &keys.payer)?;
    let anchor = n.info().await?.sink;
    let genesis = priced_transaction(c, |fee| Ok(node::invitations::create(a, &c.network, anchor, &lobby, funding, &keys.player, funding_key, fee)?.genesis))?;
    let invite = node::invitations::from_transaction(a, &genesis)?;
    invite.validate(a)?;
    if ui.cancelled() { return Ok(()); }
    let dir = directory(dir, invite.genesis.id())?;
    let session = Session { wallet, directory: dir, side: 0, invitation: invite, start: None, code_received: None };
    if ["invitation.json", "left-state.json", "right-state.json"].iter().any(|name| session.directory.join(name).exists()) {
        return Err("game directory is already in use; start a fresh match in another directory".into());
    }
    // Public intent also prevents accidentally reusing another game's folder.
    save_new(&session.directory.join("invitation.json"), &session.invitation)?;
    node::hosted::remember_state(&state_path(&session), &c.network, &keys.player, &keys.payer)?;
    node::hosted::remember(&session.directory.join("invitation.json"), &session.invitation, node::client::now_ms())?;
    // Speed selection disabled: previously printed ball_speed.label().
    eprintln!("Locking your 5 KAS stake plus 7 KAS refundable operational collateral. Guest contributes another 5 KAS and pays the start fee.");
    eprintln!("Winner receives the 10 KAS pot at their player address; collateral returns to the host fee address. Network fees are additional.");
    if ui.cancelled() { return Ok(()); }
    // Complete this submission even if q arrives in flight. The public
    // invitation above durably records its exact transaction before sending.
    n.submit(&session.invitation.genesis).await?;
    play_connected(a, c, session, json, Some(n.clone()), Some(ui), shared).await
    }.await;
    result
}

pub async fn join(a: &Artifact, c: &Config, keyfile: Option<&Path>, code: &str, dir: Option<PathBuf>, json: bool, shared: &mut node::rpc::startup::Connection, entered: std::time::Instant) -> Result<()> {
    let lookup = std::time::Instant::now();
    node::invitations::configured(c)?;
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let ui = network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
    let waiting = format!("Looking up game {code}...\nWaiting for invitation acceptance/indexing; retrying automatically.\nq/Esc: main menu. No guest funding has been submitted.");
    ui.set_status(&waiting);
    eprintln!("{waiting}");
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let invite = loop {
        if ui.cancelled() { return Ok(()); }
        if let Some(invite) = node::invitations::discover_pending(a, c, &pubkey(&keys.player), code).await? {
            break invite;
        }
        if tokio::time::Instant::now() >= until {
            return Err("invitation not indexed after 60 seconds; check the shared directory/network and invited public key, then retry".into());
        }
        for _ in 0..5 {
            if ui.cancelled() { return Ok(()); }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    if ui.cancelled() { return Ok(()); }
    node::rpc::startup::phase("invitation_lookup_complete", lookup);
    ui.set_status("Preparing/synchronizing game setup. q/Esc: main menu.");
    let dir = directory(dir, invite.genesis.id())?;
    let session = Session { wallet, directory: dir, side: 1, invitation: invite, start: None, code_received: Some(entered) };
    save_public_invitation(&session.directory.join("invitation.json"), &session.invitation)?;
    let result = play_connected(a, c, session, json, None, Some(ui), shared).await;
    result
}

async fn play_connected(a: &Artifact, c: &Config, mut s: Session, json: bool, connection: Option<Node>, ui: Option<network_ui::Setup>, shared: &mut node::rpc::startup::Connection) -> Result<()> {
    let ui = match ui {
        Some(ui) => ui,
        None => network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?,
    };
    if ui.cancelled() { return Ok(()); }
    s.invitation.validate(a)?;
    if s.invitation.network != c.network { return Err("invitation network differs from configuration".into()); }
    let keys = node::wallet::Wallet::load(&s.wallet)?;
    let expected = if s.side == 0 { &s.invitation.lobby.left } else { &s.invitation.lobby.right };
    if &pubkey(&keys.player) != expected {
        return Err("session wallet is not the agreed player".into());
    }
    let path = state_path(&s);
    // Setup uses the same state lock as gameplay; no second signer can race it.
    let lock = node::lock(&path)?;
    node::hosted::remember_state(&path, &c.network, &keys.player, &keys.payer)?;
    if path.exists() {
        return Err("this game has already been opened by this player; start a fresh match".into());
    }
    let mut cache = Cache::new(&s.invitation);
    cache.validate(a, &s.invitation)?;

    let n = match connection {
        Some(n) => n,
        None => shared.get(c).await?,
    };
    let result = async {
    if ui.cancelled() { return Ok(()); }
    eprintln!("Synchronizing game setup...");
    // A cold guest need not replay from a potentially pruned creation anchor.
    // Its own node supplies the accepted exact Lobby UTXO and a fresh cursor.
    if s.side == 1 {
        let Some(bootstrapped) = n.bootstrap_lobby(a, &s.invitation, c, || ui.cancelled()).await? else { return Ok(()); };
        cache = bootstrapped;
    } else {
    let ready = cache.sync(a, &s.invitation, &n, c).await?.0;
    if !ready || matches!(cache.tracker.stage, Stage::AwaitingLobby) {
        if !synchronize(&mut cache, a, &s.invitation, &n, c, &ui).await? { return Ok(()); }
    }
    }
    if ui.cancelled() { return Ok(()); }
    if !matches!(cache.tracker.stage, Stage::Lobby(_)) {
        return Err("this invitation is no longer open; start a fresh match".into());
    }
    let observed = std::time::Instant::now();
    if let Stage::Lobby(lobby) = &cache.tracker.stage {
        if s.side == 0 {
            let code = node::invitations::code(s.invitation.genesis.id());
            eprintln!("Game code: {code}");
            eprintln!("Guest runs: .\\pong WALLET_NAME join {code}");
            eprintln!("Full code (only needed if the short code is ambiguous): {}", node::invitations::full_code(s.invitation.genesis.id()));
        } else {
            let p = setup::prepare(a, &s.invitation, lobby)?;
            if !n.sources_unspent(&p.transaction, &p.inputs()).await? { return Err("lobby is no longer open".into()); }
            eprintln!("Verified host {} for game {}. Each player stakes 5 KAS; winner receives 10 KAS.",
                secp256k1::XOnlyPublicKey::from_slice(&s.invitation.lobby.left)?, s.invitation.genesis.id());
            let preparation = std::time::Instant::now();
            let minimum = (DEFAULT_STAKE as u64).checked_add(1).ok_or("stake overflow")?;
            let candidates = node::wallet::funding_candidates(&n, &keys.player, &keys.payer, minimum, c.coinbase_maturity_daa, &[]).await?;
            if ui.cancelled() { return Ok(()); }
            // Try each owned address with its actual key. No host coin is
            // supplied to the guest's funding approval.
            let artifact = a.clone();
            let invitation = s.invitation.clone();
            let player = keys.player;
            let fee_key = keys.payer;
            let config = c.clone();
            let approved = tokio::task::spawn_blocking(move || {
                let mut issue = "no owned guest coin covers 5 KAS plus the start fee".to_string();
                for payer in [&fee_key, &player] {
                    match setup::approve(&artifact, &invitation, p.clone(), candidates.clone(), &player, payer, &config) {
                        Ok(package) => return Ok(package),
                        Err(error) => issue = error.to_string(),
                    }
                    if pubkey(&fee_key) == pubkey(&player) { break; }
                }
                Err::<setup::StartPackage, String>(issue)
            }).await?.map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            if ui.cancelled() { return Ok(()); }
            if !n.sources_unspent(&approved.transaction, &approved.inputs()).await? {
                return Err("launch funding changed before signing could be submitted; create a fresh match".into());
            }
            eprintln!("Contributing your 5 KAS stake; external start fee {} KAS. Winnings/refunded stake arrive at your player address, spendable by this wallet.", node::fees::coins(approved.fee as u128));
            s.start = Some(approved);
            node::rpc::startup::phase("guest_funding_preparation_complete", preparation);
        }
    }
    if ui.cancelled() { return Ok(()); }
    let persistence = std::time::Instant::now();
    node::save(&path, &cache)?;
    node::rpc::startup::phase("setup_required_persistence", persistence);
    let startup = if s.side == 1 { s.start } else { None };
    let terminal = !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let prepared = PreparedRun { lock, cache, connection: n.clone(), observed, code_received: s.code_received };
    // Join the setup reader and restore its screen before gameplay reads keys.
    drop(ui);
    run(a, c, s.invitation, path, false, false, false, None, terminal, Some(&s.wallet), startup, Some(prepared), shared).await
    }.await;
    result
}
