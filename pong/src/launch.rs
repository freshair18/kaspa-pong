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
        match $ui.read($future).await { Some(result) => result?, None => return Ok(Default::default()) }
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

/// Read-only rematch preparation. Owning this never authorizes a transaction.
pub struct Rematch {
    host: Vec<u8>,
    guest: Vec<u8>,
    previous_tag: [u8; node::invitations::RENDEZVOUS_BYTES],
    terms: node::invitations::GameTerms,
    offer: Option<(String, [u8; node::invitations::RENDEZVOUS_BYTES])>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<tokio::task::JoinHandle<std::result::Result<Invitation, String>>>,
    request: Option<network_ui::RematchRequest>,
}
impl Rematch {
    pub fn new(invite: &Invitation, side: i64) -> Result<Self> {
        let terms = node::invitations::GameTerms::from_lobby(&invite.lobby)?;
        let offer = if side == 1 {
            let tag = node::invitations::new_rendezvous_tag(terms)?;
            Some((node::invitations::rendezvous_code(&invite.lobby.right, &tag)?, tag))
        } else { None };
        Ok(Self { host: invite.lobby.right.clone(), guest: invite.lobby.left.clone(), terms, offer,
            previous_tag: node::invitations::rendezvous_tag(&invite.genesis)?,
            ready: Default::default(), watcher: None, request: None })
    }
    pub fn prepare(&mut self, a: &Artifact, c: &Config, n: &Node, cursor: kaspa_consensus_core::Hash) {
        let Some((_, tag)) = &self.offer else { return; };
        if self.watcher.is_some() { return; }
        let (a, c, n) = (a.clone(), c.clone(), n.clone());
        let (host, guest, terms, tag) = (self.host.clone(), self.guest.clone(), self.terms, *tag);
        self.watcher = Some(tokio::spawn(async move {
            let mut cursor = cursor;
            loop {
                if let Some(delta) = n.delta(cursor).await.ok() {
                    if let Some(common) = delta.common { cursor = common; }
                    for block in &delta.added {
                        for accepted in &block.transactions {
                            if !accepted.tx.payload.starts_with(b"PONGI") { continue; }
                            let Ok(invite) = node::invitations::from_transaction(&a, &accepted.tx) else { continue; };
                            if rematch_lobby_matches(&invite, &c.network, &host, &guest, terms, &tag) {
                                return Ok(invite);
                            }
                        }
                        cursor = block.hash;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(c.poll_ms.max(25))).await;
            }
        }));
        // The coordinator supplies its already-validated accepted-chain cursor.
        // No extra RPC round trip is needed before the completion screen.
        self.ready.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn screen_offer(&self) -> Option<network_ui::RematchOffer> {
        match &self.offer {
            Some((code, _)) if self.ready.load(std::sync::atomic::Ordering::Acquire) =>
                Some(network_ui::RematchOffer::Host { code: code.clone(), summary: self.terms.summary() }),
            Some(_) => None,
            None => Some(network_ui::RematchOffer::Guest),
        }
    }
    pub fn requested(mut self, request: Option<network_ui::RematchRequest>) -> Option<Self> {
        self.request = request;
        self.request.as_ref()?;
        Some(self)
    }
    pub fn invalidate(&mut self) {
        // This rollback belongs to the previous game, not the offered rematch.
        // Keep the public code and its independent, fork-aware Lobby watcher.
        self.request = None;
    }
}
impl Drop for Rematch {
    fn drop(&mut self) { if let Some(task) = &self.watcher { task.abort(); } }
}

fn rematch_lobby_matches(invite: &Invitation, network: &str, host: &[u8], guest: &[u8],
    terms: node::invitations::GameTerms, tag: &[u8; node::invitations::RENDEZVOUS_BYTES]) -> bool {
    invite.network == network && invite.lobby.right == host && invite.lobby.left == guest
        && terms.matches(&invite.lobby) && node::invitations::rendezvous_tag(&invite.genesis).ok().as_ref() == Some(tag)
}

enum NextMatch {
    Host { settings: kaspa_pong::menu::GameSettings, rematch: Option<Rematch> },
    Join { code: String, entered: std::time::Instant, expected: Option<(Vec<u8>, node::invitations::GameTerms, [u8; node::invitations::RENDEZVOUS_BYTES])> },
}
async fn matches(a: &Artifact, c: &Config, keyfile: Option<&Path>, mut dir: Option<PathBuf>, json: bool,
    shared: &mut node::rpc::startup::Connection, mut next: NextMatch) -> Result<()> {
    loop {
        let again = match next {
            NextMatch::Host { settings, rematch } => host_once(a, c, keyfile, dir.take(), json, shared, settings, rematch).await?,
            NextMatch::Join { code, entered, expected } => {
                if let Some((host, terms, previous)) = expected { validate_rematch_code(&code, &host, terms, &previous)?; }
                join_once(a, c, keyfile, &code, dir.take(), json, shared, entered).await?
            }
        };
        let Some(mut rematch) = again else { return Ok(()); };
        next = match rematch.request.take() {
            Some(network_ui::RematchRequest::Host) => {
                let settings = kaspa_pong::menu::GameSettings { speed: rematch.terms.speed,
                    ball_count: rematch.terms.ball_count, goal_limit: rematch.terms.win_score };
                NextMatch::Host { settings, rematch: Some(rematch) }
            }
            Some(network_ui::RematchRequest::Join(code)) => NextMatch::Join { code, entered: std::time::Instant::now(), expected: Some((rematch.host.clone(), rematch.terms, rematch.previous_tag)) },
            None => return Ok(()),
        };
    }
}

fn validate_rematch_code(code: &str, expected_host: &[u8], expected_terms: node::invitations::GameTerms,
    previous_tag: &[u8; node::invitations::RENDEZVOUS_BYTES]) -> Result<()> {
    let (host, tag, terms) = node::invitations::parse_rendezvous_code(code)?;
    if host != expected_host { return Err("rematch code belongs to a different host".into()); }
    if terms != expected_terms { return Err("rematch code changes the previous game settings; use Join from the menu for a different game".into()); }
    if tag == *previous_tag { return Err("that code belongs to the completed game; enter the host's fresh rematch code".into()); }
    Ok(())
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

    fn invitation() -> Invitation {
        use secp256k1::{Keypair, Secp256k1, SecretKey};
        use kaspa_consensus_core::{Hash, tx::{TransactionOutpoint, UtxoEntry}};
        let key = |n| Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap());
        let a: Artifact = serde_json::from_str(include_str!("../artifacts/artifact.json")).unwrap();
        let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(1)),
            control_value: SIDE_DEPOSIT as i64, ball_tick_daa: 15, stake_per_player: DEFAULT_STAKE, ball_count: 8, win_score: 15 };
        let funds = Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([90; 32]), 0),
            utxo: UtxoEntry::new(2_000_000_000, p2pk(&pubkey(&key(1))), 1, false, None) };
        node::invitations::create(&a, "devnet", Hash::from_bytes([0; 32]), &lobby, &funds, &key(1), &key(1), 100_000).unwrap()
    }

    #[test]
    fn host_rematch_offer_is_read_only_hidden_until_watching_and_keeps_terms() {
        let invite = invitation();
        let host = Rematch::new(&invite, 1).unwrap();
        assert!(host.screen_offer().is_none());
        assert!(host.watcher.is_none());
        host.ready.store(true, std::sync::atomic::Ordering::Release);
        let Some(network_ui::RematchOffer::Host { code, .. }) = host.screen_offer() else { panic!("host code missing") };
        let (key, _, terms) = node::invitations::parse_rendezvous_code(&code).unwrap();
        assert_eq!(key, invite.lobby.right);
        assert!(terms.matches(&invite.lobby));
        assert!(host.requested(None).is_none(), "leaving the result screen does not approve a rematch");
        let guest = Rematch::new(&invite, 0).unwrap();
        assert!(matches!(guest.screen_offer(), Some(network_ui::RematchOffer::Guest)));
    }

    #[test]
    fn rematch_requires_same_players_terms_network_and_fresh_tag() {
        let invite = invitation();
        let terms = node::invitations::GameTerms::from_lobby(&invite.lobby).unwrap();
        let tag = node::invitations::rendezvous_tag(&invite.genesis).unwrap();
        let matches = |candidate: &Invitation| rematch_lobby_matches(candidate, "devnet", &invite.lobby.right, &invite.lobby.left, terms, &tag);
        assert!(matches(&invite));
        for field in 0..6 {
            let mut changed = invite.clone();
            match field {
                0 => changed.lobby.right[0] ^= 1,
                1 => changed.lobby.left[0] ^= 1,
                2 => changed.network = "testnet-10".into(),
                3 => changed.lobby.ball_count = 7,
                4 => changed.lobby.ball_tick_daa = 1,
                _ => changed.lobby.win_score = 14,
            }
            assert!(!matches(&changed));
        }
        let mut another = tag; another[7] ^= 1;
        assert!(!rematch_lobby_matches(&invite, "devnet", &invite.lobby.right, &invite.lobby.left, terms, &another));
    }

    #[test]
    fn guest_rematch_rejects_changed_configuration_and_reused_game_code() {
        let invite = invitation();
        let terms = node::invitations::GameTerms::from_lobby(&invite.lobby).unwrap();
        let previous = node::invitations::rendezvous_tag(&invite.genesis).unwrap();
        let mut fresh = previous; fresh[7] ^= 1;
        let code = node::invitations::rendezvous_code(&invite.lobby.right, &fresh).unwrap();
        validate_rematch_code(&code, &invite.lobby.right, terms, &previous).unwrap();
        let old = node::invitations::rendezvous_code(&invite.lobby.right, &previous).unwrap();
        assert!(validate_rematch_code(&old, &invite.lobby.right, terms, &previous).is_err());
        assert!(validate_rematch_code(&code, &invite.lobby.left, terms, &previous).is_err());
        for changed in [node::invitations::GameTerms { ball_count: 7, ..terms },
            node::invitations::GameTerms { speed: GameSpeed::SuperFast, ..terms },
            node::invitations::GameTerms { win_score: 14, ..terms }] {
            assert!(validate_rematch_code(&code, &invite.lobby.right, changed, &previous).is_err());
        }
    }

    #[tokio::test]
    async fn abandoned_rematch_aborts_only_the_read_only_watcher() {
        let mut rematch = Rematch::new(&invitation(), 1).unwrap();
        let task = tokio::spawn(std::future::pending::<std::result::Result<Invitation, String>>());
        let abort = task.abort_handle();
        rematch.watcher = Some(task);
        let old_ready = rematch.ready.clone();
        rematch.ready.store(true, std::sync::atomic::Ordering::Release);
        let published = rematch.screen_offer();
        let original_tag = rematch.offer.as_ref().unwrap().1;
        rematch.request = Some(network_ui::RematchRequest::Host);
        rematch.invalidate();
        assert!(std::sync::Arc::ptr_eq(&old_ready, &rematch.ready));
        assert!(rematch.watcher.is_some());
        assert!(!abort.is_finished(), "rolling back the old game must not cancel the next Lobby watcher");
        assert!(rematch.request.is_none());
        assert_eq!(rematch.screen_offer(), published, "a shared rematch code must stay stable");
        let rematch = rematch.requested(Some(network_ui::RematchRequest::Host)).unwrap();
        let (waiting_code, waiting_tag) = rematch.offer.as_ref().unwrap();
        let Some(network_ui::RematchOffer::Host { code, .. }) = published else { panic!("host offer missing") };
        assert_eq!(waiting_code, &code);
        assert_eq!(*waiting_tag, original_tag);
        let waiting = host_waiting_status(rematch.terms, waiting_code, true);
        assert!(waiting.contains("REMATCH CONFIRMED"));
        assert!(waiting.contains(&code));
        assert!(!waiting.contains("Share this game code"));
        drop(rematch);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !abort.is_finished() { tokio::task::yield_now().await; }
        }).await.unwrap();
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

