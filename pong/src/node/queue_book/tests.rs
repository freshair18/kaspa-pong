use super::*;
use crate::{
    node::queue_codes::Descriptor,
    reusable::{self, Set},
};
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn hash(n: u8) -> Hash {
    Hash::from_bytes([n; 32])
}
fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn coin(n: u8, owner: &Keypair) -> Funding {
    Funding {
        outpoint: TransactionOutpoint::new(hash(n), 0),
        utxo: UtxoEntry::new(10_000_000_000, p2pk(&pubkey(owner)), 1, false, None),
    }
}
fn created(b: &TxBuilder<'_>, owner: &Keypair, n: u8) -> (Transaction, Set, Funding) {
    let f = coin(n, owner);
    let (tx, states) = reusable::create_with_nonce(b, owner, 800_000_000, 300_000_000, &[f.clone()], 100_000, hash(n + 1)).unwrap();
    let set = reusable::accepted_set(&tx, [0, 1, 2], states, 100);
    (tx, set, f)
}
fn book(owner: &Keypair, set: Set) -> Book {
    let mut book = Book::new("testnet-10".into(), pubkey(owner), hash(0));
    book.wallet.records.push(Record { set, phase: Phase::Available });
    book
}
fn entries(tx: &Transaction, cells: Vec<(TransactionOutpoint, UtxoEntry)>, coin: &Funding) -> Vec<UtxoEntry> {
    tx.inputs
        .iter()
        .map(|i| {
            cells.iter().find(|(out, _)| *out == i.previous_outpoint).map(|(_, u)| u.clone()).unwrap_or_else(|| {
                assert_eq!(i.previous_outpoint, coin.outpoint);
                coin.utxo.clone()
            })
        })
        .collect()
}
fn block(n: u8, daa: u64, tx: Transaction, inputs: Vec<UtxoEntry>) -> ChainBlock {
    ChainBlock { hash: hash(n), daa, transactions: vec![AcceptedTx { tx, inputs }] }
}
fn sign(book: &mut Book, tx: Transaction, inputs: Vec<UtxoEntry>, effect: Effect, successors: Vec<Set>, creates_set: bool) {
    let fee = super::super::client::transaction_fee(&tx, &inputs).unwrap();
    book.authorize(
        Signed {
            authorization: Authorization { transaction: tx, inputs, purpose: "fixture".into(), successors, creates_set },
            effect,
            attempted_ms: 0,
            fee,
        },
        &Config::default(),
    )
    .unwrap();
}
struct Fixture {
    guest: Keypair,
    host: Keypair,
    left: Book,
    right: Book,
    reserve: ChainBlock,
    id: Hash,
}
fn fixture(b: &TxBuilder<'_>) -> Fixture {
    let guest = key(31);
    let host = key(32);
    let (_, g, _) = created(b, &guest, 61);
    let (_, h, _) = created(b, &host, 63);
    let mut left = book(&guest, g.clone());
    let mut right = book(&host, h.clone());
    let terms = super::super::invitations::GameTerms { speed: GameSpeed::Medium, ball_count: 1, win_score: 3, stake_kas: 5 };
    let code = HostCode::new("testnet-10", super::super::invitations::new_rendezvous_tag(terms).unwrap(), &h).unwrap();
    right.reserve_offer(code.clone(), None, false).unwrap();
    let r = Reservation { host:HostCode::parse(&code.text().unwrap()).unwrap(), guest: Descriptor::from_set(&g).unwrap(), capital: g.balance() };
    let funding = coin(65, &guest);
    let tx = reusable::reserve_with_payload(
        b,
        &g,
        &r.offer().unwrap(),
        &guest,
        g.balance(),
        &[funding.clone()],
        100_000,
        r.payload().unwrap(),
    )
    .unwrap();
    let id = tx.id();
    let inputs = entries(&tx, g.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())).collect(), &funding);
    left.add_session(Session {
        id,
        reservation: r,
        creation: tx.clone(),
        creation_inputs: inputs.clone(),
        host_set: None,
        stage: None,
        cancel_requested: false,
        created_ms: 1,
        authorized_fees: 0,
        paid_fees: 0,
        recovered: false,
        unavailable: false,
        cursor: hash(0),
        anchor: hash(0),
        fee_budget: 1_000_000_000,
    })
    .unwrap();
    sign(&mut left, tx.clone(), inputs.clone(), Effect::Game { session: id }, vec![], false);
    Fixture { guest, host, left, right, reserve: block(1, 110, tx, inputs), id }
}
fn accept_both(f: &mut Fixture, a: &Artifact, block: &ChainBlock) {
    f.left.apply(a, &[], &[block.clone()]).unwrap();
    f.right.apply(a, &[], &[block.clone()]).unwrap();
}

