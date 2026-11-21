use super::*;
use crate::{
    node::queue_codes::Descriptor,
    reusable::{self, Set},
};
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn hash(n: u8) -> Hash {
    Hash::from_bytes([n; 32])
}

#[tokio::test]
async fn recovery_history_reads_later_batches_and_stops_at_the_same_head() {
    use super::super::rpc::{Delta, recovery_delta_from};
    let mut reads = vec![];
    let delta = recovery_delta_from(hash(0), hash(3), |cursor| {
        reads.push(cursor);
        let numbers = if cursor == hash(0) { vec![1] } else { vec![2, 3, 4] };
        async move { Ok(Delta { removed: vec![], common: None, added: numbers.into_iter()
            .map(|n| ChainBlock { hash: hash(n), daa: n as u64, transactions: vec![] }).collect() }) }
    }).await.unwrap();
    assert_eq!(reads, vec![hash(0), hash(1)]);
    assert_eq!(delta.added.iter().map(|b| b.hash).collect::<Vec<_>>(), vec![hash(1), hash(2), hash(3)]);
    assert!(recovery_delta_from(hash(3), hash(3), |_| async { panic!("aligned cursors need no RPC") }).await.unwrap().added.is_empty());
}

#[tokio::test]
async fn recovery_history_rejects_reorgs_and_incomplete_routes() {
    use super::super::rpc::{Delta, recovery_delta_from};
    for mode in 0..3 {
        let result = recovery_delta_from(hash(0), hash(3), |cursor| async move {
            let first = cursor == hash(0);
            Ok(Delta { removed: if !first && mode == 0 { vec![hash(1)] } else { vec![] }, common: None,
                added: if !first && mode == 1 { vec![] } else { vec![ChainBlock { hash: hash(1), daa: 1, transactions: vec![] }] } })
        }).await;
        assert!(result.is_err(), "a reorg, exhausted route, or repeated cursor must not install partial recovery");
    }
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
    let (tx, states) = reusable::collect(b, &result, &sets, &payer, &[funding.clone()], 100_000).unwrap();
    let settled = std::array::from_fn::<_,2,_>(|side| reusable::accepted_set(&tx, std::array::from_fn(|i| side*3+i), states[side].clone(), 410));
    let inputs = entries(&tx, QueueStage::Collecting { result, sets }.cells(), &funding);
    let mut collect = block(5, 410, tx, inputs);
    for (side, set) in settled.iter().enumerate() {
        let payer = if side == 0 { f.guest } else { f.host };
        let funding = coin(112 + side as u8, &payer);
        let tx = reusable::restore_bank(b,set,&payer,std::slice::from_ref(&funding),100_000).unwrap();
        let inputs = entries(&tx,vec![(set.lanes[0].outpoint,set.lanes[0].utxo.clone())],&funding);
        collect.transactions.push(AcceptedTx { tx, inputs });
    }
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
    let (_, mut collect) = finish(&mut f, &a, &b);
    assert_eq!(f.left.available().len(), 1);
    assert_eq!(f.right.available().len(), 1);
    assert_eq!(f.left.available()[0].generation(), 1);
    f.left.apply(&a, &[hash(5)], &[]).unwrap();
    assert!(matches!(f.left.sessions[0].stage, Some(QueueStage::Collecting { .. })));
    assert!(f.left.available().is_empty());
    let opponent_restoration = collect.transactions.pop().unwrap();
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
    let late = ChainBlock { hash: hash(7), daa: 430, transactions: vec![opponent_restoration] };
    f.left.apply(&a, &[], std::slice::from_ref(&late)).unwrap();
    assert!(matches!(&f.left.sessions[0].stage, Some(QueueStage::Finished { sets, .. }) if sets.iter().all(Set::idle)));
    assert_eq!(f.left.available()[0].balance(), set.balance() + 200_000_000);
    f.left.apply(&a, &[late.hash], &[]).unwrap();
    assert!(f.left.sessions[0].stage.as_ref().unwrap().restoring_set(&pubkey(&f.host)).is_some());
    assert_eq!(f.left.available()[0].balance(), set.balance() + 200_000_000);
    f.left.apply(&a, &[], &[late]).unwrap();
    assert!(!f.left.sessions[0].stage.as_ref().unwrap().has_pending_restoration());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("active-wallet.json");
    f.left.save(&path).unwrap();
    let loaded = Book::load(&path, "testnet-10", &pubkey(&f.guest)).unwrap();
    assert_eq!(loaded.available()[0].balance(), set.balance() + 200_000_000);
    assert!(loaded.sessions[0].finished());
    reservations::restore(&path, vec![]).unwrap();
}
#[test]
fn restore_can_be_signed_before_collection_and_is_discarded_if_parent_loses() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let (_, finished) = finish(&mut f, &a, &b);
    let winner = match &f.left.sessions[0].stage {
        Some(QueueStage::Finished { result, .. }) => result.winner,
        _ => panic!("expected a finished game"),
    };
    let (book, owner, sponsor) = if winner == 0 {
        (&mut f.left, f.guest, f.host)
    } else {
        (&mut f.right, f.host, f.guest)
    };
    book.apply(&a, &[finished.hash], &[]).unwrap();
    let stage = book.sessions[0].stage.clone().unwrap();
    let QueueStage::Collecting { result, sets } = &stage else { panic!("expected collection stage") };
    let fee = 100_000;
    let tx = reusable::collect(&b, result, sets, &owner, &[], fee).unwrap().0;
    let funding = coin(105, &sponsor);
    let inputs = entries(&tx, stage.cells(), &funding);
    let parent = Signed {
        authorization: Authorization { transaction: tx.clone(), inputs: inputs.clone(), purpose: "collect queues".into(),
            successors: vec![], creates_set: false },
        effect: Effect::Game { session: f.id },
        attempted_ms: 0,
        fee,
    };
    let config = Config { fee_sompi: 50_000_000, ..Config::default() };
    let child = book.prepare_collection_restoration(&parent, &owner, &config).unwrap().unwrap();
    let child_id = child.authorization.transaction.id();
    let child_tx = child.authorization.transaction.clone();
    let mut child_inputs = child.authorization.inputs.clone();
    assert_eq!(child_tx.inputs[0].previous_outpoint.transaction_id, tx.id());
    assert_eq!(child_inputs[0].block_daa_score, u64::MAX);
    book.authorize(parent, &config).unwrap();
    book.authorize(child, &config).unwrap();
    assert!(book.available().is_empty());
    assert!(book.source_parents(&child_tx, &child_inputs).unwrap().is_empty());
    book.signed.iter_mut().find(|s| s.authorization.transaction.id() == tx.id()).unwrap().attempted_ms = 1;
    assert_eq!(book.source_parents(&child_tx, &child_inputs).unwrap()[0].id(), tx.id());
    let mut competing = book.clone();
    let other = reusable::collect(&b, result, sets, &sponsor, &[funding.clone()], fee).unwrap().0;
    let other_inputs = entries(&other, stage.cells(), &funding);
    competing.apply(&a, &[], &[block(5, 410, other, other_inputs)]).unwrap();
    assert!(competing.is_conflicted(tx.id()));
    assert!(competing.is_conflicted(child_id));
    assert!(competing.sessions[0].stage.as_ref().unwrap().restoring_set(&pubkey(&owner)).is_some());

    let mut accepted = block(5, 410, tx, inputs);
    child_inputs[0].block_daa_score = accepted.daa;
    accepted.transactions.push(AcceptedTx { tx: child_tx, inputs: child_inputs });
    book.apply(&a, &[], &[accepted]).unwrap();
    assert!(book.was_confirmed(child_id));
    assert_eq!(book.available().len(), 1);
    book.apply(&a, &[hash(5)], &[]).unwrap();
    assert!(book.available().is_empty());
    assert!(book.is_pending(child_id));
}