fn guest_review(terms: &node::invitations::GameTerms, host: &[u8], network: &str) -> String {
    format!("Welcome to Kaspa Pong\n\nGAME SETTINGS\nBalls: {}   First to {} goals\nSpeed: {} ({} DAA per ball step)\nYour stake: {} KAS   Winner's pot: {} KAS\nNetwork fees are extra.\nNetwork: {}\nHost:\n{}",
        terms.ball_count, terms.win_score, terms.speed.label(), terms.speed.tick_daa(),
        terms.stake_kas, u16::from(terms.stake_kas) * 2, network,
        host.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

fn host_waiting_status(terms: node::invitations::GameTerms, code: &str, rematch: bool) -> String {
    if rematch {
        format!("REMATCH CONFIRMED - WAITING FOR GUEST\n{}\n\nShare this rematch code with your opponent:\nCODE: {code}\n\nWaiting for your guest to confirm.\nq/Esc: main menu.", terms.summary())
    } else {
        format!("HOSTING A NEW GAME\n{}\n\nNEXT STEP - SHARE THIS CODE WITH YOUR GUEST\nCODE: {code}\n\nWaiting for your guest to confirm.\nThe game starts automatically once both stakes are ready.\nq/Esc: main menu. No funds are locked.", terms.summary())
    }
}

pub async fn host(
    a: &Artifact,
    c: &Config,
    keyfile: Option<&Path>,
    dir: Option<PathBuf>,
    json: bool,
    shared: &mut node::rpc::startup::Connection,
    settings: kaspa_pong::menu::GameSettings,
) -> Result<()> {
    matches(a, c, keyfile, dir, json, shared, NextMatch::Host { settings, rematch: None }).await
}
async fn host_once(a: &Artifact, c: &Config, keyfile: Option<&Path>, dir: Option<PathBuf>, json: bool,
    shared: &mut node::rpc::startup::Connection, settings: kaspa_pong::menu::GameSettings, mut rematch: Option<Rematch>) -> Result<Option<Rematch>> {
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let player = keys.player;
    let terms = settings.terms();
    terms.validate()?;
    let (code, rendezvous) = if let Some(rematch) = &rematch {
        rematch.offer.clone().ok_or("host rematch lacks a game code")?
    } else {
        let tag = node::invitations::new_rendezvous_tag(terms)?;
        (node::invitations::rendezvous_code(&pubkey(&player), &tag)?, tag)
    };
    let interactive = !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let ui = network_ui::Setup::new(interactive)?;
    let n = setup_read!(ui, shared.get(c));
    let cursor = if rematch.is_none() { Some(setup_read!(ui, n.accepted_cursor())) } else { None };
    let waiting = host_waiting_status(terms, &code, rematch.is_some());
    ui.set_status(&waiting);
    if !interactive { eprintln!("{waiting}"); }
    let invitation = if let Some(rematch) = &mut rematch {
        let watcher = rematch.watcher.as_mut().ok_or("rematch watcher was not started")?;
        match ui.read(watcher).await {
            Some(result) => Some(result.map_err(|e| e.to_string())?.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?),
            None => None,
        }
    } else { wait_for_lobby(a, c, &n, cursor.expect("fresh host cursor"), &pubkey(&player), &rendezvous, terms, &ui).await? };
    let Some(invite) = invitation else { return Ok(None); };
    let dir = directory(dir, invite.genesis.id(), 1)?;
    if ["invitation.json", "left-state.json", "right-state.json"].iter().any(|name| dir.join(name).exists()) {
        return Err("game directory is already in use; start a fresh match in another directory".into());
    }
    save_public_invitation(&dir.join("invitation.json"), &invite)?;
    let session = Session { wallet, directory: dir, side: 1, invitation: invite, start: None, code_received: None };
    play_connected(a, c, session, json, Some(n), Some(ui), shared).await
}

pub async fn join(a: &Artifact, c: &Config, keyfile: Option<&Path>, code: &str, dir: Option<PathBuf>, json: bool, shared: &mut node::rpc::startup::Connection, entered: std::time::Instant) -> Result<()> {
    matches(a, c, keyfile, dir, json, shared, NextMatch::Join { code: code.into(), entered, expected: None }).await
}
async fn join_once(a: &Artifact, c: &Config, keyfile: Option<&Path>, code: &str, dir: Option<PathBuf>, json: bool,
    shared: &mut node::rpc::startup::Connection, entered: std::time::Instant) -> Result<Option<Rematch>> {
    let wallet = wallet(keyfile)?;
    let keys = node::wallet::Wallet::load(&wallet)?;
    let (host, rendezvous, terms) = node::invitations::parse_rendezvous_code(code)?;
    if host == pubkey(&keys.player) { return Err("a game code cannot name your current player key".into()); }
    let review = guest_review(&terms, &host, &c.network);
    let ui = network_ui::Setup::join_review(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal(), review)?;
    ui.set_status("Preparing your transaction. Nothing has been sent.");
    eprintln!("Joining: {}", terms.summary());
    let n = setup_read!(ui, shared.get(c));
    let minimum = (DEFAULT_STAKE as u64).checked_add(c.fee_sompi).ok_or("fee cap overflow")?;
    let candidates = setup_read!(ui, node::wallet::funding_candidates(&n, &keys.player, &keys.player, minimum, c.coinbase_maturity_daa, &[]));
    let (candidates, claim) = node::reservations::available(candidates, false)?;
    let funding = candidates.iter().find(|f| f.utxo.amount >= minimum)
        .ok_or("insufficient AVAILABLE guest funds: need a single unreserved mature coin covering 5 KAS plus fees")?;
    let lobby = Lobby { left: pubkey(&keys.player), right: host, refund: pubkey(&keys.player), control_value: SIDE_DEPOSIT as i64,
        ball_tick_daa: terms.speed.tick_daa(), stake_per_player: DEFAULT_STAKE,
        ball_count: i64::from(terms.ball_count), win_score: i64::from(terms.win_score) };
    let anchor = setup_read!(ui, n.info()).sink;
    let funding_key = node::wallet::funding_key(funding, &keys.player, &keys.player)?;
    let genesis = priced_transaction(c, |fee| Ok(node::invitations::create_with_rendezvous(a, &c.network, anchor, &lobby, funding, &keys.player, funding_key, fee, rendezvous)?.genesis))?;
    let invite = node::invitations::from_transaction(a, &genesis)?;
    let creation_fee = node::invitations::creation_payment(&invite)?.sompi;
    ui.set_status(&format!("Network fee: {} KAS.", node::fees::coins(creation_fee as u128)));
    if !ui.confirm().await? { return Ok(None); }
    let dir = directory(dir, invite.genesis.id(), 0)?;
    let session = Session { wallet, directory: dir, side: 0, invitation: invite, start: None, code_received: Some(entered) };
    if ["invitation.json", "left-state.json", "right-state.json"].iter().any(|name| session.directory.join(name).exists()) {
        return Err("game directory is already in use; start a fresh match in another directory".into());
    }
    if !setup_read!(ui, n.sources_unspent(&session.invitation.genesis, &[funding.utxo.clone()])) { return Err("joining-player funding changed before submission; retry with available funds".into()); }
    // Retire the review reader and drain queued cancellation before authorization.
    if ui.finish() { return Ok(None); }
    let ui = network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
    ui.set_status("Joining confirmed. Submitting your approved stake transaction; then waiting for the host.");
    save_new(&session.directory.join("creation-authorization.json"), &serde_json::json!({"transaction": session.invitation.genesis, "inputs": [funding.utxo.clone()]}))?;
    save_new(&session.directory.join("invitation.json"), &session.invitation)?;
    node::hosted::remember_state(&state_path(&session), &c.network, &keys.player, &keys.player)?;
    node::hosted::remember(&session.directory.join("invitation.json"), &session.invitation, node::client::now_ms())?;
    drop(claim);
    eprintln!("Locking your 5 KAS stake. The host will contribute the matching stake and start the game.");
    if let Err(error) = n.submit(&session.invitation.genesis).await {
        // A node's explicit transaction rejection proves this Lobby was never
        // admitted. Remove the pre-submit recovery record so its funding coin
        // cannot be falsely reserved after restart. Transport failures remain
        // ambiguous and deliberately retain recovery evidence.
        if error.to_string().to_ascii_lowercase().contains("rejected transaction") {
            node::hosted::discard_rejected_creation(
                &session.directory.join("invitation.json"),
                &state_path(&session),
                &c.network,
                &keys.player,
                &keys.player,
            )?;
        }
        return Err(error);
    }
    let invitation_path = session.directory.join("invitation.json");
    let result = play_connected(a, c, session, json, Some(n), Some(ui), shared).await;
    // The Lobby creator controls its unjoined refund. Queue that recovery when
    // this short-lived session leaves; a started game is never cancelled here.
    let cancellation = node::hosted::request_cancel(&invitation_path, &c.network, &pubkey(&keys.player));
    match (result, cancellation) {
        (Err(game), Err(cancel)) => Err(format!("{game}; could not queue invitation recovery: {cancel}").into()),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(rematch), Ok(())) => Ok(rematch),
    }
}

/// The host starts this watcher before it discloses its fresh rendezvous code. Only
/// accepted Full-chain additions are considered; an index server is never in
/// the trust or availability path.
async fn wait_for_lobby(
    a: &Artifact,
    c: &Config,
    n: &Node,
    mut cursor: kaspa_consensus_core::Hash,
    player: &[u8],
    rendezvous: &[u8; node::invitations::RENDEZVOUS_BYTES],
    terms: node::invitations::GameTerms,
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
                if invite.network == c.network && invite.lobby.right == player && terms.matches(&invite.lobby) && node::invitations::rendezvous_tag(&accepted.tx)? == *rendezvous {
                    return Ok(Some(invite));
                }
            }
            cursor = block.hash;
        }
        tokio::time::sleep(std::time::Duration::from_millis(c.poll_ms)).await;
    }
}