#[test]
fn reservation_replay_checks_declared_capital_against_queue_outputs() {
    let artifact = reusable::artifact().unwrap();
    let builder = TxBuilder::new(&artifact).unwrap();
    let fixture = fixture(&builder);
    let accepted = &fixture.reserve.transactions[0];
    let mut reservation = Reservation::parse(&accepted.tx, "testnet-10").unwrap();
    assert!(reservation.accepted(&builder, accepted, fixture.reserve.daa).is_ok());
    reservation.capital += 1;
    let error = reservation.accepted(&builder, accepted, fixture.reserve.daa).unwrap_err();
    assert!(error.to_string().contains("capital does not match"));
}

fn start_game(f: &mut Fixture, a: &Artifact, b: &TxBuilder<'_>) -> ChainBlock {
    accept_both(f, a, &f.reserve.clone());
    let stage = f.right.session(f.id).unwrap().stage.clone().unwrap();
    let QueueStage::Lobby { lobby, sets } = stage else { panic!() };
    let funding = coin(66, &f.host);
    let tx = reusable::start(b, &lobby, &sets[0], &sets[1], &f.host, sets[1].balance(), &[funding.clone()], 100_000).unwrap().0;
    let inputs = entries(&tx, QueueStage::Lobby { lobby, sets }.cells(), &funding);
    sign(&mut f.right, tx.clone(), inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    let block = block(2, 120, tx, inputs);
    accept_both(f, a, &block);
    block
}
fn finish(f: &mut Fixture, a: &Artifact, b: &TxBuilder<'_>) -> (ChainBlock, ChainBlock) {
    let stage = f.left.session(f.id).unwrap().stage.clone().unwrap();
    let QueueStage::Live(m) = stage else { panic!() };
    let side = next_contact(&m.snapshot()).unwrap().0;
    let payer = if side == 0 { f.guest } else { f.host };
    let funding = coin(69, &payer);
    let (tx, _) = reusable::timeout(b, &m, &payer, &[funding.clone()], 100_000).unwrap();
    let inputs = entries(&tx, QueueStage::Live(m).cells(), &funding);
    let end = block(4, 400, tx, inputs);
    accept_both(f, a, &end);
    let stage = f.left.session(f.id).unwrap().stage.clone().unwrap();
    let QueueStage::Collecting { result, sets } = stage else { panic!() };
    let funding = coin(70, &payer);
    let tx = reusable::collect(b, &result, &sets, &payer, &[funding.clone()], 100_000).unwrap().0;
    let inputs = entries(&tx, QueueStage::Collecting { result, sets }.cells(), &funding);
    let collect = block(5, 410, tx, inputs);
    accept_both(f, a, &collect);
    (end, collect)
}

#[test]
fn creation_restart_uncertain_submission_reorg_and_reacceptance() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let owner = key(33);
    let (tx, mut set, f) = created(&b, &owner, 71);
    for c in &mut set.lanes {
        c.utxo.block_daa_score = u64::MAX;
    }
    let mut journal = Book::new("testnet-10".into(), pubkey(&owner), hash(0));
    let id = tx.id();
    sign(&mut journal, tx.clone(), vec![f.utxo.clone()], Effect::Wallet { closes: vec![] }, vec![set], true);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queues.json");
    journal.save(&path).unwrap();
    let mut restored = Book::load(&path, "testnet-10", &pubkey(&owner)).unwrap();
    assert_eq!(restored.wallet.created_count, 1);
    assert!(restored.is_pending(id));
    assert!(restored.available().is_empty());
    let accepted = block(1, 100, tx.clone(), vec![f.utxo.clone()]);
    restored.apply(&a, &[], &[accepted]).unwrap();
    assert_eq!(restored.available().len(), 1);
    assert!(restored.was_confirmed(id));
    restored.apply(&a, &[hash(1)], &[]).unwrap();
    assert!(restored.is_pending(id));
    assert!(restored.available().is_empty());
    assert_eq!(restored.wallet.created_count, 1);
    restored.apply(&a, &[], &[block(2, 105, tx, vec![f.utxo])]).unwrap();
    assert_eq!(restored.available()[0].lanes[0].utxo.block_daa_score, 105);
    reservations::restore(&path, vec![]).unwrap();
}
#[test]
fn full_game_both_wallets_replay_undo_and_reuse_without_overwriting_resize() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let stage = f.left.session(f.id).unwrap().stage.clone().unwrap();
    let QueueStage::Live(m) = stage else { panic!() };
    let funding = coin(67, &f.guest);
    let tx = reusable::command(&b, &m.sets[0].lanes[0], 1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let inputs = entries(&tx, QueueStage::Live(m).cells(), &funding);
    sign(&mut f.left, tx.clone(), inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    let movement = block(3, 130, tx.clone(), inputs.clone());
    accept_both(&mut f, &a, &movement);
    f.left.session(f.id).unwrap();
    f.left.sessions[0].cancel_requested = true;
    f.left.sessions[0].fee_budget += 100_000_000;
    f.left.apply(&a, &[hash(3)], &[]).unwrap();
    f.right.apply(&a, &[hash(3)], &[]).unwrap();
    assert!(f.left.sessions[0].cancel_requested);
    assert_eq!(f.left.sessions[0].fee_budget, 1_100_000_000);
    assert!(f.left.is_pending(tx.id()));
    accept_both(&mut f, &a, &block(3, 135, tx, inputs));
    let (_, collect) = finish(&mut f, &a, &b);
    assert_eq!(f.left.available().len(), 1);
    assert_eq!(f.right.available().len(), 1);
    assert_eq!(f.left.available()[0].generation(), 1);
    f.left.apply(&a, &[hash(5)], &[]).unwrap();
    assert!(matches!(f.left.sessions[0].stage, Some(QueueStage::Collecting { .. })));
    assert!(f.left.available().is_empty());
    f.left.apply(&a, &[], &[collect]).unwrap();
    let set = f.left.available()[0].clone();
    let funding = coin(72, &f.guest);
    let tx = reusable::resize(&b, &set, &f.guest, set.balance() + 200_000_000, &[funding.clone()], 100_000).unwrap();
    let inputs = entries(&tx, set.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())).collect(), &funding);
    let next = reusable::accepted_set(&tx, [0, 1, 2], std::array::from_fn(|i| set.lanes[i].state.clone()), u64::MAX);
    sign(&mut f.left, tx.clone(), inputs.clone(), Effect::Wallet { closes: vec![] }, vec![next], false);
    let after = block(6, 420, tx, inputs);
    f.left.apply(&a, &[], &[after]).unwrap();
    assert_eq!(f.left.available()[0].balance(), set.balance() + 200_000_000);
    f.left.apply(&a, &[], &[ChainBlock { hash: hash(7), daa: 430, transactions: vec![] }]).unwrap();
    assert_eq!(f.left.available()[0].balance(), set.balance() + 200_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("active-wallet.json");
    f.left.save(&path).unwrap();
    let loaded = Book::load(&path, "testnet-10", &pubkey(&f.guest)).unwrap();
    assert_eq!(loaded.available()[0].balance(), set.balance() + 200_000_000);
    assert!(loaded.sessions[0].finished());
    reservations::restore(&path, vec![]).unwrap();
}
#[test]
fn cold_recovery_requires_positive_reset_proof_and_does_not_need_the_opponent() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let old = f.left.clone();
    finish(&mut f, &a, &b);
    let idle = f.left.available()[0].clone();
    let found = idle.lanes.iter().map(|c| Funding { outpoint: c.outpoint, utxo: c.utxo.clone() }).collect::<Vec<_>>();
    let mut missing = old.clone();
    missing.bootstrap(hash(10), 500, &[]).unwrap();
    assert!(missing.available().is_empty());
    assert!(missing.sessions[0].unavailable);
    assert!(!missing.sessions[0].recovered);
    let mut recovered = old;
    let scripts = recovered.recovery_scripts(&b).unwrap();
    assert!(found.iter().all(|f| scripts.contains(&f.utxo.script_public_key)));
    recovered.bootstrap(hash(10), 500, &found).unwrap();
    assert_eq!(recovered.available().len(), 1);
    assert!(recovered.sessions[0].recovered);
    assert_eq!(recovered.available()[0].generation(), 1);
    assert_eq!(recovered.available()[0].balance(), idle.balance());
    assert!(recovered.pending().is_empty());
    let host = f.right.available()[0].clone();
    let previous = f.right.sessions[0].reservation.host.clone();
    let code =
        HostCode::new("testnet-10", super::super::invitations::new_rendezvous_tag(previous.terms().unwrap()).unwrap(), &host).unwrap();
    f.right.reserve_offer(code.clone(), Some(pubkey(&f.guest)), true).unwrap();
    assert_eq!(f.right.rematch_offer(&f.right.sessions[0]), Some(code), "Resume must retain the published rematch code after restart");
}
#[test]
fn actual_conflict_releases_pending_authorization_and_undo_restores_it() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(m) = stage else { panic!() };
    let funding = coin(73, &f.guest);
    let cell = &m.sets[0].lanes[0];
    let first = reusable::command(&b, cell, 1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let second = reusable::command(&b, cell, -1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let sources = QueueStage::Live(m).cells();
    let inputs = entries(&first, sources.clone(), &funding);
    let id = first.id();
    sign(&mut f.left, first, inputs, Effect::Game { session: f.id }, vec![], false);
    let inputs = entries(&second, sources, &funding);
    f.left.apply(&a, &[], &[block(3, 130, second, inputs)]).unwrap();
    assert!(f.left.is_conflicted(id));
    assert!(!f.left.is_pending(id));
    f.left.apply(&a, &[hash(3)], &[]).unwrap();
    assert!(!f.left.is_conflicted(id));
    assert!(f.left.is_pending(id));
}
#[test]
fn recovered_session_receipts_restore_pending_claims_when_the_head_reorgs() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    let mut stale = f.left.clone();
    start_game(&mut f, &a, &b);
    stale.bootstrap(hash(10), 500, &[]).unwrap();
    assert!(stale.sessions[0].unavailable);
    stale.apply(&a, &[], &[ChainBlock { hash: hash(11), daa: 510, transactions: vec![] }]).unwrap();
    stale.restore_session(f.id, f.left.sessions[0].stage.clone().unwrap(), hash(11), vec![f.id], vec![]).unwrap();
    assert!(stale.pending().is_empty());
    assert!(stale.was_confirmed(f.id));
    assert_eq!(stale.sessions[0].paid_fees, 100_000);
    assert!(!stale.sessions[0].unavailable);
    stale.apply(&a, &[hash(11)], &[]).unwrap();
    assert!(stale.is_pending(f.id));
    assert!(stale.sessions[0].unavailable);
    assert_eq!(stale.sessions[0].paid_fees, 0);
}
#[test]
fn cancelling_a_restored_host_offer_releases_the_unused_set() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    let reserve = f.reserve.clone();
    accept_both(&mut f, &a, &reserve);
    assert!(f.right.available().is_empty());
    f.right.request_cancel(f.id).unwrap();
    assert_eq!(f.right.available().len(), 1);
    assert!(!f.right.offers[0].available);
}
#[test]
fn fast_iteration_unrelated_reorg_does_not_reset_game_and_timeout_is_automatic(){
    use crate::node::queue_service::{game_state_changed,recovery_action,GameAction};
    let a=reusable::artifact().unwrap();let b=TxBuilder::new(&a).unwrap();let mut f=fixture(&b);
    assert!(!f.right.can_bootstrap_idle(), "open host codes still need inbound reservation history");
    start_game(&mut f,&a,&b);
    assert!(!f.left.can_bootstrap_idle());
    let session=f.left.sessions[0].clone();let mut same=session.clone();same.cursor=hash(99);same.paid_fees+=1;
    assert!(!game_state_changed(&Some(session.clone()),&Some(same)));
    let QueueStage::Live(m)=session.stage.as_ref().unwrap()else{panic!()};let deadline=reusable::response_timeout(&m.snapshot()).unwrap().end_daa as u64;
    assert!(recovery_action(&session,deadline-1,&pubkey(&f.guest),&[]).is_none());
    assert!(matches!(recovery_action(&session,deadline,&pubkey(&f.guest),&[]),Some(GameAction::Timeout)));
    assert!(GameAction::Timeout.recovery_ready(false, true));
    assert!(GameAction::Collect.recovery_ready(false, true));
    assert!(!GameAction::Timeout.recovery_ready(false, false));
    assert!(!GameAction::Collect.recovery_ready(false, false));
    assert!(!GameAction::Cancel.recovery_ready(false, true));
    assert!(f.left.recent_transactions.iter().any(|r|r.event.action=="start" && r.event.txid!=f.id));
    let (_,collect)=finish(&mut f,&a,&b);let txid=collect.transactions[0].tx.id();
    assert!(f.left.can_bootstrap_idle());
    assert!(game_state_changed(&Some(session),&Some(f.left.sessions[0].clone())));
    assert!(f.left.recent_transactions.back().unwrap().terminal_lines().contains(&txid.to_string()));
    f.left.apply(&a,&[hash(5)],&[]).unwrap();
    assert!(f.left.recent_transactions.back().unwrap().terminal_lines().contains("REVERTED"));
}

