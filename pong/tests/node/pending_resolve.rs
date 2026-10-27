use super::*;

fn pending_contact() -> (argent_runtime::Artifact, Invitation, Cache, usize, i64) {
    let a = artifact();
    let (invite, mut tracker, lobby) = fixture(&a);
    start(&a, &invite, &mut tracker, &lobby);
    let Stage::Live(s) = &tracker.stage else { panic!() };
    let (side, deadline) = next_contact(s).unwrap();
    let f = funding(150);
    let (tx, _) = resolve_tx(&TxBuilder::new(&a).unwrap(), s, &key(side as u8 + 1), &f, &key(3), 100_000, false).unwrap();
    let mut entries = vec![s.court.utxo.clone()];
    entries.extend([side, side + 2, side + 4].map(|i| s.players[i].utxo.clone()));
    entries.push(f.utxo);
    let mut cache = Cache::new(&invite);
    cache.tracker = tracker;
    cache.tracker.daa = deadline as u64;
    cache.authorize(tx, entries, "contact".into(), &node::Config::default()).unwrap();
    (a, invite, cache, side, deadline)
}

fn child(a: &argent_runtime::Artifact, cache: &Cache, side: usize, coin: u8) -> (kaspa_consensus_core::tx::Transaction, Vec<UtxoEntry>) {
    let lane = cache.available_lane(side as i64).unwrap();
    let Stage::Live(s) = cache.movement_stage().unwrap() else { panic!() };
    let f = funding(coin);
    let (tx, _) = command_tx(&TxBuilder::new(a).unwrap(), &s.players[lane], 1,
        &key(side as u8 + 1), &f, &key(3), 100_000).unwrap();
    (tx, vec![s.players[lane].utxo.clone(), f.utxo])
}

#[test]
fn three_children_use_replacement_lanes_and_keep_accepted_state_unchanged() {
    let (a, invite, mut cache, side, deadline) = pending_contact();
    let old_court = cache.tracker.stage.cells()[0].0;
    assert_eq!(cache.next_action(side as i64, deadline as u64, Some(1)).unwrap(), None);
    cache.pending[0].attempted_ms = 1;
    for n in 0..3 {
        assert_eq!(cache.next_action(side as i64, deadline as u64, Some(1)).unwrap(), Some(Action::Move(1)));
        let (tx, entries) = child(&a, &cache, side, 151 + n);
        assert_eq!(tx.inputs[0].previous_outpoint.transaction_id, cache.pending[0].transaction.id());
        assert_eq!(entries[0].block_daa_score, u64::MAX);
        assert!(cache.draft_current(&tx, &entries));
        cache.authorize(tx, entries, "move 1".into(), &node::Config::default()).unwrap();
        assert_eq!(cache.next_action(side as i64, deadline as u64, None).unwrap(), None);
    }
    assert_eq!(cache.pending.len(), 4);
    assert!(cache.available_lane(side as i64).is_none());
    assert_eq!(cache.next_action(side as i64, deadline as u64, Some(1)).unwrap(), None);
    assert_eq!(cache.tracker.stage.cells()[0].0, old_court);
    assert!(!cache.discard_conflicting_pending());
    cache.validate(&a, &invite).unwrap();
    let recovered: Cache = serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();
    recovered.validate(&a, &invite).unwrap();
    assert_eq!(recovered.pending.len(), 4);
}

#[test]
fn parent_acceptance_keeps_child_draft_and_reservations_current() {
    let (a, invite, mut cache, side, deadline) = pending_contact();
    cache.pending[0].attempted_ms = 1;
    let parent = cache.pending[0].clone();
    let (tx, entries) = child(&a, &cache, side, 154);
    cache.authorize(tx.clone(), entries.clone(), "move 1".into(), &node::Config::default()).unwrap();
    let mut validated = cache.clone();
    validated.tracker.apply(&a, &invite, &[], &[ChainBlock {
        hash: hash(151), daa: deadline as u64 + 2,
        transactions: vec![AcceptedTx { tx: parent.transaction, inputs: parent.inputs }],
    }]).unwrap();
    cache.publish_validated(validated, false);
    assert_eq!(cache.pending.len(), 1);
    assert!(cache.draft_current(&tx, &entries));
    assert!(!cache.discard_conflicting_pending());
    cache.validate(&a, &invite).unwrap();
    cache.tracker.history.clear();
    assert!(cache.draft_current(&tx, &entries), "the accepted source itself proves provenance after undo history expires");
}

#[test]
fn rejected_parent_drops_children_but_retains_fresh_durable_reservations() {
    let (a, _, mut cache, side, _) = pending_contact();
    cache.pending[0].attempted_ms = 1;
    let parent = cache.pending[0].transaction.id();
    let (tx, entries) = child(&a, &cache, side, 155);
    let fresh = cache.authorize_unsubmitted(tx.clone(), entries.clone(), "move 1".into(), &node::Config::default()).unwrap();
    cache.discard_pending(parent);
    assert_eq!(cache.pending.len(), 2);
    assert!(cache.pending.iter().all(|p| p.branch_changed));
    assert!(!cache.draft_current(&tx, &entries));
    cache.begin_withdrawal(&fresh).unwrap();
    cache.complete_withdrawal(fresh.id()).unwrap();
    cache.discard_pending(parent);
    assert!(cache.pending.is_empty());
}

#[test]
fn unavailable_parent_and_next_contact_block_new_movement() {
    let (a, _, mut cache, side, deadline) = pending_contact();
    cache.pending[0].attempted_ms = 1;
    let Stage::Live(s) = cache.movement_stage().unwrap() else { panic!() };
    let (receiver, next) = next_contact(&s).unwrap();
    assert!(!cache.movement_allowed(receiver as i64, next as u64).unwrap());
    let (tx, entries) = child(&a, &cache, side, 156);
    cache.pending[0].branch_changed = true;
    assert_eq!(cache.next_action(side as i64, deadline as u64, Some(1)).unwrap(), None);
    assert!(!cache.draft_current(&tx, &entries));
}

#[test]
fn reconnect_keeps_children_reserved_until_parent_availability_is_rechecked() {
    let (a, _, mut cache, side, _) = pending_contact();
    cache.pending[0].attempted_ms = 1;
    let parent = cache.pending[0].transaction.id();
    let (tx, entries) = child(&a, &cache, side, 157);
    cache.authorize(tx.clone(), entries.clone(), "move 1".into(), &node::Config::default()).unwrap();
    for p in &mut cache.pending { p.branch_changed = true; }
    assert!(!cache.discard_conflicting_pending());
    assert_eq!(cache.pending.len(), 2);
    assert_eq!(cache.source_parents(&tx, &entries).iter().map(|p| p.id()).collect::<Vec<_>>(), vec![parent]);
    assert!(!cache.draft_current(&tx, &entries));
    cache.pending[0].branch_changed = false;
    assert!(cache.draft_current(&tx, &entries));
}

#[test]
fn prepared_movement_yields_to_an_available_opponent_timeout() {
    let (_, _, mut cache, receiver, deadline) = pending_contact();
    cache.pending.clear();
    let daa = deadline as u64 + RESPONSE_TIMEOUT_DAA;
    assert_eq!(action(&cache.tracker.stage, 1 - receiver as i64, daa, None).unwrap(), Some(Action::Timeout));
    assert!(!cache.movement_allowed(1 - receiver as i64, daa).unwrap());
}
