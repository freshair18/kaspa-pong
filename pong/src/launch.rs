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

// Read-only setup operations may be dropped immediately on Escape. Durable
// authorization and transaction submission are deliberately never wrapped.
macro_rules! setup_read {
    ($ui:expr, $future:expr) => {
        match $ui.read($future).await { Some(result) => result?, None => return Ok(()) }
    };
}

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
fn default_directory(id: kaspa_consensus_core::Hash, side: usize) -> PathBuf {
    PathBuf::from("games").join(format!("{id}-{}", if side == 0 { "left" } else { "right" }))
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn local_players_get_separate_recovery_discoverable_directories() {
        let id = kaspa_consensus_core::Hash::from_bytes([7; 32]);
        let left = default_directory(id, 0);
        let right = default_directory(id, 1);
        assert_ne!(left, right);
        assert_eq!(left.parent(), Some(Path::new("games")));
        assert_eq!(right.parent(), Some(Path::new("games")));
        let explicit = tempfile::tempdir().unwrap();
        assert_eq!(directory(Some(explicit.path().to_path_buf()), id, 1).unwrap(), explicit.path().canonicalize().unwrap());
    }
}

fn directory(path: Option<PathBuf>, id: kaspa_consensus_core::Hash, side: usize) -> Result<PathBuf> {
    let path = path.unwrap_or_else(|| default_directory(id, side));
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
    dir: Option<PathBuf>,
    json: bool,
    shared: &mut node::rpc::startup::Connection,
    // ball_speed: BallSpeed, // Speed selection disabled.
) -> Result<()> {
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let game_player = node::wallet::Wallet::game_key()?;
    let code = node::invitations::player_code(&pubkey(&game_player))?;
    let pending = PathBuf::from("games").join(format!("pending-{}.wallet.json", game_player.x_only_public_key().0));
    node::wallet::Wallet { player: game_player, payer: keys.payer }.save_new(&pending)?;
    let ui = network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
    let n = setup_read!(ui, shared.get(c));
    let cursor = setup_read!(ui, n.accepted_cursor());
    let waiting = format!("Share this game code with the guest:\n{code}\n\nWaiting for the guest's accepted Lobby transaction. q/Esc: main menu. No funds are locked.");
    ui.set_status(&waiting);
    eprintln!("{waiting}");
    let Some(invite) = wait_for_lobby(a, c, &n, cursor, &pubkey(&game_player), &ui).await? else { return Ok(()); };
    let dir = directory(dir, invite.genesis.id(), 1)?;
    let game_wallet = dir.join("right-game.wallet.json");
    if game_wallet.exists() || ["invitation.json", "left-state.json", "right-state.json"].iter().any(|name| dir.join(name).exists()) {
        return Err("game directory is already in use; start a fresh match in another directory".into());
    }
    std::fs::rename(&pending, &game_wallet)?;
    save_public_invitation(&dir.join("invitation.json"), &invite)?;
    let session = Session { wallet: game_wallet, directory: dir, side: 1, invitation: invite, start: None, code_received: None };
    play_connected(a, c, session, json, Some(n), Some(ui), shared).await
}

pub async fn join(a: &Artifact, c: &Config, keyfile: Option<&Path>, code: &str, dir: Option<PathBuf>, json: bool, shared: &mut node::rpc::startup::Connection, entered: std::time::Instant) -> Result<()> {
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let host = node::invitations::parse_player_code(code)?;
    if host == pubkey(&keys.player) { return Err("a game code cannot name your current player key".into()); }
    let ui = network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
    let n = setup_read!(ui, shared.get(c));
    let minimum = (DEFAULT_STAKE as u64).checked_add(c.fee_sompi).ok_or("fee cap overflow")?;
    let candidates = setup_read!(ui, node::wallet::funding_candidates(&n, &keys.player, &keys.payer, minimum, c.coinbase_maturity_daa, &[]));
    let (candidates, claim) = node::reservations::available(candidates, false)?;
    let funding = candidates.iter().find(|f| f.utxo.amount >= minimum)
        .ok_or("insufficient AVAILABLE guest funds: need a single unreserved mature coin covering 5 KAS plus fees")?;
    let lobby = Lobby { left: pubkey(&keys.player), right: host, refund: pubkey(&keys.payer), control_value: SIDE_DEPOSIT as i64,
        ball_tick_daa: DEFAULT_BALL_TICK_DAA, stake_per_player: DEFAULT_STAKE };
    let anchor = setup_read!(ui, n.info()).sink;
    let funding_key = node::wallet::funding_key(funding, &keys.player, &keys.payer)?;
    let genesis = priced_transaction(c, |fee| Ok(node::invitations::create(a, &c.network, anchor, &lobby, funding, &keys.player, funding_key, fee)?.genesis))?;
    let invite = node::invitations::from_transaction(a, &genesis)?;
    let dir = directory(dir, invite.genesis.id(), 0)?;
    let session = Session { wallet, directory: dir, side: 0, invitation: invite, start: None, code_received: Some(entered) };
    if ["invitation.json", "left-state.json", "right-state.json"].iter().any(|name| session.directory.join(name).exists()) {
        return Err("game directory is already in use; start a fresh match in another directory".into());
    }
    if !setup_read!(ui, n.sources_unspent(&session.invitation.genesis, &[funding.utxo.clone()])) { return Err("guest funding changed before submission; retry with available funds".into()); }
    save_new(&session.directory.join("creation-authorization.json"), &serde_json::json!({"transaction": session.invitation.genesis, "inputs": [funding.utxo.clone()]}))?;
    save_new(&session.directory.join("invitation.json"), &session.invitation)?;
    node::hosted::remember_state(&state_path(&session), &c.network, &keys.player, &keys.payer)?;
    node::hosted::remember(&session.directory.join("invitation.json"), &session.invitation, node::client::now_ms())?;
    drop(claim);
    eprintln!("Locking your 5 KAS stake. The host will contribute the matching stake and start the game.");
    n.submit(&session.invitation.genesis).await?;
    let invitation_path = session.directory.join("invitation.json");
    let result = play_connected(a, c, session, json, Some(n), Some(ui), shared).await;
    // The Lobby creator controls its unjoined refund. Queue that recovery when
    // this short-lived session leaves; a started game is never cancelled here.
    node::hosted::request_cancel(&invitation_path, &c.network, &pubkey(&keys.player))?;
    result
}

