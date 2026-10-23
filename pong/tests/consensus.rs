//! Contract consensus checks: real validation, disposable local funds.
use argent::{build_file, builder::TxBuilder};
use engine::*;
use kaspa_consensus::{
    config::ConfigBuilder,
    consensus::test_consensus::TestConsensus,
    params::{DEVNET_PARAMS, ForkActivation},
};
use kaspa_consensus_core::{
    Hash,
    api::{ConsensusApi, args::TransactionValidationArgs},
    blockstatus::BlockStatus,
    header::Header,
    muhash::MuHashExtensions,
    tx::{MutableTransaction, Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_muhash::MuHash;
use kaspa_pong::engine;
use secp256k1::{Keypair, Secp256k1, SecretKey};

fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}
fn out(n: u8) -> TransactionOutpoint {
    TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0)
}
fn actual(consensus: &TestConsensus, outpoint: TransactionOutpoint) -> UtxoEntry {
    let mut tx = MutableTransaction::from_tx(TxBuilder::transaction(vec![TxBuilder::transaction_input(outpoint, vec![])], vec![]));
    consensus.populate_mempool_transaction(&mut tx).unwrap();
    tx.entries[0].clone().unwrap()
}
async fn block(consensus: &TestConsensus, tip: &mut Hash, number: &mut u64, txs: Vec<Transaction>) {
    *number += 1;
    for tx in &txs {
        let mut candidate = MutableTransaction::from_tx(tx.clone());
        if let Err(error) = consensus.validate_mempool_transaction(&mut candidate, &TransactionValidationArgs::default()) {
            panic!(
                "tx rejected at tip DAA {}: {error:?}; sequence {}; source DAAs {:?}",
                consensus.get_header(*tip).unwrap().daa_score,
                tx.inputs[0].sequence,
                candidate.entries.iter().map(|u| u.as_ref().map(|u| u.block_daa_score)).collect::<Vec<_>>()
            );
        }
    }
    let hash = (*number).into();
    let status = consensus.add_utxo_valid_block_with_parents(hash, vec![*tip], txs).await;
    assert!(matches!(status, Ok(BlockStatus::StatusUTXOValid)), "block {number}: {status:?}");
    *tip = hash;
}