#[test]
fn cold_recovery_requires_positive_reset_proof_and_does_not_need_the_opponent() {
    let a = reusable::artifact().unwrap();
    let b = TxBuilder::new(&a).unwrap();
    let mut f = fixture(&b);
    start_game(&mut f, &a, &b);
    let old = f.left.clone();
    let (end, mut settlement) = finish(&mut f, &a, &b);
    settlement.transactions.truncate(1);
    let mut midway = old.clone();
    midway.apply(&a, &[], &[end, settlement]).unwrap();
    let waiting = midway.sessions[0].stage.as_ref().unwrap().restoring_set(&pubkey(&f.guest)).unwrap();
    let waiting = waiting.lanes.iter().map(|c| Funding { outpoint: c.outpoint, utxo: c.utxo.clone() }).collect::<Vec<_>>();
    let mut pruned = old.clone();
    let scripts = pruned.recovery_scripts(&b).unwrap();
    assert!(waiting.iter().all(|f| scripts.contains(&f.utxo.script_public_key)));
    pruned.bootstrap(hash(10), 500, &waiting).unwrap();
    assert!(pruned.available().is_empty());
    assert!(matches!(super::super::queue_service::recovery_action(&pruned.sessions[0],500,&pubkey(&f.guest),&[]),
        Some(super::super::queue_service::GameAction::RestoreBank)));
    let idle = f.left.available()[0].clone();
    let found = idle.lanes.iter().map(|c| Funding { outpoint: c.outpoint, utxo: c.utxo.clone() }).collect::<Vec<_>>();
    pruned.bootstrap(hash(11), 501, &found).unwrap();
    assert_eq!(pruned.available().len(), 1);
    assert!(pruned.sessions[0].recovered);
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
fn rechecked_start_can_reactivate_the_same_id_without_counting_its_fee_twice() {
    let (artifact, builder) = reusable::current_runtime().unwrap();
    let mut fixture = fixture(builder);
    let start = start_game(&mut fixture, artifact, builder);
    fixture.right.apply(artifact, &[start.hash], &[]).unwrap();
    let wallet = &mut fixture.right;
    let mut signed = wallet.signed.iter().find(|signed| signed.authorization.transaction.id() == start.transactions[0].tx.id())
        .unwrap().clone();
    signed.authorization.purpose = "start".into();
    let id = signed.authorization.transaction.id();
    wallet.signed.iter_mut().find(|signed| signed.authorization.transaction.id() == id)
        .unwrap().authorization.purpose = "start".into();
    wallet.conflicted.insert(id);
    wallet.refresh_pending();
    let fees = wallet.session(fixture.id).unwrap().authorized_fees;
    let count = wallet.signed.len();
    let queue_parent = signed.authorization.transaction.inputs[1].previous_outpoint.transaction_id;
    wallet.conflicted.insert(queue_parent);
    wallet.authorize(signed.clone(), &Config::default()).unwrap();
    assert!(!wallet.conflict_descendants(), "a verified accepted queue output overrides the old parent conflict");
    assert!(wallet.is_pending(id));
    assert_eq!(wallet.signed.len(), count);
    assert_eq!(wallet.session(fixture.id).unwrap().authorized_fees, fees);
    let mut missing = wallet.clone();
    missing.sessions[0].unavailable = true;
    for record in &mut missing.wallet.records { record.phase = Phase::Unavailable; }
    assert!(missing.conflict_descendants(), "an unverified descendant must still inherit its parent's conflict");
    assert!(missing.is_conflicted(id));
    wallet.authorize(signed.clone(), &Config::default()).unwrap();
    assert_eq!(wallet.session(fixture.id).unwrap().authorized_fees, fees);
    wallet.conflicted.insert(id);
    wallet.refresh_pending();
    wallet.sessions.iter_mut().find(|session| session.id == fixture.id).unwrap().cancel_requested = true;
    assert!(wallet.authorize(signed, &Config::default()).is_err());
    assert!(wallet.is_conflicted(id));
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
    use crate::node::queue_service::{game_state_changed,recovery_action,GameAction,COLLECTION_WINNER_HEAD_START_DAA};
    let a=reusable::artifact().unwrap();let b=TxBuilder::new(&a).unwrap();let mut f=fixture(&b);
    assert!(!f.right.can_bootstrap_idle(), "open host codes still need inbound reservation history");
    start_game(&mut f,&a,&b);
    assert!(!f.left.can_bootstrap_idle());
    let session=f.left.sessions[0].clone();let mut same=session.clone();same.cursor=hash(99);same.paid_fees+=1;
    assert!(!game_state_changed(&Some(session.clone()),&Some(same)));
    let mut retimed=session.clone();
    let Some(QueueStage::Live(game))=retimed.stage.as_mut()else{panic!("game should be live")};
    game.court.state.world.clock+=1;
    game.court.state.world.launch_epoch+=1;
    game.court.state.world.serve_until+=1;
    game.court.state.world.response_at=0;
    game.court.utxo.block_daa_score+=1;
    assert!(game_state_changed(&Some(session.clone()),&Some(retimed)),"retiming must invalidate coordinator work even when presentation stays continuous");
    let mut moved=session.clone();
    let Some(QueueStage::Live(game))=moved.stage.as_mut()else{panic!("game should be live")};
    game.court.state.world.left_y+=1;
    assert!(game_state_changed(&Some(session.clone()),&Some(moved)),"a corrected paddle position must be surfaced to presentation");
    let QueueStage::Live(m)=session.stage.as_ref().unwrap()else{panic!()};let deadline=reusable::response_timeout(&m.snapshot()).unwrap().end_daa as u64;
    assert!(recovery_action(&session,deadline-1,&pubkey(&f.guest),&[]).is_none());
    let winner = reusable::response_timeout(&m.snapshot()).unwrap().winner;
    for owner in [&f.guest, &f.host] {
        assert_eq!(matches!(recovery_action(&session,deadline,&pubkey(owner),&[]),Some(GameAction::Timeout)),
            session.side(&pubkey(owner)) as i64 == winner, "only the opponent may automatically claim timeout");
    }
    assert!(GameAction::Timeout.recovery_ready(false, true));
    assert!(GameAction::Collect.recovery_ready(false, true));
    assert!(GameAction::RestoreBank.recovery_ready(false, true));
    assert!(!GameAction::RestoreBank.recovery_ready(false, false));
    assert!(!GameAction::Timeout.recovery_ready(false, false));
    assert!(!GameAction::Collect.recovery_ready(false, false));
    assert!(!GameAction::Cancel.recovery_ready(false, true));
    assert!(f.left.recent_transactions.iter().any(|r|r.event.action=="start" && r.event.txid!=f.id));
    let (_,collect)=finish(&mut f,&a,&b);let txid=collect.transactions[0].tx.id();
    assert!(f.left.can_bootstrap_idle());
    assert!(game_state_changed(&Some(session),&Some(f.left.sessions[0].clone())));
    assert!(f.left.recent_transactions.iter().any(|r| r.terminal_lines().contains(&txid.to_string())));
    assert_eq!(f.left.recent_transactions.back().unwrap().event.action, "restore_bank");
    f.left.apply(&a,&[hash(5)],&[]).unwrap();
    assert!(f.left.recent_transactions.back().unwrap().terminal_lines().contains("REVERTED"));
    let session = &f.left.sessions[0];
    let Some(QueueStage::Collecting { result, .. }) = &session.stage else { panic!("collection rollback should recover the result") };
    let (winner, loser) = if result.state.collection.finish.winner == 0 { (f.guest, f.host) } else { (f.host, f.guest) };
    let accepted = result.utxo.block_daa_score;
    let fallback = accepted + COLLECTION_WINNER_HEAD_START_DAA;
    assert!(matches!(recovery_action(session, accepted, &pubkey(&winner), &[]), Some(GameAction::Collect)));
    assert!(recovery_action(session, fallback - 1, &pubkey(&loser), &[]).is_none());
    assert!(matches!(recovery_action(session, fallback, &pubkey(&loser), &[]), Some(GameAction::Collect)));
}

#[test]
fn timeout_replacement_fee_scales_to_the_rebuilt_transaction() {
    use super::super::queue_service::timeout_replacement_fee;
    assert_eq!(timeout_replacement_fee(Some((1001, 100)), 200).unwrap(), 2102);
    assert_eq!(timeout_replacement_fee(Some((1001, 100)), 50).unwrap(), 601);
    assert!(timeout_replacement_fee(Some((1001, 0)), 50).is_err());
    assert!(timeout_replacement_fee(Some((u64::MAX, 1)), 2).is_err());
}

#[test]
fn coordinator_detects_exact_sources_even_when_the_visible_world_is_unchanged() {
    use super::super::queue_service::game_state_changed;
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let before = f.left.sessions[0].clone();
    for source in 0..4 {
        let mut after = before.clone();
        let Some(QueueStage::Live(game)) = after.stage.as_mut() else { panic!() };
        if source == 0 { game.court.utxo.block_daa_score += 1; }
        else { game.sets[0].lanes[source - 1].utxo.block_daa_score += 1; }
        assert!(game_state_changed(&Some(before.clone()), &Some(after)), "source {source} acceptance metadata must invalidate planning");
    }
    let mut after = before.clone();
    let Some(QueueStage::Live(game)) = after.stage.as_mut() else { panic!() };
    game.sets[0].lanes[1].outpoint.transaction_id = hash(99);
    assert!(game_state_changed(&Some(before), &Some(after)), "identical state on a new outpoint still needs fresh planning");
}

#[test]
fn timeout_repricing_retires_only_the_previous_opponent_timeout() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!() };
    let winner = reusable::response_timeout(&game.snapshot()).unwrap().winner;
    let owner = if winner == 0 { f.guest } else { f.host };
    let loser = if winner == 0 { f.host } else { f.guest };
    let config = Config { fee_sompi: 1_000_000, ..Default::default() };
    let make = |key: &Keypair, n, fee| {
        let funding = coin(n, key);
        let tx = reusable::timeout(b, game, key, &[funding.clone()], fee).unwrap().0;
        let inputs = entries(&tx, stage.cells(), &funding);
        Signed { authorization: Authorization { transaction: tx, inputs, purpose: "timeout".into(),
            successors: vec![], creates_set: false }, effect: Effect::Game { session: f.id }, attempted_ms: 1, fee }
    };
    let own_timeout = make(&loser, 95, 100_000);
    let loser_book = if winner == 0 { &mut f.right } else { &mut f.left };
    assert!(loser_book.authorize(own_timeout, &config).unwrap_err().to_string().contains("own timeout"));
    let old = make(&owner, 96, 100_000);
    let old_id = old.authorization.transaction.id();
    let new = make(&owner, 97, 200_000);
    let new_id = new.authorization.transaction.id();
    let wallet = if winner == 0 { &mut f.left } else { &mut f.right };
    wallet.authorize(old, &config).unwrap();
    assert!(wallet.replace_timeout(old_id, new.clone(), &Config { fee_sompi: 150_000, ..config.clone() }).is_err());
    assert!(wallet.is_pending(old_id));
    wallet.replace_timeout(old_id, new, &config).unwrap();
    assert!(!wallet.is_pending(old_id));
    assert!(wallet.is_pending(new_id));
}

