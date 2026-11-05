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
    let (tx, _) = reusable::end(b, &m, &payer, true, &[funding.clone()], 100_000).unwrap();
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
    let a=reusable::artifact().unwrap();let b=TxBuilder::new(&a).unwrap();let mut f=fixture(&b);start_game(&mut f,&a,&b);
    let session=f.left.sessions[0].clone();let mut same=session.clone();same.cursor=hash(99);same.paid_fees+=1;
    assert!(!game_state_changed(&Some(session.clone()),&Some(same)));
    let QueueStage::Live(m)=session.stage.as_ref().unwrap()else{panic!()};let deadline=timeout_result(&m.snapshot()).unwrap().end_daa as u64;
    assert!(recovery_action(&session,deadline-1,&pubkey(&f.guest),&[]).is_none());
    assert!(matches!(recovery_action(&session,deadline,&pubkey(&f.guest),&[]),Some(GameAction::End{timeout:true})));
    assert!(f.left.recent_transactions.iter().any(|r|r.event.action=="start" && r.event.txid!=f.id));
    let (_,collect)=finish(&mut f,&a,&b);let txid=collect.transactions[0].tx.id();
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
    let mut current=Book::new("testnet-10".into(),pubkey(&owner),hash(0));current.import_previous(&path).unwrap();
    assert_eq!(current.wallet.records[0].phase,Phase::Legacy);assert!(current.available().is_empty());assert_eq!(current.wallet.created_count,1);
    current.wallet.validate("testnet-10",&pubkey(&owner)).unwrap();
    let tx=crate::reusable::close(&b,&set,&owner,&[],100_000).unwrap();let inputs=set.lanes.iter().map(|c|c.utxo.clone()).collect::<Vec<_>>();
    previous.signed.push(Signed{authorization:Authorization{transaction:tx,inputs,purpose:"old uncertain close".into(),successors:vec![],creates_set:false},effect:Effect::Wallet{closes:vec![set.id().unwrap()]},attempted_ms:1,fee:100_000});previous.refresh_pending();
    super::super::save(&path,&previous).unwrap();current.import_previous(&path).unwrap();assert_eq!(current.wallet.records[0].phase,Phase::Unavailable);
    assert!(crate::node::reservations::reserved().unwrap().contains(&set.lanes[0].outpoint));reservations::restore(&path,vec![]).unwrap();
}