#[test]
fn previous_idle_sets_stay_visible_and_closable_without_releasing_pending_inputs(){
    let owner=key(57);let a=crate::reusable::previous_artifact().unwrap();let b=TxBuilder::new(&a).unwrap();let (_,set,_)=created(&b,&owner,75);
    let mut previous=book(&owner,set.clone());previous.version=2;previous.wallet.created_count=1;
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("previous.json");super::super::save(&path,&previous).unwrap();
    let mut current=Book::new("testnet-10".into(),pubkey(&owner),hash(0));current.import_previous(&[&path]).unwrap();
    assert_eq!(current.wallet.records[0].phase,Phase::Legacy);assert!(current.available().is_empty());assert_eq!(current.wallet.created_count,1);
    current.wallet.validate("testnet-10",&pubkey(&owner)).unwrap();
    let tx=crate::reusable::close(&b,&set,&owner,&[],100_000).unwrap();let inputs=set.lanes.iter().map(|c|c.utxo.clone()).collect::<Vec<_>>();
    previous.signed.push(Signed{authorization:Authorization{transaction:tx,inputs,purpose:"old uncertain close".into(),successors:vec![],creates_set:false},effect:Effect::Wallet{closes:vec![set.id().unwrap()]},attempted_ms:1,fee:100_000});previous.refresh_pending();
    super::super::save(&path,&previous).unwrap();current.import_previous(&[&path]).unwrap();assert_eq!(current.wallet.records[0].phase,Phase::Unavailable);
    assert!(crate::node::reservations::reserved().unwrap().contains(&set.lanes[0].outpoint));reservations::restore(&path,vec![]).unwrap();
}