#[test]
fn loading_v40_prunes_archived_queues_and_keeps_current_recovery() {
    let (a, builder) = reusable::current_runtime().unwrap();
    let owner = key(57);
    let (_, current, _) = created(builder, &owner, 73);
    let (_, archived, _) = created(builder, &owner, 75);
    let (_, mut incompatible, _) = created(builder, &owner, 77);
    incompatible.lanes[0].utxo.script_public_key = p2pk(&pubkey(&owner));
    let mut saved = book(&owner, current.clone());
    saved.wallet.records.extend([
        Record { set: archived.clone(), phase: Phase::Archived },
        Record { set: incompatible, phase: Phase::Unavailable },
    ]);

    let closure = reusable::close(builder, &archived, &owner, &[], 100_000).unwrap();
    let closure_id = closure.id();
    let inputs = archived.lanes.iter().map(|lane| lane.utxo.clone()).collect();
    sign(&mut saved, closure, inputs, Effect::Wallet { closes: vec![archived.id().unwrap()] }, vec![], false);

    let (tx, set, funding) = created(builder, &owner, 79);
    let current_pending = tx.id();
    sign(&mut saved, tx, vec![funding.utxo], Effect::Wallet { closes: vec![] }, vec![set], true);
    saved.history.push_back(Undo {
        hash: hash(82),
        parent: hash(0),
        daa: 0,
        before: Some(saved.state()),
        status: vec![(closure_id, false, false)],
    });
    saved.cursor = hash(82);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queues-v40.json");
    saved.save(&path).unwrap();
    let mut loaded = Book::load(&path, "testnet-10", &pubkey(&owner)).unwrap();
    let durable: Book = super::super::load(&path).unwrap();
    assert_eq!(durable.wallet.records.len(), 1);
    assert!(!loaded.has_authorization(closure_id));
    assert!(loaded.has_authorization(current_pending));

    loaded.apply(a, &[hash(82)], &[]).unwrap();
    assert_eq!(loaded.available().len(), 1);
    assert_eq!(loaded.wallet.records[0].set.id().unwrap(), current.id().unwrap());
    assert!(loaded.is_pending(current_pending));
    reservations::restore(&path, vec![]).unwrap();
}