#[tokio::test]
async fn independent_commands_and_idle_physics_survive_actual_consensus() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = build_file(concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/pong.ag"), dir.path()).unwrap();
    let builder = TxBuilder::new(&artifact).unwrap();
    let id = Hash::from_bytes([77; 32]);
    let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)), pubkey(&key(3)));
    world.started = true;
    world.ball_tick_daa = 2;
    let qs = empty_queues(&world.left, &world.right);
    let mut snapshot = Snapshot {
        court: Cell {
            outpoint: out(1),
            utxo: builder.covenant_utxo("Court", world.state(), COURT_DEPOSIT + 2 * DEFAULT_STAKE as u64, 0, false, Some(id)).unwrap(),
            state: world,
        },
        players: std::array::from_fn(|i| Cell {
            outpoint: out(2 + i as u8),
            utxo: builder.covenant_utxo(qs[i].actor(), qs[i].state(), QUEUE_DEPOSIT, 0, false, Some(id)).unwrap(),
            state: qs[i].clone(),
        }),
    };
    let sponsor = key(4);
    let mut fees: [Funding; 3] = std::array::from_fn(|i| Funding {
        outpoint: out(90 + i as u8),
        utxo: UtxoEntry::new(100_000_000_000, p2pk(&pubkey(&sponsor)), 0, false, None),
    });
    let mut seeds = vec![(snapshot.court.outpoint, snapshot.court.utxo.clone())];
    seeds.extend(snapshot.players.iter().map(|p| (p.outpoint, p.utxo.clone())));
    seeds.extend(fees.iter().map(|f| (f.outpoint, f.utxo.clone())));
    let config = ConfigBuilder::new(DEVNET_PARAMS)
        .skip_proof_of_work()
        .edit_consensus_params(|params| {
            let mut set = MuHash::new();
            seeds.iter().for_each(|(o, u)| set.add_utxo(o, u));
            params.genesis.utxo_commitment = set.finalize();
            params.genesis.hash = Header::from(&params.genesis).hash;
            params.coinbase_maturity = 0;
            params.crescendo_activation = ForkActivation::always();
            params.toccata_activation = ForkActivation::always();
        })
        .build();
    let consensus = TestConsensus::new(&config);
    let mut set = MuHash::new();
    consensus.append_imported_pruning_point_utxos(&seeds, &mut set);
    consensus.import_pruning_point_utxo_set(config.genesis.hash, set).unwrap();
    let handles = consensus.init();
    let mut tip = config.genesis.hash;
    let mut number = 1000;
    // Disjoint movement inputs still work together in a real consensus block.
    let (left, lq) = command_tx(&builder, &snapshot.players[0], -1, &key(1), &fees[0], &sponsor, 1_000_000).unwrap();
    let (right, rq) = command_tx(&builder, &snapshot.players[1], 1, &key(2), &fees[1], &sponsor, 1_000_000).unwrap();
    block(&consensus, &mut tip, &mut number, vec![left.clone(), right.clone()]).await;
    block(&consensus, &mut tip, &mut number, vec![]).await;
    snapshot.players[0] = accepted_cell(&left, 0, 0, lq);
    snapshot.players[1] = accepted_cell(&right, 0, 0, rq);
    for p in &mut snapshot.players {
        p.utxo = actual(&consensus, p.outpoint);
    }
    let (receiver, deadline) = next_contact(&snapshot).unwrap();
    let (early, _) = checkpoint_tx(&builder, &snapshot, receiver, deadline, &key(receiver as u8 + 1), &fees[2], &sponsor, 1_000_000).unwrap();
    let error = consensus
        .validate_mempool_transaction(&mut MutableTransaction::from_tx(early), &TransactionValidationArgs::default())
        .unwrap_err();
    assert!(matches!(error, kaspa_consensus_core::errors::tx::TxRuleError::SequenceLockConditionsAreNotMet), "{error:?}");
    // Force a miss at every contact using real signed moves before its deadline.
    // No caller-provided time is trusted by the covenant or consensus.
    for _ in 0..6 {
        let (side, deadline) = next_contact(&snapshot).unwrap();
        let w = &snapshot.court.state;
        let ticks = (if w.ball_vx == 1 { RIGHT_CONTACT_X - w.ball_x } else { w.ball_x - LEFT_CONTACT_X } + 1) / 2;
        let phase = (w.ball_y + w.ball_vy * ticks).rem_euclid(62);
        let arrival = if phase <= 31 { phase } else { 62 - phase };
        let target = if arrival < 16 { 28 } else { 3 };
        let now = consensus.get_virtual_daa_score();
        let view = project(&snapshot, now).unwrap();
        let y = if side == 0 { view.world.left_y } else { view.world.right_y };
        if target != y {
            let (tx, q) =
                command_tx(&builder, &snapshot.players[side], target - y, &key(side as u8 + 1), &fees[2], &sponsor, 1_000_000)
                    .unwrap();
            block(&consensus, &mut tip, &mut number, vec![tx.clone()]).await;
            block(&consensus, &mut tip, &mut number, vec![]).await;
            snapshot.players[side] = accepted_cell(&tx, 0, 0, q);
            snapshot.players[side].utxo = actual(&consensus, snapshot.players[side].outpoint);
            let out = TransactionOutpoint::new(tx.id(), (tx.outputs.len() - 1) as u32);
            fees[2] = Funding { outpoint: out, utxo: actual(&consensus, out) };
        }
        while consensus.get_virtual_daa_score() <= deadline as u64 + 1 {
            block(&consensus, &mut tip, &mut number, vec![]).await;
        }
        let (tx, plan) = resolve_tx(&builder, &snapshot, &key(side as u8 + 1), &fees[2], &sponsor, 1_000_000, false).unwrap();
        assert_eq!(tx.inputs.len(), 5);
        let other = snapshot.players[1 - side].outpoint;
        block(&consensus, &mut tip, &mut number, vec![tx.clone()]).await;
        block(&consensus, &mut tip, &mut number, vec![]).await;
        snapshot = accepted_transition(&builder, &snapshot, &tx, plan, 0).unwrap();
        snapshot.court.utxo = actual(&consensus, snapshot.court.outpoint);
        for p in &mut snapshot.players {
            p.utxo = actual(&consensus, p.outpoint);
        }
        assert_eq!(snapshot.players[1 - side].outpoint, other);
        let out = TransactionOutpoint::new(tx.id(), (tx.outputs.len() - 1) as u32);
        fees[2] = Funding { outpoint: out, utxo: actual(&consensus, out) };
        if snapshot.court.state.ended() {
            break;
        }
    }
    assert!(snapshot.court.state.ended());
    let settle = settle_tx(&builder, &snapshot, &fees[2], &sponsor, 1_000_000).unwrap();
    block(&consensus, &mut tip, &mut number, vec![settle.clone()]).await;
    block(&consensus, &mut tip, &mut number, vec![]).await;
    let mut finish = accepted_collection(&snapshot, &settle, snapshot.court.state.result(), 0);
    finish.collector.utxo = actual(&consensus, finish.collector.outpoint);
    let out = TransactionOutpoint::new(settle.id(), (settle.outputs.len() - 1) as u32);
    fees[2] = Funding { outpoint: out, utxo: actual(&consensus, out) };
    let refund = reclaim_tx(&builder, &finish, &fees[2], &sponsor, 1_000_000).unwrap();
    block(&consensus, &mut tip, &mut number, vec![refund.clone()]).await;
    block(&consensus, &mut tip, &mut number, vec![]).await;
    assert_eq!(actual(&consensus, TransactionOutpoint::new(refund.id(), 0)).amount, GAME_DEPOSIT);
    println!(
        "real consensus: early contact checkpoint rejected, independent moves accepted, contact/settlement and full seven-coin refund passed"
    );
    consensus.shutdown(handles);
}