#[test]
fn auto_creation_uses_existing_sets_not_lifetime_creation_count() {
    let owner = key(53);
    let a = reusable::artifact().unwrap(); let b = TxBuilder::new(&a).unwrap();
    let (_, set, _) = created(&b, &owner, 81);
    let mut wallet = book(&owner, set);
    wallet.wallet.created_count = 12;
    for phase in [Phase::Available, Phase::Unavailable, Phase::Legacy, Phase::Active(hash(1)), Phase::Settling(hash(1))] {
        wallet.wallet.records[0].phase = phase;
        assert!(!wallet.may_auto_create());
    }
    wallet.wallet.records[0].phase = Phase::Closed;
    assert!(wallet.may_auto_create());
    wallet.previous_creation_pending = true;
    assert!(!wallet.may_auto_create());
    wallet.previous_creation_pending = false;
    let (tx, set, funding) = created(&b, &owner, 83);
    sign(&mut wallet, tx.clone(), vec![funding.utxo], Effect::Wallet { closes: vec![] }, vec![set], true);
    assert!(!wallet.may_auto_create());
    wallet.conflicted.insert(tx.id()); wallet.refresh_pending();
    assert!(wallet.may_auto_create());
}

#[test]
fn lane_fee_hint_reuses_exact_pricing_without_overpaying_when_stale() {
    use super::super::queue_service::price_lane_action;
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let Some(QueueStage::Live(m)) = &f.left.sessions[0].stage else { panic!("missing live game") };
    let (refill, lane) = reusable::command_with_reserve_target(
        &b, &m.sets[0].lanes[0], 1, &f.guest, &[coin(87, &f.guest)], 100_000, reusable::DEFAULT_QUEUE_RESERVE,
    ).unwrap();
    let cell = crate::engine::accepted_cell(&refill, 0, 121, lane);
    let config = Config::default();
    let rate = super::super::fee_policy::selected_rate(&config);
    let mut passes = 0;
    let build = |coins: &[Funding], fee| {
        passes += 1;
        reusable::command(&b, &cell, 1, &f.guest, coins, fee)
    };
    let (_, _, _, fee) = price_lane_action(&config, rate, None, build).unwrap();
    passes = 0;
    let build = |coins: &[Funding], fee| {
        passes += 1;
        reusable::command(&b, &cell, 1, &f.guest, coins, fee)
    };
    let (_, _, _, warm_fee) = price_lane_action(&config, rate, Some(fee), build).unwrap();
    assert_eq!((warm_fee, passes), (fee, 1));
    let (_, _, _, refreshed_fee) = price_lane_action(&config, rate, Some(fee + 100), |coins, fee| {
        reusable::command(&b, &cell, 1, &f.guest, coins, fee)
    }).unwrap();
    assert_eq!(refreshed_fee, fee);
}