#[test]
fn auto_creation_uses_existing_sets_not_lifetime_creation_count() {
    let owner = key(53);
    let a = reusable::artifact().unwrap(); let b = TxBuilder::new(&a).unwrap();
    let (_, set, _) = created(&b, &owner, 81);
    let mut wallet = book(&owner, set);
    wallet.wallet.created_count = 12;
    for phase in [Phase::Available, Phase::Unavailable, Phase::Active(hash(1)), Phase::Settling(hash(1))] {
        wallet.wallet.records[0].phase = phase;
        assert!(!wallet.may_auto_create());
    }
    wallet.wallet.records[0].phase = Phase::Closed;
    assert!(wallet.may_auto_create());
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
fn old_offers_do_not_force_history_replay_when_idle() {
    let (_, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    assert!(!f.right.can_bootstrap_idle(), "a current invitation can still be accepted");
    for lane in &mut f.right.wallet.records[0].set.lanes {
        lane.state.generation += 1;
    }
    assert!(f.right.can_bootstrap_idle());
    f.right.wallet.records[0].phase = Phase::Unavailable;
    assert!(!f.right.can_bootstrap_idle(), "unverified generations cannot displace an old invitation");
}

#[test]
fn setup_reanchor_preserves_settlement_and_discovers_a_fresh_reservation() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    assert!(!f.right.can_reanchor_for_setup(), "an advertised host code must retain inbound history");
    start_game(&mut f, a, b);
    assert!(!f.right.can_reanchor_for_setup(), "a live game must not be skipped");
    let (_, collect) = finish(&mut f, a, b);
    f.right.apply(a, &[collect.hash], &[]).unwrap();
    let stage = f.right.sessions[0].stage.clone().unwrap();
    let QueueStage::Collecting { result, sets } = &stage else { panic!() };
    let funding = coin(83, &f.host);
    let tx = reusable::collect(b, result, sets, &f.host, &[funding.clone()], 100_000).unwrap().0;
    let inputs = entries(&tx, stage.cells(), &funding);
    sign(&mut f.right, tx.clone(), inputs, Effect::Game { session: f.id }, vec![], false);
    f.right.signed.last_mut().unwrap().authorization.purpose = "collect queues".into();
    for offer in &mut f.right.offers { offer.available = false; }
    let (_, fresh_host, _) = created(b, &f.host, 76);
    f.right.wallet.records.push(Record { set: fresh_host.clone(), phase: Phase::Available });
    assert!(!f.right.can_bootstrap_idle());
    assert!(f.right.can_reanchor_for_setup(), "old settlement must not force global history replay");
    let found = f.right.all_cells().into_iter().map(|(outpoint, utxo)| Funding { outpoint, utxo }).collect::<Vec<_>>();
    f.right.bootstrap(hash(90), 100_000, &found).unwrap();
    assert_eq!(f.right.cursor, hash(90));
    assert!(f.right.is_pending(tx.id()), "an uncertain signed collection must survive reanchoring");
    assert!(matches!(f.right.session(f.id).unwrap().stage, Some(QueueStage::Collecting { .. })));
    let terms = super::super::invitations::GameTerms { speed: GameSpeed::Medium, ball_count: 1, win_score: 3, stake_kas: 5 };
    let code = HostCode::new("testnet-10", super::super::invitations::new_rendezvous_tag(terms).unwrap(), &fresh_host).unwrap();
    f.right.reserve_offer(code.clone(), None, false).unwrap();
    assert!(!f.right.can_reanchor_for_setup(), "do not skip a reservation after publishing this code");
    let (_, guest, _) = created(b, &f.guest, 79);
    let reservation = Reservation { host: code, guest: Descriptor::from_set(&guest).unwrap(), capital: guest.balance() };
    let funding = coin(82, &f.guest);
    let tx = reusable::reserve_with_payload(b, &guest, &reservation.offer().unwrap(), &f.guest,
        guest.balance(), &[funding.clone()], 100_000, reservation.payload().unwrap()).unwrap();
    let inputs = entries(&tx, guest.lanes.iter().map(|c| (c.outpoint, c.utxo.clone())).collect(), &funding);
    f.right.apply(a, &[], &[block(91, 100_010, tx.clone(), inputs)]).unwrap();
    assert!(matches!(f.right.session(tx.id()).unwrap().stage, Some(QueueStage::Lobby { .. })),
        "new reservation should be discovered without replaying the old game's history");
}

#[test]
fn old_repair_yields_to_live_play_and_skips_superseded_sets() {
    let (_, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    let settled = f.left.wallet.records[0].set.clone();
    f.left.sessions[0].stage = Some(QueueStage::Cancelled(settled));
    f.left.sessions[0].unavailable = true;
    assert_eq!(f.left.repair_candidates(None).len(), 1);

    let mut active = f.left.sessions[0].clone();
    active.id = hash(218);
    active.stage = None;
    active.unavailable = false;
    f.left.sessions.push(active);
    assert!(f.left.repair_candidates(Some(hash(218))).is_empty());
    assert_eq!(f.left.repair_candidates(Some(f.id)).len(), 1);
    f.left.sessions[1].cancel_requested = true;
    assert_eq!(f.left.repair_candidates(Some(hash(218))).len(), 1);

    let generation = f.left.wallet.records[0].set.generation();
    for lane in &mut f.left.wallet.records[0].set.lanes {
        lane.state.generation = generation + 2;
    }
    assert!(f.left.repair_candidates(None).is_empty());
    assert!(f.left.state().stages.iter().all(|(id, ..)| *id != f.id));
    assert!(f.left.session(f.id).is_ok(), "skipping repair keeps the saved game");
    f.left.wallet.records[0].phase = Phase::Unavailable;
    assert_eq!(f.left.repair_candidates(None).len(), 1, "an unverified set does not prove recovery");
}

#[test]
fn idle_compaction_preserves_pending_sessions_and_rebases_retained_undo() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    let book = &mut f.left;
    book.daa = 1_000;
    let mut settled = book.wallet.records[0].set.clone();
    book.sessions[0].stage = Some(QueueStage::Cancelled(settled.clone()));
    assert!(!book.compact_finished(200, None), "unresolved transaction prevents compaction");

    book.signed.clear();
    book.refresh_pending();
    assert!(!book.compact_finished(1_001, None), "unobserved pruning point");
    assert!(!book.compact_finished(200, Some(f.id)), "keep the selected result");
    settled.lanes[0].state.restoring = true;
    book.sessions[0].stage = Some(QueueStage::Cancelled(settled.clone()));
    assert!(!book.compact_finished(200, None), "keep recovery for an incomplete restoration");
    settled.lanes[0].state.restoring = false;
    book.sessions[0].stage = Some(QueueStage::Cancelled(settled));

    let before = book.state();
    let mut later = book.sessions[0].clone();
    later.id = hash(214);
    later.unavailable = true;
    book.sessions.push(later);
    let head = hash(215);
    book.history.push_back(Undo { hash: hash(210), parent: book.cursor, daa: 100, before: None, status: vec![] });
    book.history.push_back(Undo { hash: hash(211), parent: hash(210), daa: 150, before: None, status: vec![] });
    book.history.push_back(Undo { hash: head, parent: hash(211), daa: 400, before: Some(before), status: vec![] });
    book.cursor = head;

    assert!(book.compact_finished(200, None));
    assert_eq!(book.history.front().unwrap().hash, hash(211), "keep the pruning boundary row");
    assert_eq!(book.sessions.len(), 1);
    assert_eq!(book.sessions[0].id, hash(214));
    assert_eq!(book.wallet.records.len(), 1);
    assert!(!book.compact_finished(200, None));
    book.apply(a, &[head], &[]).unwrap();
    assert!(book.sessions[0].stage.is_none(), "undo still recognizes games created after the snapshot");
}

#[test]
fn gameplay_outbox_restores_exact_spend_without_double_counting_and_keeps_undo() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queues.json");
    f.left.save(&path).unwrap();

    let QueueStage::Live(game) = f.left.sessions[0].stage.as_ref().unwrap() else { panic!("expected live game") };
    let lane = &game.sets[0].lanes[1];
    let funding = coin(96, &f.guest);
    let (tx, _) = reusable::command(b, lane, 1, &f.guest, &[funding.clone()], 100_000).unwrap();
    let inputs = vec![lane.utxo.clone(), funding.utxo];
    sign(&mut f.left, tx.clone(), inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    let signed = f.left.signed.last_mut().unwrap();
    signed.authorization.purpose = "move".into();
    Book::save_gameplay_authorization(&path, signed).unwrap();

    let load = || Book::load(&path, "testnet-10", &pubkey(&f.guest)).unwrap();
    let mut recovered = load();
    assert!(!recovered.has_authorization(tx.id()));
    assert!(recovered.load_gameplay_outbox(&path).unwrap());
    let pending = recovered.pending().into_iter().find(|s| s.authorization.transaction.id() == tx.id()).unwrap();
    assert_eq!(serde_json::to_vec(&pending.authorization.transaction).unwrap(), serde_json::to_vec(&tx).unwrap());
    assert_eq!(pending.authorization.inputs, inputs);
    assert!(!recovered.direct_send.contains(&tx.id()));
    recovered.checkpoint(&path).unwrap();
    assert!(Book::outbox_files(&path).unwrap().is_empty());
    let mut recovered = load();
    assert!(recovered.pending_game(f.id));

    // Simulate a stop after committing a checkpoint but before deleting an
    // older record. Reopening must recognize it without counting the fee again.
    Book::save_gameplay_authorization(&path, signed).unwrap();
    recovered.apply(a, &[], &[block(3, 130, tx.clone(), inputs)]).unwrap();
    recovered.save(&path).unwrap();

    let mut checkpointed = load();
    assert!(checkpointed.load_gameplay_outbox(&path).unwrap());
    assert_eq!(checkpointed.session(f.id).unwrap().authorized_fees, f.left.session(f.id).unwrap().authorized_fees);
    assert!(checkpointed.was_confirmed(tx.id()));
    checkpointed.checkpoint(&path).unwrap();
    assert!(Book::outbox_files(&path).unwrap().is_empty());
    checkpointed.apply(a, &[hash(3)], &[]).unwrap();
    assert!(checkpointed.is_pending(tx.id()));
}

#[test]
fn reaccepted_lane_releases_a_contact_proof_signed_with_the_old_daa() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let Some(QueueStage::Live(game)) = f.left.sessions[0].stage.clone() else { panic!("expected live game") };
    let (side, until) = next_contact(&game.snapshot()).unwrap();
    // A move after contact remains in the replacement queue with its DAA.
    let accepted = until as u64 + 2;
    let owner = if side == 0 { f.guest } else { f.host };
    let wallet = if side == 0 { &mut f.left } else { &mut f.right };
    let lane = &game.sets[side].lanes[1];
    let funding = coin(98, &owner);
    let (parent, _) = reusable::command(b, lane, 1, &owner, &[funding.clone()], 100_000).unwrap();
    let parent_inputs = vec![lane.utxo.clone(), funding.utxo];
    sign(wallet, parent.clone(), parent_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    wallet.apply(a, &[], &[block(3, accepted, parent.clone(), parent_inputs.clone())]).unwrap();

    let stage = wallet.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!("expected live game") };
    let funding = coin(99, &owner);
    let (proof, _, _) = reusable::checkpoint(b, game, side, until, &owner, &[funding.clone()], 100_000).unwrap();
    let proof_inputs = entries(&proof, stage.cells(), &funding);
    sign(wallet, proof.clone(), proof_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();

    assert!(wallet.stale_spends(&stage.cells()).is_empty());
    wallet.apply(a, &[hash(3)], &[block(4, accepted + 1, parent.clone(), parent_inputs.clone())]).unwrap();
    assert!(wallet.is_conflicted(proof.id()));
    assert!(!wallet.pending_game(f.id));
    let mut retimed = proof_inputs.clone();
    for (input, source) in proof.inputs.iter().zip(&mut retimed) {
        if input.previous_outpoint.transaction_id == parent.id() { source.block_daa_score = accepted + 1; }
    }
    assert!(!super::super::rpc::retimed_retry_executes(&proof, &proof_inputs, &retimed, accepted + 1000),
        "maturity cannot make an invalid retimed Court proof retryable");

    let stage = wallet.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!("expected live game") };
    let stale = wallet.stale_spends(&stage.cells());
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, proof.id());
    assert_eq!(stale[0].replacement_fee(&proof).unwrap(), Some(stale[0].fee + 100));
    let (retry, _, _) = reusable::checkpoint(b, game, side, until, &owner, &[funding.clone()], 100_000).unwrap();
    assert_ne!(retry.id(), proof.id());
    let retry_inputs = entries(&retry, stage.cells(), &funding);
    sign(wallet, retry.clone(), retry_inputs, Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();
    assert!(wallet.is_pending(retry.id()));
    wallet.apply(a, &[hash(4)], &[block(3, accepted, parent, parent_inputs)]).unwrap();
    assert!(wallet.is_pending(proof.id()));
    assert!(wallet.is_conflicted(retry.id()));
    let stage = wallet.session(f.id).unwrap().stage.as_ref().unwrap();
    assert!(wallet.stale_spends(&stage.cells()).iter().all(|stale| stale.id != proof.id()));
}