/// The host starts this watcher before it discloses its fresh public key. Only
/// accepted Full-chain additions are considered; an index server is never in
/// the trust or availability path.
async fn wait_for_lobby(
    a: &Artifact,
    c: &Config,
    n: &Node,
    mut cursor: kaspa_consensus_core::Hash,
    player: &[u8],
    ui: &network_ui::Setup,
) -> Result<Option<Invitation>> {
    loop {
        if ui.cancelled() { return Ok(None); }
        let delta = match ui.read(n.delta(cursor)).await {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                ui.set_status(&format!("Waiting for the guest's accepted Lobby transaction. Retrying node query.\n{error}\nq/Esc: main menu. No funds are locked."));
                tokio::time::sleep(std::time::Duration::from_millis(c.poll_ms)).await;
                continue;
            }
            None => return Ok(None),
        };
        if let Some(common) = delta.common { cursor = common; }
        for block in &delta.added {
            for accepted in &block.transactions {
                if !accepted.tx.payload.starts_with(b"PONGI") { continue; }
                let Ok(invite) = node::invitations::from_transaction(a, &accepted.tx) else { continue; };
                if invite.network == c.network && invite.lobby.right == player {
                    return Ok(Some(invite));
                }
            }
            cursor = block.hash;
        }
        tokio::time::sleep(std::time::Duration::from_millis(c.poll_ms)).await;
    }
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
        None => setup_read!(ui, shared.get(c)),
    };
    let mut _start_claim = None;
    let result = async {
    if ui.cancelled() { return Ok(()); }
    eprintln!("Synchronizing game setup...");
    // A cold guest need not replay from a potentially pruned creation anchor.
    // Its own node supplies the accepted exact Lobby UTXO and a fresh cursor.
    if s.side == 1 {
        let Some(bootstrapped) = setup_read!(ui, n.bootstrap_lobby(a, &s.invitation, c, || ui.cancelled())) else { return Ok(()); };
        cache = bootstrapped;
    } else {
    let ready = setup_read!(ui, cache.sync(a, &s.invitation, &n, c)).0;
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
            eprintln!("Lobby accepted. Waiting for the host to fund and start the game.");
        } else {
            let p = setup::prepare(a, &s.invitation, lobby)?;
            if !setup_read!(ui, n.sources_unspent(&p.transaction, &p.inputs())) { return Err("lobby is no longer open".into()); }
            eprintln!("Verified host {} for game {}. Each player stakes 5 KAS; winner receives 10 KAS.",
                secp256k1::XOnlyPublicKey::from_slice(&s.invitation.lobby.left)?, s.invitation.genesis.id());
            let preparation = std::time::Instant::now();
            let minimum = (DEFAULT_STAKE as u64).checked_add(1).ok_or("stake overflow")?;
            let candidates = setup_read!(ui, node::wallet::funding_candidates(&n, &keys.player, &keys.payer, minimum, c.coinbase_maturity_daa, &[]));
            let (candidates, claim) = node::reservations::available(candidates, true)?;
            if ui.cancelled() { return Ok(()); }
            // Try each owned address with its actual key. No host coin is
            // supplied to the guest's funding approval.
            let artifact = a.clone();
            let invitation = s.invitation.clone();
            let player = keys.player;
            let fee_key = keys.payer;
            let config = c.clone();
            let (approved, mut claim) = setup_read!(ui, tokio::task::spawn_blocking(move || {
                let mut issue = "no owned guest coin covers 5 KAS plus the start fee".to_string();
                for payer in [&fee_key, &player] {
                    match setup::approve(&artifact, &invitation, p.clone(), candidates.clone(), &player, payer, &config) {
                        Ok(package) => return Ok((package, claim)),
                        Err(error) => issue = error.to_string(),
                    }
                    if pubkey(&fee_key) == pubkey(&player) { break; }
                }
                Err::<(setup::StartPackage, node::reservations::Claim), String>(issue)
            })).map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            claim.restrict(approved.transaction.inputs.iter().map(|i| i.previous_outpoint).collect())?;
            _start_claim = Some(claim);
            if ui.cancelled() { return Ok(()); }
            if !setup_read!(ui, n.sources_unspent(&approved.transaction, &approved.inputs())) {
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