#[test]
fn pending_fee_change_supports_independent_lanes_and_conflict_rollback() {
    let a = reusable::artifact().unwrap(); let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b); start_game(&mut f, &a, &b);
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(m) = stage.clone() else { panic!() };
    let funding = coin(88, &f.guest);
    let first = reusable::command(&b, &m.sets[0].lanes[0], 1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let alternate = reusable::command(&b, &m.sets[0].lanes[0], -1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let first_inputs = entries(&first, stage.cells(), &funding);
    sign(&mut f.left, first.clone(), first_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    assert!(f.left.pending_fee_coins(f.id).is_empty(), "unattempted transactions are not funding");
    f.left.signed.last_mut().unwrap().attempted_ms = 1;
    let change = f.left.pending_fee_coins(f.id);
    assert_eq!(change.len(), 1);
    assert!(f.left.pending_fee_coins(hash(99)).is_empty());
    assert_eq!(change[0].utxo.block_daa_score, u64::MAX);
    assert!(change[0].utxo.covenant_id.is_none());
    let second = reusable::command(&b, &m.sets[0].lanes[1], 1, &f.guest, &change, 100_000).unwrap().0;
    let second_inputs = entries(&second, stage.cells(), &change[0]);
    assert_eq!(f.left.fee_parents(&second, &second_inputs), vec![first.clone()]);
    let mut wrong_sources = second_inputs.clone();
    wrong_sources.last_mut().unwrap().covenant_id = Some(hash(99));
    assert!(f.left.fee_parents(&second, &wrong_sources).is_empty(), "ordinary funding proofs cannot authorize native successors");
    sign(&mut f.left, second.clone(), second_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    assert!(f.left.pending_fee_coins(f.id).is_empty(), "spent change must not be selected twice");
    f.left.signed.last_mut().unwrap().attempted_ms = 1;
    assert_eq!(f.left.pending_fee_coins(f.id).len(), 1);
    let inputs = entries(&alternate, stage.cells(), &funding);
    f.left.apply(&a, &[], &[block(3, 130, alternate, inputs)]).unwrap();
    assert!(f.left.is_conflicted(first.id())); assert!(f.left.is_conflicted(second.id()));
    assert!(f.left.pending_fee_coins(f.id).is_empty());
    f.left.apply(&a, &[hash(3)], &[]).unwrap();
    assert!(f.left.is_pending(first.id())); assert!(f.left.is_pending(second.id()));
    assert_eq!(f.left.pending_fee_coins(f.id).len(), 1);
    let paid = f.left.sessions[0].paid_fees;
    let mut accepted_child_inputs = second_inputs;
    accepted_child_inputs.last_mut().unwrap().block_daa_score = 130;
    f.left.apply(&a, &[], &[block(3, 130, first.clone(), first_inputs), block(4, 131, second.clone(), accepted_child_inputs)]).unwrap();
    assert!(f.left.was_confirmed(first.id())); assert!(f.left.was_confirmed(second.id()));
    assert_eq!(f.left.sessions[0].paid_fees, paid + 200_000);
    assert!(f.left.pending_fee_coins(f.id).is_empty());
    f.left.apply(&a, &[hash(4), hash(3)], &[]).unwrap();
    assert_eq!(f.left.sessions[0].paid_fees, paid);
    assert!(f.left.is_pending(first.id())); assert!(f.left.is_pending(second.id()));
}

#[test]
fn creating_alternate_queues_does_not_charge_the_running_game() {
    let a = reusable::artifact().unwrap(); let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b); start_game(&mut f, &a, &b);
    let paid = f.left.sessions[0].paid_fees;
    let (tx, set, funding) = created(&b, &f.guest, 91);
    sign(&mut f.left, tx.clone(), vec![funding.utxo.clone()], Effect::Wallet { closes: vec![] }, vec![set], true);
    f.left.apply(&a, &[], &[block(3, 130, tx, vec![funding.utxo])]).unwrap();
    assert_eq!(f.left.sessions[0].paid_fees, paid);
}

#[test]
fn submitted_resolution_allows_a_verified_movement_child_and_rolls_back_safely() {
    let a = reusable::artifact().unwrap(); let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b); start_game(&mut f, &a, &b);
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(current) = &stage else { panic!() };
    let (side, until) = next_contact(&current.snapshot()).unwrap();
    let owner = if side == 0 { f.guest } else { f.host };
    let wallet = if side == 0 { &mut f.left } else { &mut f.right };
    let funding = coin(93, &owner);
    let (resolve, _, _) = reusable::checkpoint(&b, current, side, until, &owner, &[funding.clone()], 100_000).unwrap();
    let resolve_inputs = entries(&resolve, stage.cells(), &funding);
    sign(wallet, resolve.clone(), resolve_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();
    assert!(wallet.resolve_successor(f.id).unwrap().is_none());
    wallet.signed.last_mut().unwrap().attempted_ms = 1;
    let successor = wallet.resolve_successor(f.id).unwrap().unwrap();
    assert_eq!(successor.sets[side].lanes[1].utxo.block_daa_score, u64::MAX);
    let change = wallet.pending_fee_coins(f.id);
    for lane in 0..3 {
        let movement = reusable::command(&b, &successor.sets[side].lanes[lane], 1, &owner, &change, 100_000).unwrap().0;
        let sources = entries(&movement, QueueStage::Live(successor.clone()).cells(), &change[0]);
        assert_eq!(wallet.source_parents(&movement, &sources).unwrap(), vec![resolve.clone()]);
        let mut accepted = sources[0].clone(); accepted.block_daa_score = until as u64;
        assert!(super::super::rpc::accepted_source_executes(&movement, &sources, 0, &accepted), "lane {lane}");
    }
    wallet.signed.last_mut().unwrap().authorization.inputs[0].amount += 1;
    assert!(wallet.resolve_successor(f.id).unwrap().is_none(), "a stale or false source must not produce a usable successor");
    wallet.signed.last_mut().unwrap().authorization.inputs[0].amount -= 1;
    let (child, _) = reusable::command(&b, &successor.sets[side].lanes[1], 1, &owner, &change, 100_000).unwrap();
    let child_inputs = entries(&child, QueueStage::Live(successor).cells(), &change[0]);
    assert_eq!(wallet.source_parents(&child, &child_inputs).unwrap(), vec![resolve.clone()]);
    assert_eq!(wallet.command_successors(&child, &child_inputs).unwrap().len(), 1);
    let mut forged = child_inputs.clone(); forged[0].amount += 1;
    assert!(wallet.command_successors(&child, &forged).unwrap().is_empty());
    sign(wallet, child.clone(), child_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    wallet.direct_send.insert(child.id());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queues.json");
    wallet.save(&path).unwrap();
    let restarted = Book::load(&path, "testnet-10", &pubkey(&owner)).unwrap();
    assert!(restarted.is_pending(child.id()));
    assert!(!restarted.direct_send.contains(&child.id()));
    let paid = wallet.sessions[0].paid_fees;
    // Exact parent provenance is lost after conflict, and restored on rollback.
    let alternate = reusable::checkpoint(&b, current, side, until, &owner, &[funding.clone()], 200_000).unwrap().0;
    let alternate_inputs = entries(&alternate, stage.cells(), &funding);
    wallet.apply(&a, &[], &[block(3, until as u64, alternate, alternate_inputs)]).unwrap();
    assert!(wallet.is_conflicted(child.id()));
    assert!(!wallet.direct_send.contains(&child.id()));
    assert!(wallet.source_parents(&child, &child_inputs).unwrap().is_empty());
    wallet.apply(&a, &[hash(3)], &[]).unwrap();
    assert_eq!(wallet.source_parents(&child, &child_inputs).unwrap(), vec![resolve.clone()]);
    // The command must execute with both pending and actual parent birth scores.
    let mut actual = child_inputs.clone();
    for u in &mut actual { if u.block_daa_score == u64::MAX { u.block_daa_score = until as u64; } }
    assert!(super::super::rpc::accepted_source_executes(&child, &child_inputs, 0, &actual[0]));
    wallet.apply(&a, &[], &[block(3, until as u64, resolve.clone(), resolve_inputs), block(4, until as u64 + 1, child.clone(), actual)]).unwrap();
    assert!(wallet.was_confirmed(resolve.id())); assert!(wallet.was_confirmed(child.id()));
    assert_eq!(wallet.sessions[0].paid_fees, paid + 200_000);
    wallet.apply(&a, &[hash(4), hash(3)], &[]).unwrap();
    assert!(wallet.is_pending(resolve.id())); assert!(wallet.is_pending(child.id()));
    assert_eq!(wallet.sessions[0].paid_fees, paid);
}

#[test]
fn missing_history_and_repeated_timeouts_trigger_queue_reconciliation() {
    use super::super::queue_service::needs_queue_reconciliation;
    assert!(needs_queue_reconciliation("RPC Server: cannot find header abc", 1));
    assert!(needs_queue_reconciliation("queue reorg exceeds retained history", 1));
    assert!(!needs_queue_reconciliation("node RPC request timed out", 2));
    assert!(needs_queue_reconciliation("node RPC request timed out", 3));
    assert!(!needs_queue_reconciliation("wallet journal lock poisoned", 3));
}