#[test]
fn reaccepted_move_releases_a_child_signed_with_the_old_daa() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let QueueStage::Live(game) = &f.left.sessions[0].stage.as_ref().unwrap() else { panic!("expected live game") };
    let lane = &game.sets[0].lanes[1];
    let parent_funding = coin(96, &f.guest);
    let (parent, _) = reusable::command(b, lane, 1, &f.guest, &[parent_funding.clone()], 100_000).unwrap();
    let parent_inputs = vec![lane.utxo.clone(), parent_funding.utxo];
    sign(&mut f.left, parent.clone(), parent_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    f.left.apply(a, &[], &[block(3, 130, parent.clone(), parent_inputs.clone())]).unwrap();

    let QueueStage::Live(game) = &f.left.sessions[0].stage.as_ref().unwrap() else { panic!("expected live game") };
    let lane = &game.sets[0].lanes[1];
    let funding = coin(97, &f.guest);
    let (child, _) = reusable::command(b, lane, -1, &f.guest, &[funding.clone()], 100_000).unwrap();
    let child_inputs = vec![lane.utxo.clone(), funding.utxo.clone()];
    sign(&mut f.left, child.clone(), child_inputs, Effect::Game { session: f.id }, vec![], false);
    f.left.signed.last_mut().unwrap().authorization.purpose = "move".into();

    f.left.apply(a, &[hash(3)], &[block(4, 131, parent.clone(), parent_inputs.clone())]).unwrap();
    assert!(f.left.is_conflicted(child.id()));
    assert!(!f.left.is_pending(child.id()));
    let mut rollback = f.left.clone();
    rollback.apply(a, &[hash(4)], &[block(3, 130, parent, parent_inputs)]).unwrap();
    assert!(rollback.is_pending(child.id()), "undo restores the command if its source DAA returns");

    let stage = f.left.sessions[0].stage.as_ref().unwrap();
    let QueueStage::Live(game) = stage else { panic!("expected live game") };
    let lane = &game.sets[0].lanes[1];
    let (retry, _) = reusable::command(b, lane, -1, &f.guest, &[funding.clone()], 100_000).unwrap();
    assert_ne!(retry.id(), child.id());
    let stale = f.left.stale_spends(&stage.cells());
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, child.id());
    assert_eq!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap(), vec![(child.id(), 1, -1)]);
    let retry_mass = u128::from(transaction_mass(&retry).unwrap().priority);
    assert_eq!(stale[0].replacement_fee(&retry).unwrap(),
        Some((u128::from(stale[0].fee) * retry_mass / u128::from(stale[0].mass)) as u64 + 100));
    let other_funding = coin(95, &f.guest);
    let (unrelated, _) = reusable::command(b, &game.sets[0].lanes[2], -1, &f.guest, &[other_funding], 100_000).unwrap();
    assert_eq!(stale[0].replacement_fee(&unrelated).unwrap(), None);
    assert!(rollback.stale_spends(&rollback.session(f.id).unwrap().stage.as_ref().unwrap().cells()).is_empty());
    assert!(rollback.stale_move_repairs(&rollback.sessions[0]).unwrap().is_empty());
    let retry_inputs = vec![lane.utxo.clone(), funding.utxo];
    sign(&mut f.left, retry.clone(), retry_inputs, Effect::Game { session: f.id }, vec![], false);
    assert!(f.left.is_pending(retry.id()));
}