async fn play_connected(a: &Artifact, c: &Config, mut s: Session, json: bool, connection: Option<Node>, ui: Option<network_ui::Setup>, shared: &mut node::rpc::startup::Connection) -> Result<Option<Rematch>> {
    let ui = match ui {
        Some(ui) => ui,
        None => network_ui::Setup::new(!json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?,
    };
    if ui.cancelled() { return Ok(None); }
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
    node::hosted::remember_state(&path, &c.network, &keys.player, &keys.player)?;
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
    if ui.cancelled() { return Ok(None); }
    eprintln!("Synchronizing game setup...");
    // A cold host need not replay from a potentially pruned creation anchor.
    // Its own node supplies the accepted exact Lobby UTXO and a fresh cursor.
    if s.side == 1 {
        let Some(bootstrapped) = setup_read!(ui, n.bootstrap_lobby(a, &s.invitation, c, || ui.cancelled())) else { return Ok(None); };
        cache = bootstrapped;
    } else {
    let ready = setup_read!(ui, cache.sync(a, &s.invitation, &n, c)).0;
    if !ready || matches!(cache.tracker.stage, Stage::AwaitingLobby) {
        if !synchronize(&mut cache, a, &s.invitation, &n, c, &ui).await? { return Ok(None); }
    }
    }
    if ui.cancelled() { return Ok(None); }
    if !matches!(cache.tracker.stage, Stage::Lobby(_)) {
        return Err("this invitation is no longer open; start a fresh match".into());
    }
    let observed = std::time::Instant::now();
    if let Stage::Lobby(lobby) = &cache.tracker.stage {
        if s.side == 0 {
            eprintln!("Lobby accepted. Waiting for the host to fund and start the game.");
        } else {
            let p = setup::prepare(a, &s.invitation, lobby)?;
            eprintln!("Verified joining player {} for game {}. Each player stakes 5 KAS; winner receives 10 KAS.",
                secp256k1::XOnlyPublicKey::from_slice(&s.invitation.lobby.left)?, s.invitation.genesis.id());
            let preparation = std::time::Instant::now();
            let minimum = (DEFAULT_STAKE as u64).checked_add(1).ok_or("stake overflow")?;
            let candidates = setup_read!(ui, node::wallet::funding_candidates(&n, &keys.player, &keys.player, minimum, c.coinbase_maturity_daa, &[]));
            let (candidates, claim) = node::reservations::available(candidates, true)?;
            if ui.cancelled() { return Ok(None); }
            let artifact = a.clone();
            let invitation = s.invitation.clone();
            let player = keys.player;
            let config = c.clone();
            let check_node = n.clone();
            let check_transaction = p.transaction.clone();
            let check_inputs = p.inputs();
            let preliminary_check = tokio::spawn(async move {
                check_node.sources_unspent(&check_transaction, &check_inputs).await.map_err(|error| error.to_string())
            });
            let approval = tokio::task::spawn_blocking(move || {
                let package = setup::approve(&artifact, &invitation, p, candidates, &player, &player, &config)
                    .map_err(|error| error.to_string())?;
                Ok::<(setup::StartPackage, node::reservations::Claim), String>((package, claim))
            });
            let (lobby_open, (approved, mut claim)) = setup_read!(ui, async {
                let (lobby_open, approval) = tokio::try_join!(
                    async { preliminary_check.await.map_err(|error| error.to_string())? },
                    async { approval.await.map_err(|error| error.to_string())? },
                )?;
                Ok::<_, String>((lobby_open, approval))
            });
            if !lobby_open { return Err("lobby is no longer open".into()); }
            claim.restrict(approved.transaction.inputs.iter().map(|i| i.previous_outpoint).collect())?;
            _start_claim = Some(claim);
            if ui.cancelled() { return Ok(None); }
            if !setup_read!(ui, n.sources_unspent(&approved.transaction, &approved.inputs())) {
                return Err("launch funding changed before signing could be submitted; create a fresh match".into());
            }
            eprintln!("Contributing your 5 KAS stake; external start fee {} KAS. Winnings/refunded stake arrive at your player address, spendable by this wallet.", node::fees::coins(approved.fee as u128));
            s.start = Some(approved);
            node::rpc::startup::phase("host_funding_preparation_complete", preparation);
        }
    }
    if ui.cancelled() { return Ok(None); }
    let persistence = std::time::Instant::now();
    node::save(&path, &cache)?;
    node::rpc::startup::phase("setup_required_persistence", persistence);
    let startup = if s.side == 1 { s.start } else { None };
    let terminal = !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let prepared = PreparedRun { lock, cache, connection: n.clone(), observed, code_received: s.code_received };
    // Join the setup reader and restore its screen before gameplay reads keys.
    drop(ui);
    Box::pin(run(a, c, s.invitation, path, false, false, false, None, terminal, Some(&s.wallet), startup, Some(prepared), shared)).await
    }.await;
    result
}
