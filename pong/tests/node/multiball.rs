//! Accepted-state integration: invitation terms, contact replay, and rollback.
use argent_runtime::{Artifact, TxBuilder};
use kaspa_consensus_core::{Hash, tx::{TransactionOutpoint, UtxoEntry}};
use kaspa_pong::{engine::*, node::{invitations::{self, GameTerms}, tracker::*}};
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn hash(n: u8) -> Hash { Hash::from_bytes([n; 32]) }
fn funding(n: u8, owner: u8) -> Funding {
    Funding { outpoint: TransactionOutpoint::new(hash(n), 0),
        utxo: UtxoEntry::new(10_000_000_000, p2pk(&pubkey(&key(owner))), 1, false, None) }
}
fn block(n: u8, daa: u64, tx: kaspa_consensus_core::tx::Transaction, inputs: Vec<UtxoEntry>) -> ChainBlock {
    ChainBlock { hash: hash(n), daa, transactions: vec![AcceptedTx { tx, inputs }] }
}

#[test]
fn multiball_terms_contacts_and_rollback_follow_accepted_state() {
    let artifact: Artifact = serde_json::from_str(include_str!("../../artifacts/artifact.json")).unwrap();
    let builder = TxBuilder::new(&artifact).unwrap();
    let terms = GameTerms { speed: GameSpeed::Manual(1), ball_count: 8, win_score: 15, stake_kas: 5 };
    let tag = invitations::new_rendezvous_tag(terms).unwrap();
    let lobby = Lobby { left: pubkey(&key(1)), right: pubkey(&key(2)), refund: pubkey(&key(1)),
        control_value: SIDE_DEPOSIT as i64, ball_tick_daa: terms.speed.tick_daa(),
        stake_per_player: DEFAULT_STAKE, ball_count: 8, win_score: 15 };
    let creation_funds = funding(90, 1);
    let invite = invitations::create_with_rendezvous(&artifact, "devnet", hash(0), &lobby,
        &creation_funds, &key(1), &key(1), 100_000, tag).unwrap();
    let code = invitations::rendezvous_code(&lobby.right, &tag).unwrap();
    assert_eq!(invitations::parse_rendezvous_code(&code).unwrap().2, terms);
    assert_eq!(invitations::rendezvous_tag(&invite.genesis).unwrap(), tag);
    assert_eq!(invitations::creation_payment(&invite).unwrap().sompi, 100_000);
    let mut wrong = lobby.clone(); wrong.win_score = 9;
    assert!(invitations::create_with_rendezvous(&artifact, "devnet", hash(0), &wrong,
        &creation_funds, &key(1), &key(1), 100_000, tag).is_err());
    let mut tracker = Tracker::new(&invite);
    tracker.apply(&artifact, &invite, &[], &[block(10, 100, invite.genesis.clone(), vec![creation_funds.utxo])]).unwrap();
    let Stage::Lobby(cell) = tracker.stage.clone() else { panic!("Lobby not observed") };
    let host_funds = funding(91, 2);
    let (start, _) = start_tx(&builder, &cell, &key(2), &host_funds, &key(2), 100_000).unwrap();
    tracker.apply(&artifact, &invite, &[], &[block(11, 102, start, vec![cell.utxo, host_funds.utxo])]).unwrap();
    for turn in 0..8u8 {
        let Stage::Live(snapshot) = tracker.stage.clone() else { panic!("game ended unexpectedly") };
        assert_eq!(snapshot.court.state.ball_count, 8);
        assert_eq!(snapshot.court.state.win_score, 15);
        let before = snapshot.court.state.clone();
        let (receiver, deadline) = next_contact(&snapshot).unwrap();
        let owner = receiver as u8 + 1;
        let fees = funding(100 + turn, owner);
        let (tx, plan) = resolve_tx(&builder, &snapshot, &key(owner), &fees, &key(owner), 100_000, false).unwrap();
        let mut inputs = vec![snapshot.court.utxo.clone()];
        inputs.extend([receiver, receiver + 2, receiver + 4].map(|i| snapshot.players[i].utxo.clone()));
        inputs.push(fees.utxo);
        let accepted = block(20 + turn, deadline as u64 + 1, tx, inputs);
        tracker.apply(&artifact, &invite, &[], &[accepted.clone()]).unwrap();
        let Stage::Live(after) = &tracker.stage else { panic!("contact lost live state") };
        assert_eq!(after.court.state, plan.world);
        tracker.apply(&artifact, &invite, &[accepted.hash], &[]).unwrap();
        let Stage::Live(restored) = &tracker.stage else { panic!("rollback lost live state") };
        assert_eq!(restored.court.state, before);
        tracker.apply(&artifact, &invite, &[], &[accepted]).unwrap();
    }
}