#[test]
fn two_reorged_move_lanes_can_be_repaired_individually_before_contact() {
    reorged_lane_repairs(false);
}

#[test]
fn pending_move_reprice_clears_the_stale_lane_and_survives_either_acceptance() {
    use super::super::{queue_service::reprice_pending_game_action, rpc::broadcast::ReplacementTerms};
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let Some(QueueStage::Live(game)) = f.left.sessions[0].stage.clone() else { panic!() };
    let lane = &game.sets[0].lanes[1];
    let funding = coin(215, &f.guest);
    let parent = reusable::command(b, lane, 1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let inputs = vec![lane.utxo.clone(), funding.utxo];
    f.left.apply(a, &[], &[block(3, 130, parent.clone(), inputs.clone())]).unwrap();
    let Some(QueueStage::Live(game)) = f.left.sessions[0].stage.clone() else { panic!() };
    let lane = &game.sets[0].lanes[1];
    let funding = coin(216, &f.guest);
    let stale = reusable::command(b, lane, -1, &f.guest, &[funding.clone()], 100_100).unwrap().0;
    sign(&mut f.left, stale.clone(), vec![lane.utxo.clone(), funding.utxo.clone()], Effect::Game { session: f.id }, vec![], false);
    f.left.signed.last_mut().unwrap().authorization.purpose = "move".into();
    f.left.apply(a, &[hash(3)], &[block(4, 131, parent, inputs)]).unwrap();
    assert!(f.left.is_conflicted(stale.id()));
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!() };
    let lane = &game.sets[0].lanes[1];
    let original = reusable::command(b, lane, -1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
    let original_inputs = vec![lane.utxo.clone(), funding.utxo];
    sign(&mut f.left, original.clone(), original_inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    f.left.signed.last_mut().unwrap().authorization.purpose = "move".into();
    let fee_coin = coin(217, &f.host);
    let timeout = reusable::timeout(b, game, &f.host, &[fee_coin.clone()], 100_000).unwrap().0;
    let timeout_inputs = entries(&timeout, stage.cells(), &fee_coin);
    f.left.apply(a, &[], &[block(5, 400, timeout, timeout_inputs)]).unwrap();
    let terms = ReplacementTerms { conflict: stale.id(), fee: 100_100, mass: transaction_mass(&stale).unwrap().priority };
    let config = Config::default();
    assert!(reprice_pending_game_action(&mut f.left, original.id(), ReplacementTerms { conflict: hash(99), ..terms }, &f.guest, 0, &config).is_err());
    assert!(f.left.is_pending(original.id()));
    let replacement = reprice_pending_game_action(&mut f.left, original.id(), terms, &f.guest, 0, &config).unwrap();
    assert!(!f.left.is_pending(original.id()));
    assert!(!f.left.is_conflicted(original.id()));
    let signed = f.left.pending().into_iter().find(|s| s.authorization.transaction.id() == replacement).unwrap().clone();
    assert!(signed.fee >= terms.fee + 100);
    assert_eq!(f.left.fee_replacement_targets().get(&original.id()), Some(&replacement));
    let encoded = serde_json::to_vec(&f.left).unwrap();
    let mut original_wins: Book = serde_json::from_slice(&encoded).unwrap();
    original_wins.apply(a, &[], &[block(6, 401, original.clone(), original_inputs)]).unwrap();
    assert!(original_wins.was_confirmed(original.id()));
    assert_eq!(original_wins.fee_replacement_targets().get(&replacement), Some(&original.id()));
    original_wins.apply(a, &[hash(6)], &[]).unwrap();
    assert!(original_wins.is_pending(replacement));
    f.left.apply(a, &[], &[block(7, 401, signed.authorization.transaction, signed.authorization.inputs)]).unwrap();
    assert!(f.left.was_confirmed(replacement));
    assert!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap().is_empty());
}

#[test]
fn settlement_repairs_keep_collection_funds_leased_until_accepted() {
    reorged_lane_repairs(true);
}

fn reorged_lane_repairs(settlement: bool) {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let Some(QueueStage::Live(game)) = f.left.sessions[0].stage.clone() else { panic!() };
    let mut parents = vec![];
    for lane in 1..=2 {
        let funding = coin(90 + lane as u8, &f.guest);
        let tx = reusable::command(b, &game.sets[0].lanes[lane], 1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
        parents.push(AcceptedTx { tx, inputs: vec![game.sets[0].lanes[lane].utxo.clone(), funding.utxo] });
    }
    f.left.apply(a, &[], &[ChainBlock { hash: hash(3), daa: 130, transactions: parents.clone() }]).unwrap();
    let Some(QueueStage::Live(game)) = f.left.sessions[0].stage.clone() else { panic!() };
    for lane in 1..=2 {
        let funding = coin(93 + lane as u8, &f.guest);
        let tx = reusable::command(b, &game.sets[0].lanes[lane], -1, &f.guest, &[funding.clone()], 100_000).unwrap().0;
        sign(&mut f.left, tx, vec![game.sets[0].lanes[lane].utxo.clone(), funding.utxo], Effect::Game { session: f.id }, vec![], false);
        f.left.signed.last_mut().unwrap().authorization.purpose = "move".into();
    }
    f.left.apply(a, &[hash(3)], &[ChainBlock { hash: hash(4), daa: 131, transactions: parents }]).unwrap();
    assert_eq!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap().len(), 2);
    let mut collection = None;
    if settlement {
        let unrelated = (0..HISTORY + 1).map(|i| {
            let mut bytes = [0u8; 32]; bytes[..8].copy_from_slice(&(1000 + i as u64).to_le_bytes());
            ChainBlock { hash: Hash::from_bytes(bytes), daa: 200, transactions: vec![] }
        }).collect::<Vec<_>>();
        f.left.apply(a, &[], &unrelated).unwrap();
        assert_eq!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap().len(), 2, "mempool repair evidence must outlive the undo window");
        let stage = f.left.sessions[0].stage.clone().unwrap();
        let QueueStage::Live(game) = &stage else { panic!() };
        let funding = coin(100, &f.host);
        let tx = reusable::timeout(b, game, &f.host, &[funding.clone()], 100_000).unwrap().0;
        let inputs = entries(&tx, stage.cells(), &funding);
        f.left.apply(a, &[], &[block(10, 400, tx, inputs)]).unwrap();
        // Reproduce journals already compacted by the old client. Restore only
        // known conflicted bytes, with positive, current sources from the node.
        let lost = f.left.signed.iter().filter(|s| s.authorization.purpose == "move").cloned().collect::<Vec<_>>();
        f.left.signed.retain(|s| s.authorization.purpose != "move");
        let mut found = f.left.all_cells().into_iter().map(|(outpoint, utxo)| Funding { outpoint, utxo }).collect::<Vec<_>>();
        for old in &lost {
            found.extend(old.authorization.transaction.inputs.iter().zip(&old.authorization.inputs)
                .filter(|(_, u)| u.covenant_id.is_none()).map(|(i, u)| Funding { outpoint: i.previous_outpoint, utxo: u.clone() }));
        }
        let mempool_entries = lost.iter().map(|s| (s.authorization.transaction.clone(), s.fee)).collect::<Vec<_>>();
        assert_eq!(f.left.restore_stale_moves(f.id, mempool_entries.clone(), &[]).unwrap(), 0, "missing inputs are not proof");
        assert_eq!(f.left.restore_stale_moves(f.id, mempool_entries.clone(), &found).unwrap(), 2);
        assert_eq!(f.left.restore_stale_moves(f.id, mempool_entries, &found).unwrap(), 0, "restoration is idempotent");
        assert_eq!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap().len(), 2);
        let stage = f.left.sessions[0].stage.clone().unwrap();
        let QueueStage::Collecting { result, sets } = &stage else { panic!() };
        let funding = coin(101, &f.guest);
        let tx = reusable::collect(b, result, sets, &f.guest, &[funding.clone()], 100_000).unwrap().0;
        let inputs = entries(&tx, stage.cells(), &funding);
        let fee = super::super::client::transaction_fee(&tx, &inputs).unwrap();
        collection = Some(tx.id());
        f.left.authorize(Signed {
            authorization: Authorization { transaction: tx, inputs, purpose: "collect queues".into(), successors: vec![], creates_set: false },
            effect: Effect::Game { session: f.id }, attempted_ms: 1, fee,
        }, &Config::default()).unwrap();
    }
    for lane in 1..=2 {
        let stage = f.left.sessions[0].stage.clone().unwrap();
        let sets = match &stage { QueueStage::Live(game) => &game.sets, QueueStage::Collecting { sets, .. } => sets, _ => panic!() };
        let funding = coin(96 + lane as u8, &f.guest);
        let tx = reusable::command(b, &sets[0].lanes[lane], -1, &f.guest, &[funding.clone()], 100_100).unwrap().0;
        let inputs = vec![sets[0].lanes[lane].utxo.clone(), funding.utxo];
        let stale = f.left.stale_spends(&vec![(sets[0].lanes[lane].outpoint, inputs[0].clone())]);
        assert_eq!(stale.len(), 1, "each replacement has exactly one mempool conflict");
        let repair = Signed {
            authorization: Authorization { transaction: tx.clone(), inputs: inputs.clone(), purpose: "move".into(), successors: vec![], creates_set: false },
            effect: Effect::Game { session: f.id }, attempted_ms: 0, fee: 100_100,
        };
        if settlement && lane == 1 {
            let mut unrelated = repair.clone();
            unrelated.authorization.purpose = "fixture".into();
            assert!(f.left.authorize(unrelated, &Config::default()).is_err(), "overlap is allowed only for a verified stale command repair");
            let funding = coin(101, &f.guest);
            let shared_fee = reusable::command(b, &sets[0].lanes[lane], -1, &f.guest, &[funding.clone()], 100_100).unwrap().0;
            let mut unsafe_repair = repair.clone();
            unsafe_repair.authorization.transaction = shared_fee;
            unsafe_repair.authorization.inputs[1] = funding.utxo;
            assert!(f.left.authorize(unsafe_repair, &Config::default()).is_err(), "collection wallet inputs must remain leased");
        }
        f.left.authorize(repair, &Config::default()).unwrap();
        if let Some(id) = collection { assert_eq!(f.left.is_pending(id), lane == 1, "preparation must not release the old collection's lease"); }
        f.left.apply(a, &[], &[block(10 + lane as u8, 409 + lane as u64, tx, inputs)]).unwrap();
        if let Some(id) = collection { assert!(f.left.is_conflicted(id), "accepted repair proves the collection cannot mine"); }
        assert_eq!(f.left.stale_move_repairs(&f.left.sessions[0]).unwrap().len(), 2 - lane);
    }
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
fn superseded_opponent_court_releases_resolve_and_undo_restores_it() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b); start_game(&mut f, a, b);
    let initial = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &initial else { panic!() };
    let (receiver, deadline) = next_contact(&game.snapshot()).unwrap();
    let other = 1 - receiver;
    let payer = if other == 0 { f.guest } else { f.host };
    let funding = coin(203, &payer);
    let parent = reusable::checkpoint(b, game, other, deadline - 1, &payer, &[funding.clone()], 100_000).unwrap().0;
    let parent_inputs = entries(&parent, initial.cells(), &funding);
    let replacement = reusable::checkpoint(b, game, other, deadline - 1, &payer, &[funding.clone()], 200_000).unwrap().0;
    let replacement_inputs = entries(&replacement, initial.cells(), &funding);
    accept_both(&mut f, a, &block(3, deadline as u64 + 2, parent.clone(), parent_inputs.clone()));
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!() };
    let (side, until) = next_contact(&game.snapshot()).unwrap(); assert_eq!(side, receiver);
    let owner = if side == 0 { f.guest } else { f.host };
    let wallet = if side == 0 { &mut f.left } else { &mut f.right };
    let fee_coin = coin(204, &owner);
    let child = reusable::checkpoint(b, game, side, until, &owner, &[fee_coin.clone()], 100_000).unwrap().0;
    let inputs = entries(&child, stage.cells(), &fee_coin);
    sign(wallet, child.clone(), inputs, Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();
    wallet.signed.last_mut().unwrap().attempted_ms = 1;
    let successor = wallet.resolve_successor(f.id).unwrap().unwrap();
    let move_coin = coin(205, &owner);
    let movement = reusable::command(b, &successor.sets[side].lanes[1], 1, &owner, &[move_coin.clone()], 100_000).unwrap().0;
    let move_inputs = entries(&movement, QueueStage::Live(successor).cells(), &move_coin);
    sign(wallet, movement.clone(), move_inputs, Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "move".into();
    let mut no_proof = wallet.clone();
    no_proof.apply(a, &[hash(3)], &[]).unwrap();
    no_proof.sessions[0].unavailable = true;
    no_proof.apply(a, &[], &[block(4, deadline as u64 + 3, replacement.clone(), replacement_inputs.clone())]).unwrap();
    assert!(no_proof.is_pending(child.id()), "unavailable session cannot prove its canonical Court");
    let mut unchanged = wallet.clone();
    unchanged.apply(a, &[hash(3)], &[block(4, deadline as u64 + 2, parent.clone(), parent_inputs.clone())]).unwrap();
    assert!(unchanged.is_pending(child.id()), "transient rollback must not retire a re-added parent");
    wallet.apply(a, &[hash(3)], &[block(4, deadline as u64 + 3, replacement, replacement_inputs)]).unwrap();
    let Some(QueueStage::Live(current)) = &wallet.sessions[0].stage else { panic!() };
    assert_ne!(child.inputs[0].previous_outpoint, current.court.outpoint);
    assert!(!wallet.is_pending(child.id()));
    assert!(wallet.conflicted.contains(&child.id()));
    assert!(wallet.is_conflicted(movement.id()), "obsolete resolve descendants release their leases too");
    assert!(current.sets[side].lanes.iter().all(|c| child.inputs.iter().any(|i|i.previous_outpoint == c.outpoint)),
        "replacement preserves the receiver lanes that the obsolete resolve leased");
    let (side, until) = next_contact(&current.snapshot()).unwrap();
    let fresh = reusable::checkpoint(b, current, side, until, &owner, &[fee_coin.clone()], 100_000).unwrap().0;
    let sources = entries(&fresh, wallet.sessions[0].stage.as_ref().unwrap().cells(), &fee_coin);
    sign(wallet, fresh.clone(), sources, Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();
    assert!(wallet.is_pending(fresh.id()), "replacement can reserve the released lanes and fee input");
    wallet.apply(a, &[hash(4)], &[block(5, deadline as u64 + 2, parent, parent_inputs)]).unwrap();
    assert!(wallet.is_pending(child.id()), "undo restores the original resolve's reservations");
    assert!(wallet.is_pending(movement.id()), "undo restores descendant reservations too");
    assert!(!wallet.is_pending(fresh.id()), "replacement is now the obsolete branch");
}

#[test]
fn prepared_move_depends_on_its_lane_and_exact_acceptance_metadata() {
    use super::super::queue_service::gameplay_inputs_current;
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let stage = f.left.sessions[0].stage.as_ref().unwrap();
    let QueueStage::Live(game) = stage else { panic!("expected live game") };
    let lane = &game.sets[0].lanes[1];
    let funding = coin(208, &f.guest);
    let (tx, _) = reusable::command(b, lane, 1, &f.guest, &[funding.clone()], 100_000).unwrap();
    let mut cells = stage.cells();
    let sources = entries(&tx, cells.clone(), &funding);
    assert!(gameplay_inputs_current(&tx, &sources, cells.iter()));
    let court = cells.iter_mut().find(|(outpoint, _)| *outpoint == game.court.outpoint).unwrap();
    court.1.block_daa_score -= 1;
    assert!(gameplay_inputs_current(&tx, &sources, cells.iter()), "a move does not read the Court input");
    let input = cells.iter_mut().find(|(outpoint, _)| *outpoint == lane.outpoint).unwrap();
    input.1.block_daa_score -= 1;
    assert!(!gameplay_inputs_current(&tx, &sources, cells.iter()), "its lane acceptance DAA is a dependency");
}

#[test]
fn valid_retimed_resolve_retries_only_after_actual_sequence_maturity() {
    let (a, b) = reusable::current_runtime().unwrap();
    let mut f = fixture(b);
    start_game(&mut f, a, b);
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!() };
    let (side, until) = next_contact(&game.snapshot()).unwrap();
    let payer = if side == 0 { f.guest } else { f.host };
    let funding = coin(201, &payer);
    let parent = reusable::checkpoint(b, game, side, until, &payer, &[funding.clone()], 100_000).unwrap().0;
    let parent_inputs = entries(&parent, stage.cells(), &funding);
    let birth = until as u64 + 10;
    accept_both(&mut f, a, &block(3, birth, parent.clone(), parent_inputs.clone()));
    let stage = f.left.sessions[0].stage.clone().unwrap();
    let QueueStage::Live(game) = &stage else { panic!() };
    let (side, until) = next_contact(&game.snapshot()).unwrap();
    let payer = if side == 0 { f.guest } else { f.host };
    let wallet = if side == 0 { &mut f.left } else { &mut f.right };
    let funding = coin(202, &payer);
    let tx = reusable::checkpoint(b, game, side, until, &payer, &[funding.clone()], 100_000).unwrap().0;
    let inputs = entries(&tx, stage.cells(), &funding);
    assert!(tx.inputs[0].sequence > 0);
    sign(wallet, tx.clone(), inputs.clone(), Effect::Game { session: f.id }, vec![], false);
    wallet.signed.last_mut().unwrap().authorization.purpose = "resolve".into();
    wallet.apply(a, &[hash(3)], &[block(4, birth + 2, parent.clone(), parent_inputs)]).unwrap();
    let signed = wallet.signed.iter().find(|s| s.authorization.transaction.id() == tx.id()).unwrap();
    let mut actual = inputs.clone();
    for (i, source) in tx.inputs.iter().zip(&mut actual) {
        if i.previous_outpoint.transaction_id == parent.id() { source.block_daa_score = birth + 2; }
    }
    assert!(!signed.fails_with(actual.clone()), "the resolve remains valid after the later parent acceptance");
    assert!(wallet.is_pending(tx.id()), "valid uncertain authorization keeps its lane leases");
    assert!(!super::super::rpc::accepted_source_executes(&tx, &inputs, 0, &actual[0]),
        "fresh authorization keeps its conservative pending-parent rule");
    use super::super::rpc::retimed_retry_executes;
    use kaspa_consensus_core::constants::{SEQUENCE_LOCK_TIME_MASK, SEQUENCE_LOCK_TIME_DISABLED};
    let maturity = tx.inputs.iter().zip(&actual)
        .filter(|(i, _)| i.sequence & SEQUENCE_LOCK_TIME_DISABLED == 0)
        .map(|(i, u)| u.block_daa_score + (i.sequence & SEQUENCE_LOCK_TIME_MASK)).max().unwrap();
    assert!(!retimed_retry_executes(&tx, &inputs, &actual, maturity - 1));
    assert!(retimed_retry_executes(&tx, &inputs, &actual, maturity));
    let mut wrong = actual.clone(); wrong[0].amount += 1;
    assert!(!retimed_retry_executes(&tx, &inputs, &wrong, maturity));
    let mut pending = actual.clone(); pending[0].block_daa_score = u64::MAX;
    assert!(!retimed_retry_executes(&tx, &inputs, &pending, maturity));
    let mut invalid = tx.clone(); invalid.inputs[0].signature_script.clear();
    assert!(!retimed_retry_executes(&invalid, &inputs, &actual, maturity));
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
