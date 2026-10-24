//! Client/tracker, invitation, and opt-in disposable-node integration checks.

mod client {
    use argent_runtime::TxBuilder;
    use engine::*;
    use kaspa_consensus_core::{
        Hash,
        tx::{TransactionOutpoint, UtxoEntry},
    };
    use kaspa_pong::{ascii, engine, menu, network_ui, node, terminal};
    use node::{
        client::{Action, Cache, action},
        setup,
        tracker::*,
    };
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    fn hash(n: u8) -> Hash {
        Hash::from_bytes([n; 32])
    }
    fn funding(n: u8) -> Funding {
        Funding {
            outpoint: TransactionOutpoint::new(hash(n), 0),
            utxo: UtxoEntry::new(10_000_000_000, p2pk(&pubkey(&key(3))), 1, false, None),
        }
    }
    fn guest_funding(n: u8) -> Funding {
        let mut coin = funding(n);
        coin.utxo.script_public_key = p2pk(&pubkey(&key(2)));
        coin
    }
    fn approve_start(a: &argent_runtime::Artifact, invite: &Invitation, p: setup::StartPackage,
        right: &Keypair, max_fee: u64) -> Result<setup::StartPackage> {
        let config = node::Config { fee_sompi: max_fee, relay_fee_per_kg: 1, ..Default::default() };
        setup::approve(a, invite, p, vec![guest_funding(91)], right, &key(2), &config)
    }
    fn artifact() -> argent_runtime::Artifact {
        node::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts/artifact.json"))).unwrap()
    }
    fn fixture(a: &argent_runtime::Artifact) -> (Invitation, Tracker, Cell<Lobby>) {
        let lobby = Lobby {
            left: pubkey(&key(1)),
            right: pubkey(&key(2)),
            refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64,
            ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE,
        };
        let f = funding(90);
        let i = node::invitations::create(a, "devnet", hash(0), &lobby, &f, &key(1), &key(3), 100_000).unwrap();
        let genesis = i.genesis.clone();
        i.validate(a).unwrap();
        let mut t = Tracker::new(&i);
        t.apply(
            a,
            &i,
            &[],
            &[ChainBlock { hash: hash(10), daa: 100, transactions: vec![AcceptedTx { tx: genesis.clone(), inputs: vec![f.utxo] }] }],
        )
        .unwrap();
        (i, t, accepted_cell(&genesis, 0, 100, lobby))
    }
    fn start(a: &argent_runtime::Artifact, i: &Invitation, t: &mut Tracker, lobby: &Cell<Lobby>) {
        let f = guest_funding(91);
        let (tx, _) = start_tx(&TxBuilder::new(a).unwrap(), lobby, &key(2), &f, &key(2), 100_000).unwrap();
        t.apply(
            a,
            i,
            &[],
            &[ChainBlock { hash: hash(11), daa: 102, transactions: vec![AcceptedTx { tx, inputs: vec![lobby.utxo.clone(), f.utxo] }] }],
        )
        .unwrap();
    }

    mod stake_only_mass {
        use super::*;
        use node::client::{priced_transaction, relay_fee, transaction_fee};

        #[test]
        fn full_stake_only_lifecycle_fits_mass_and_external_fee_policy() {
            let a = artifact();
            let b = TxBuilder::new(&a).unwrap();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            assert_eq!((lobby.utxo.amount, s.court.utxo.amount), (500_000_000, 400_000_000));
            assert_eq!(s.court.utxo.amount + s.players.iter().map(|p| p.utxo.amount).sum::<u64>(), GAME_POT);
            assert!(!s.court.state.state().contains_key("refund"));
            let config = node::Config::default();
            let check = |label: &str, tx: &kaspa_consensus_core::tx::Transaction, entries: Vec<UtxoEntry>| {
                let mass = storage_mass(tx, entries.clone()).unwrap();
                let limit = kaspa_consensus_core::config::params::MAINNET_PARAMS.block_mass_limits().after().storage;
                assert!(mass <= limit && mass == tx.storage_mass(), "{label}: storage {mass}/{limit}");
                assert!(tx.outputs.iter().all(|o| o.value > 0), "no zero collateral/dust placeholder");
                let fee = transaction_fee(tx, &entries).unwrap();
                assert!(fee >= relay_fee(tx, &config).unwrap() && fee <= config.fee_sompi);
                for i in 0..entries.len() { argent_runtime::execute_input_with_covenants(tx, entries.clone(), i).unwrap(); }
                let masses = kaspa_consensus_core::mass::MassCalculator::new_with_consensus_params(
                    &kaspa_consensus_core::config::params::MAINNET_PARAMS).calc_non_contextual_masses(tx);
                let bytes = kaspa_consensus_core::mass::transaction_estimated_serialized_size(tx);
                println!("{label}: bytes={bytes}, compute={}, transient={}, storage={mass}/{limit}, fee={fee} sompi",
                    masses.compute_mass, masses.transient_mass);
            };
            for amount in [600_000_000, 1_500_000_000, 10_000_000_000] {
                let mut host = funding(201); host.utxo.amount = amount;
                let creation = priced_transaction(&config, |fee| Ok(node::invitations::create(&a, "devnet", hash(0), &invite.lobby, &host, &key(1), &key(3), fee)?.genesis)).unwrap();
                check("host", &creation, vec![host.utxo.clone()]);
                assert_eq!(creation.outputs[0].value, DEFAULT_STAKE as u64);
                let mut guest = guest_funding(202); guest.utxo.amount = amount;
                let join = setup::approve(&a, &invite, setup::prepare(&a, &invite, &lobby).unwrap(), vec![guest], &key(2), &key(2), &config).unwrap();
                check("join", &join.transaction, join.inputs());
                assert_eq!(join.transaction.outputs[..7].iter().map(|o| o.value).sum::<u64>(), GAME_POT);
                let mut fee_coin = funding(203); fee_coin.utxo.amount = amount;
                let movement = priced_transaction(&config, |fee| Ok(command_tx(&b, &s.players[0], 1, &key(1), &fee_coin, &key(3), fee)?.0)).unwrap();
                check("move", &movement, vec![s.players[0].utxo.clone(), fee_coin.utxo.clone()]);
                let prefix = priced_transaction(&config, |fee| Ok(checkpoint_tx(&b, s, 0, 110, &key(1), &fee_coin, &key(3), fee)?.0)).unwrap();
                let mut contact_entries = vec![s.court.utxo.clone(), s.players[0].utxo.clone(), s.players[2].utxo.clone(), s.players[4].utxo.clone(), fee_coin.utxo.clone()];
                check("checkpoint", &prefix, contact_entries.clone());
                let side = next_contact(s).unwrap().0;
                let contact = priced_transaction(&config, |fee| Ok(resolve_tx(&b, s, &key(side as u8 + 1), &fee_coin, &key(3), fee, false)?.0)).unwrap();
                contact_entries = vec![s.court.utxo.clone(), s.players[side].utxo.clone(), s.players[side+2].utxo.clone(), s.players[side+4].utxo.clone(), fee_coin.utxo.clone()];
                check("contact", &contact, contact_entries);
                let mut ended = s.clone(); ended.court.state.left_score = WIN_SCORE;
                ended.court.utxo = b.covenant_utxo("Court", ended.court.state.state(), COURT_VALUE, 102, false, ended.court.utxo.covenant_id).unwrap();
                let closing = priced_transaction(&config, |fee| settle_tx(&b, &ended, &fee_coin, &key(3), fee)).unwrap();
                check("settle", &closing, vec![ended.court.utxo.clone(), fee_coin.utxo.clone()]);
                let closed = accepted_collection(&ended, &closing, ended.court.state.result(), 500);
                let payout = priced_transaction(&config, |fee| reclaim_tx(&b, &closed, &fee_coin, &key(3), fee)).unwrap();
                let entries = std::iter::once(closed.collector.utxo.clone()).chain(closed.players.iter().map(|p| p.utxo.clone())).chain(std::iter::once(fee_coin.utxo.clone())).collect();
                check("payout", &payout, entries);
                assert_eq!(payout.outputs.len(), 2, "winner plus external fee change, no collateral output");
                assert_eq!(payout.outputs[0].value, GAME_POT);
            }
        }
    }

    mod player_ui {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn wallet_names_are_explicit_and_ambiguous_names_are_rejected() {
            let dir = tempfile::tempdir().unwrap();
            assert!(menu::named_wallet(dir.path(), "left").is_err());
            let legacy = dir.path().join("left.wallet.json");
            // These are deliberately not valid wallets: name discovery must not read keys.
            std::fs::write(&legacy, "private contents must not be read").unwrap();
            assert_eq!(menu::named_wallet(dir.path(), "left").unwrap(), legacy);
            let personal = dir.path().join("wallets/left/wallet.json");
            std::fs::create_dir_all(personal.parent().unwrap()).unwrap();
            std::fs::write(&personal, "private contents must not be read").unwrap();
            assert!(menu::named_wallet(dir.path(), "left").unwrap_err().to_string().contains("ambiguous"));
            assert!(menu::named_wallet(dir.path(), "../left").is_err());
            std::fs::remove_file(&legacy).unwrap();
            assert_eq!(menu::named_wallet(dir.path(), "left").unwrap(), personal);
        }

        #[test]
        fn named_wallet_launch_and_missing_wallet_never_load_keys_on_cancel() {
            use std::{
                io::Write,
                process::{Command, Stdio},
            };
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("left.wallet.json"), "do not load this file").unwrap();
            for (args, expected) in [
                (vec!["left"], "Host a new game"),
                (vec!["left", "host"], "Paste opponent's public key"),
                (vec!["left", "join"], "Game code"),
                (vec!["--wallet", "left.wallet.json"], "Host a new game"),
            ] {
                let mut child = Command::new(env!("CARGO_BIN_EXE_pong-node"))
                    .args(&args)
                    .current_dir(dir.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child.stdin.take().unwrap().write_all(b"q\n").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                let text = String::from_utf8_lossy(&output.stdout);
                assert!(text.contains(expected));
                let identity = String::from_utf8_lossy(&output.stderr);
                assert!(identity.contains("Selected wallet:") && identity.contains("left.wallet.json"));
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }

        #[test]
        fn missing_wallet_is_rejected_before_menu_session_defaults_environment_or_network() {
            use std::process::Command;
            let dir = tempfile::tempdir().unwrap();
            for name in ["left.wallet.json", "right.wallet.json", "left.session.json"] {
                std::fs::write(dir.path().join(name), "must not be loaded").unwrap();
            }
            for args in [
                vec![],
                vec!["host"],
                vec!["host", "--json"],
                vec!["join"],
                vec!["address"],
                vec!["run", "--invitation", "missing.json", "--state", "missing-state.json"],
            ] {
                let output = Command::new(env!("CARGO_BIN_EXE_pong-node"))
                    .args(&args)
                    .env("PONG_PLAYER_KEY", key(1).secret_key().display_secret().to_string())
                    .env("PONG_FEE_KEY", key(2).secret_key().display_secret().to_string())
                    .current_dir(dir.path())
                    .output()
                    .unwrap();
                let error = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "must require identity for {args:?}");
                assert!(error.contains("explicit wallet"), "{args:?}: {error}");
                assert!(error.contains("Usage:"));
                assert!(output.stdout.is_empty(), "must not open a game menu or print a default identity");
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "must not create or change session files");
        }

        #[test]
        fn local_join_draft_cannot_enqueue_after_lobby_is_spent() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            let draft = setup::prepare(&a, &invite, &lobby).unwrap();
            let join = approve_start(&a, &invite, draft, &key(2), 100_000).unwrap();
            let mut cache = Cache::new(&invite);
            start(&a, &invite, &mut tracker, &lobby);
            cache.tracker = tracker;
            assert!(!join.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            let Stage::Live(snapshot) = &mut cache.tracker.stage else { panic!() };
            snapshot.court.state.left_score = 3;
            assert!(!join.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            cache.tracker.stage = Stage::Reclaimed(Finish { stake_per_player: DEFAULT_STAKE, left_score: 3, right_score: 0, winner: 0, end_daa: 200 });
            assert!(!join.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
        }

        #[test]
        fn preview_moves_immediately_reconciles_acceptance_and_retains_waiting_inputs() {
            let now = Instant::now();
            let movement = network_ui::Movement { direction: -1, pressed: now };
            let mut preview = network_ui::InputPreview::default();
            let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
            let mut accepted = Vec::new();
            assert!(preview.push(movement.clone()));
            assert_eq!(preview.paddle(&world, 0, &accepted, now), (15, 1));
            assert_eq!((world.left_y, world.right_y), (16, 16), "preview must not change accepted state");
            preview.submitted(now, hash(20));
            assert_eq!(
                preview.paddle(&world, 0, &accepted, now + Duration::from_secs(6)),
                (15, 1),
                "submitted inputs remain visible while pending"
            );
            accepted.push(hash(20));
            world.left_y = 15;
            assert_eq!(
                preview.paddle(&world, 0, &accepted, now + Duration::from_secs(6)),
                (15, 0),
                "accepted movement must not be applied twice"
            );
            assert!(preview.push(movement.clone()));
            assert_eq!(preview.paddle(&world, 0, &accepted, now + Duration::from_secs(60)), (14, 1), "waiting input never expires");
            preview.discard(now);
            assert_eq!(preview.paddle(&world, 0, &accepted, now), (15, 0));
            for offset in 0..100_u64 {
                assert!(preview.push(network_ui::Movement { direction: 1, pressed: now + Duration::from_millis(offset) }));
            }
            world.right_y = PADDLE_MAX_Y;
            assert_eq!(preview.paddle(&world, 1, &accepted, now), (PADDLE_MAX_Y, 100));
            preview.clear();
            assert_eq!(preview.paddle(&world, 1, &accepted, now), (PADDLE_MAX_Y, 0), "match completion clears local predictions");
        }

        #[test]
        fn preview_changes_only_rendered_paddle_and_never_ball_or_result() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(snapshot) = &tracker.stage else { panic!() };
            let before = project(snapshot, 155).unwrap().world;
            let frame = ascii::frame_with_preview(snapshot, 155, "LOCAL PREVIEW", "PONG", "controls", "footer", Some((0, 8))).unwrap();
            assert!(frame.contains("y=16/16"), "white accepted paddle must not be replaced by local intent");
            assert!(frame.contains(['▓', '▴', '▾']));
            assert_eq!(project(snapshot, 155).unwrap().world, before);
            let ball_cells = |frame: &str| {
                frame
                    .lines()
                    .enumerate()
                    .flat_map(|(y, line)| {
                        line.chars().enumerate().filter_map(move |(x, ch)| ('\u{2801}'..='\u{28ff}').contains(&ch).then_some((x, y, ch)))
                    })
                    .collect::<Vec<_>>()
            };
            let accepted = ascii::frame_with_labels(snapshot, 155, "", "PONG", "controls", "footer").unwrap();
            assert!(!ball_cells(&frame).is_empty());
            assert_eq!(ball_cells(&frame), ball_cells(&accepted));
        }
    }

    #[test]
    fn local_guest_join_draft_is_public_and_validates_the_invited_signer() {
        let a = artifact();
        let (invite, _, lobby) = fixture(&a);
        let p = setup::prepare(&a, &invite, &lobby).unwrap();
        setup::validate(&a, &invite, &p, 100_000).unwrap();
        assert!(p.funding.is_none());
        let encoded = serde_json::to_string(&p).unwrap();
        for n in 1..=3 {
            assert!(!encoded.contains(&key(n).secret_key().display_secret().to_string()));
        }
        let p: setup::StartPackage = serde_json::from_str(&encoded).unwrap();
        assert!(approve_start(&a, &invite, p.clone(), &key(1), 100_000).is_err());
        let mut joined = approve_start(&a, &invite, p, &key(2), 100_000).unwrap();
        setup::validate(&a, &invite, &joined, 100_000).unwrap();
        joined.transaction.outputs[0].value += 1;
        assert!(setup::validate(&a, &invite, &joined, 100_000).is_err());
    }

    mod wager_setup {
        use super::*;
        use kaspa_consensus_core::{hashing::sighash_type::{SIG_HASH_SINGLE, SIG_HASH_ANY_ONE_CAN_PAY}, tx::MutableTransaction};

        fn launch_witness(a: &argent_runtime::Artifact, p: &mut setup::StartPackage) {
            let mut args = vec![argent_runtime::ArtifactValue::Bytes(p.right_signature.clone())];
            for actor in ["Player", "Court"] {
                let contract = a.sil_abi.contract(actor).unwrap();
                let (prefix, _, suffix) = contract.compiled.script_parts(&contract.compiled.bytecode).unwrap();
                args.push(argent_runtime::ArtifactValue::Bytes(prefix.to_vec()));
                args.push(argent_runtime::ArtifactValue::Bytes(suffix.to_vec()));
            }
            let encoded = argent::artifact::encode_contract_entry_sig_script(&a.sil_abi, "Lobby", "start", &args).unwrap();
            p.transaction.inputs[0].signature_script = kaspa_txscript::pay_to_script_hash_signature_script_with_flags(
                redeem(&TxBuilder::new(a).unwrap(), a, "Lobby", p.lobby.state.state()).unwrap(), encoded,
                argent_runtime::covenant_engine_flags()).unwrap();
        }
        fn mutable(p: &setup::StartPackage) -> MutableTransaction<kaspa_consensus_core::tx::Transaction> {
            let mut m = MutableTransaction::new(p.transaction.clone());
            m.entries = p.inputs().into_iter().map(Some).collect(); m
        }

        #[test]
        fn proposal_is_explicitly_unfunded_and_guest_contributes_only_its_stake_and_fee() {
            let a = artifact(); let (invite, tracker, lobby) = fixture(&a);
            assert_eq!(DEFAULT_STAKE, 500_000_000, "fixed 5 KAS per player");
            assert_eq!(2 * DEFAULT_STAKE, 1_000_000_000, "10 KAS winner pot");
            assert_eq!(GAME_POT, 1_000_000_000, "only the two stakes fund the game");
            assert_eq!(node::client::transaction_fee(&invite.genesis, &[funding(90).utxo]).unwrap(), 100_000);
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            setup::validate(&a, &invite, &proposal, 100_000).unwrap();
            assert!(!proposal.complete()); assert!(proposal.funding.is_none());
            assert_eq!(proposal.transaction.inputs.len(), 1);
            assert_eq!(proposal.transaction.storage_mass(), 0);
            assert_eq!(proposal.lobby.utxo.amount, 500_000_000, "host locks only its 5 KAS stake");
            let mut cache = Cache::new(&invite); cache.tracker = tracker;
            assert!(proposal.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).is_err()); assert!(cache.pending.is_empty());
            let mut fake = proposal.clone(); fake.fee = 1;
            assert!(setup::validate(&a, &invite, &fake, 100_000).is_err());
            let mut guest = guest_funding(91); guest.utxo.amount = 600_000_000;
            let config = node::Config { fee_sompi: 100_000, relay_fee_per_kg: 1, ..Default::default() };
            let complete = setup::approve(&a, &invite, proposal, vec![guest], &key(2), &key(2), &config).unwrap();
            let coin = complete.funding.as_ref().unwrap();
            assert!(coin.utxo.amount < 1_000_000_000, "guest does not need the old 10 KAS stake");
            assert_eq!(coin.utxo.script_public_key, p2pk(&pubkey(&key(2))));
            assert_eq!(complete.transaction.outputs[0].value, 400_000_000, "Court holds 4 KAS of the pot");
            assert_eq!(complete.transaction.outputs[1..7].iter().map(|o| o.value).collect::<Vec<_>>(), vec![100_000_000; 6]);
            assert_eq!(complete.transaction.outputs[..7].iter().map(|o| o.value).sum::<u64>(), 1_000_000_000);
            assert_eq!(complete.transaction.storage_mass(), storage_mass(&complete.transaction, complete.inputs()).unwrap());
            assert!(complete.transaction.storage_mass() <= 500_000);
            assert_eq!(complete.transaction.outputs[7].script_public_key, coin.utxo.script_public_key);
            assert_eq!(coin.utxo.amount - complete.transaction.outputs[7].value, 500_000_000 + complete.fee);
            let payment = node::fees::Payment::from_transaction(&complete.transaction, &complete.inputs()).unwrap().unwrap();
            assert_eq!(payment.sompi, complete.fee); assert_eq!(payment.payer, coin.utxo.script_public_key);
            assert!(complete.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            cache.validate(&a, &invite).unwrap();
        }

        #[test]
        fn valid_signatures_with_wrong_flags_and_reordered_or_changed_outputs_fail() {
            let a = artifact(); let (invite, _, lobby) = fixture(&a);
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            let good = approve_start(&a, &invite, proposal, &key(2), 100_000).unwrap();
            for right_mode in [SIG_HASH_SINGLE, SIG_HASH_SINGLE | SIG_HASH_ANY_ONE_CAN_PAY] {
                let mut p = good.clone();
                p.right_signature = sign_with_type(&mutable(&p), 0, &key(2), right_mode);
                launch_witness(&a, &mut p);
                assert!(setup::validate(&a, &invite, &p, 100_000).is_err());
                assert!(argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), 0).is_err(), "VM must reject correctly signed unauthorized flags");
            }
            for index in [0, 1, 6, 7] {
                let mut p = good.clone(); p.transaction.outputs[index].value -= 1;
                p.right_signature = sign(&mutable(&p), 0, &key(2)); launch_witness(&a, &mut p);
                assert!(setup::validate(&a, &invite, &p, 100_000).is_err());
                if index < 7 { assert!(argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), 0).is_err()); }
            }
            let mut p = good.clone(); p.transaction.outputs.swap(0, 1);
            p.right_signature = sign(&mutable(&p), 0, &key(2)); launch_witness(&a, &mut p);
            assert!(setup::validate(&a, &invite, &p, 100_000).is_err());
            assert!(argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), 0).is_err());
            let mut p = good; p.transaction.inputs.swap(0, 1);
            assert!(setup::validate(&a, &invite, &p, 100_000).is_err());
            let mut entries = p.inputs(); entries.swap(0, 1);
            assert!(argent_runtime::execute_input_with_covenants(&p.transaction, entries, 1).is_err());
        }

        #[test]
        fn fixed_client_stake_and_owned_funding_are_required() {
            let a = artifact(); let (invite, _, lobby) = fixture(&a);
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            let config = node::Config::default();
            assert!(setup::approve(&a, &invite, proposal.clone(), vec![funding(91)], &key(2), &key(2), &config).is_err());
            let mut small = guest_funding(91); small.utxo.amount = DEFAULT_STAKE as u64;
            assert!(setup::approve(&a, &invite, proposal, vec![small], &key(2), &key(2), &config).is_err());
            for stake in [0, -1, DEFAULT_STAKE + 1, 1_000_000_000, MAX_STAKE + 1] {
                let mut changed = invite.clone(); changed.lobby.stake_per_player = stake;
                assert!(changed.validate(&a).is_err());
            }
            // The unchanged covenant still permits 10 KAS, but this client rejects
            // even a correctly signed offer using that old stake at today's version.
            let b = TxBuilder::new(&a).unwrap();
            let mut old_invite = invite.clone(); old_invite.lobby.stake_per_player = 1_000_000_000;
            old_invite.genesis = genesis_tx(&b, &old_invite.lobby, &funding(90), &key(3), 100_000).unwrap();
            let old_lobby = accepted_cell(&old_invite.genesis, 0, 100, old_invite.lobby.clone());
            let guest = guest_funding(91);
            let (transaction, _) = start_tx(&b, &old_lobby, &key(2), &guest, &key(2), 100_000).unwrap();
            let mut old_package = setup::StartPackage {
                version: GAME_VERSION, game: old_invite.genesis.id(), lobby: old_lobby, funding: Some(guest), fee: 100_000,
                transaction, right_signature: vec![], fee_signature: vec![],
            };
            old_package.right_signature = sign(&mutable(&old_package), 0, &key(2));
            old_package.fee_signature = sign(&mutable(&old_package), 1, &key(2));
            launch_witness(&a, &mut old_package);
            argent_runtime::execute_input_with_covenants(&old_package.transaction, old_package.inputs(), 0).unwrap();
            argent_runtime::execute_input_with_covenants(&old_package.transaction, old_package.inputs(), 1).unwrap();
            assert!(old_invite.validate(&a).is_err());
            assert!(setup::validate(&a, &old_invite, &old_package, 100_000).is_err());
            assert!(stake_amount(MAX_STAKE).is_ok()); assert!(stake_amount(MAX_STAKE + 1).is_err());
            let player_coin = guest_funding(91);
            assert_eq!(pubkey(node::wallet::funding_key(&player_coin, &key(2), &key(5)).unwrap()), pubkey(&key(2)));
            assert!(node::wallet::funding_key(&player_coin, &key(1), &key(5)).is_err());
            let mut ledger = node::fees::Ledger::new();
            ledger.payments.insert(hash(99), node::fees::Payment { sompi: 123, payer: player_coin.utxo.script_public_key.clone() });
            assert_eq!(ledger.paid_by_wallet(&p2pk(&pubkey(&key(2))), &p2pk(&pubkey(&key(2)))), 123);
            assert_eq!(ledger.paid_by_wallet(&p2pk(&pubkey(&key(2))), &p2pk(&pubkey(&key(5)))), 123);
        }
    }

    mod ui_continuity {
        use super::*;
        use std::time::{Duration, Instant};

        fn display(stage: Stage, now: Instant) -> network_ui::Display {
            network_ui::Display {
                stage,
                daa: 160,
                ready: true,
                observed: now,
                clock_epoch: 3,
                pending: 1,
                accepted_previews: Vec::new(),
                queued: 1,
                spending: None,
                reorgs: 0,
                status: String::new(),
                last_reorg: None,
                recent_transactions: Default::default(),
                lobby_hint: None,
            }
        }

        #[test]
        fn incomplete_replay_keeps_coherent_court_and_fresh_observed_clock() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let now = Instant::now();
            let coherent = display(tracker.stage, now);
            let mut replay = coherent.clone();
            replay.ready = false;
            replay.stage = Stage::AwaitingLobby;
            replay.daa = 100;
            let drawing = network_ui::court_display(&replay, Some(&coherent));
            assert!(matches!(drawing.stage, Stage::Live(_)), "partial replay must not replace the court with a lobby");
            assert!(!replay.ready, "drawing continuity must not enable spending or inputs");
            let sample = network_ui::ClockSample { epoch: 3, daa: 165, observed: now };
            assert!(network_ui::clock_matches(drawing, &sample, now));
            let mut clock = network_ui::ObservedClock::default();
            clock.update(drawing.daa, 102, false, now);
            clock.update(sample.daa, 102, false, now);
            assert!(clock.sample(now + Duration::from_millis(200)) > drawing.daa - network_ui::PLAYBACK_INITIAL_BUFFER_DAA);
            assert!(!network_ui::clock_matches(drawing, &sample, now + Duration::from_secs(11)), "stale state must stop following time");
            replay.clock_epoch += 1;
            assert!(
                matches!(network_ui::court_display(&replay, Some(&coherent)).stage, Stage::AwaitingLobby),
                "real rollback or disconnect invalidates old visuals"
            );
            replay.ready = true;
            assert!(std::ptr::eq(network_ui::court_display(&replay, Some(&coherent)), &replay));
        }

        #[test]
        fn final_board_holds_two_seconds_across_settlement_updates_and_can_reset() {
            let now = Instant::now();
            let mut board = network_ui::FinalBoard::default();
            assert_eq!(board.show(Some("live court".into()), false, now), Some("live court"));
            assert_eq!(board.show(Some("winning court".into()), true, now), Some("winning court"));
            // Finished/refunded stage updates have no live actors and must not
            // restart the timer or remove the court before the hold expires.
            assert_eq!(board.show(None, true, now + Duration::from_secs(1)), Some("winning court"));
            assert_eq!(board.show(None, true, now + Duration::from_secs(2)), None);
            assert_eq!(board.show(None, true, now + Duration::from_secs(3)), None);
            assert_eq!(board.show(Some("restored court".into()), false, now + Duration::from_secs(3)), Some("restored court"));
            assert_eq!(board.show(None, true, now + Duration::from_secs(4)), Some("restored court"));
            let mut resumed = network_ui::FinalBoard::default();
            assert_eq!(resumed.show(None, true, now), None, "resuming an already finished game needs no artificial delay");
        }

        #[test]
        fn winning_court_retains_final_score_before_detailed_results() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(snapshot) = &mut tracker.stage else { panic!() };
            snapshot.court.state.left_score = 3;
            let court =
                ascii::court_frame(snapshot, 160, "Projected result; finalization pending", "PONG", "q quits", "footer", None).unwrap();
            assert!(court.contains("LEFT  3 :  0 RIGHT"));
            assert!(court.contains(&format!("+{}+", "-".repeat(COURT_WIDTH as usize))));
            assert!(court.contains("finalization pending"));
            assert!(ascii::frame(snapshot, 160, "").unwrap().contains("GAME COMPLETED"), "existing result rendering remains available");
        }
    }

    mod public_play_regressions {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn clock_buffers_jitter_without_predicting_chain_progress_or_jumping() {
            let now = Instant::now();
            let mut clock = network_ui::ObservedClock::default();
            clock.update(120, 100, false, now);
            assert_eq!(clock.sample(now), 115);
            assert_eq!(clock.sample(now + Duration::from_millis(500)), 120);
            clock.update(140, 100, false, now + Duration::from_secs(1));
            assert_eq!(clock.sample(now + Duration::from_secs(1)), 120, "bursts must not teleport the ball");
            assert_eq!(clock.sample(now + Duration::from_secs(3)), 140);
            assert_eq!(clock.sample(now + Duration::from_secs(20)), 140, "never invent unobserved time");
            clock.update(1000, 100, false, now + Duration::from_secs(20));
            assert_eq!(clock.sample(now + Duration::from_secs(20)), 140);
            assert_eq!(clock.sample(now + Duration::from_secs(21)), 160, "large delays fast-forward at no more than 2x");
            clock.update(115, 102, true, now + Duration::from_secs(22));
            assert_eq!(clock.sample(now + Duration::from_secs(22)), 110);
        }

        #[test]
        fn movement_delivery_retries_lost_transactions_once_and_never_duplicates_acceptance() {
            let now = Instant::now();
            let first = network_ui::Movement { direction: -1, pressed: now - Duration::from_secs(60) };
            let second = network_ui::Movement { direction: 1, pressed: now };
            let mut delivery = network_ui::MovementDelivery::default();
            delivery.submitted(first.clone(), hash(40));
            delivery.submitted(second.clone(), hash(41));
            assert!(delivery.reconcile(&[], &[hash(40), hash(41)]).is_empty(), "pending moves must not be duplicated");
            let retry = delivery.reconcile(&[hash(40)], &[]);
            assert_eq!(retry.len(), 1, "only the unaccepted lost transaction needs retry");
            assert_eq!(retry[0].pressed, second.pressed);
            assert!(delivery.reconcile(&[], &[]).is_empty(), "a loss is returned only once");
            delivery.submitted(retry[0].clone(), hash(42));
            assert!(delivery.reconcile(&[hash(42)], &[]).is_empty(), "accepted retry is never resent");
            assert!(delivery.reorged(&[hash(99)]).is_empty(), "unrelated reorg cannot replay moves");
            assert_eq!(delivery.reorged(&[hash(42)]).len(), 1);
            assert!(delivery.reorged(&[hash(42)]).is_empty(), "same rollback is recovered once");
            let rolled_back = delivery.reconcile(&[], &[]);
            assert_eq!(rolled_back.len(), 1, "an accepted move removed by a game reorg returns to the queue");
            assert_eq!(rolled_back[0].pressed, second.pressed);

            let mut preview = network_ui::InputPreview::default();
            preview.push(first.clone());
            preview.submitted(first.pressed, hash(40));
            preview.requeue(&first);
            let world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
            assert_eq!(preview.paddle(&world, 0, &[hash(40)], now), (15, 1), "retry target stays visible despite old tx id");
            preview.submitted(first.pressed, hash(43));
            let mut accepted_world = world.clone();
            accepted_world.left_y = 15;
            assert_eq!(preview.paddle(&accepted_world, 0, &[hash(43)], now), (15, 0));
            preview.requeue(&first);
            assert_eq!(preview.paddle(&world, 0, &[], now), (15, 1), "reorg retry restores an acknowledged target");
        }

        #[test]
        fn countdown_preserves_errors_and_ball_is_prominent_after_serve() {
            let a = artifact();
            let (i, mut t, lobby) = fixture(&a);
            start(&a, &i, &mut t, &lobby);
            let Stage::Live(s) = &t.stage else { panic!() };
            let frame = ascii::frame(s, 102, "Spending paused: insufficient funds").unwrap();
            assert!(frame.contains("Game will start in"));
            assert!(frame.contains("Spending paused: insufficient funds"));
            assert_eq!(frame.lines().count(), 24);
            let opening = project(s, 102).unwrap().world;
            let court_rows: Vec<_> = frame.lines().filter(|line| line.starts_with('|')).collect();
            assert!(
                court_rows[(opening.ball_y / 2) as usize]
                    .chars()
                    .nth(opening.ball_x as usize + 1)
                    .is_some_and(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch))
            );
            assert!(ascii::frame(s, 160, "Following accepted chain").unwrap().chars().any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch)));
        }

        #[test]
        fn unrelated_tip_reorg_preserves_input_but_game_rollback_invalidates_it() {
            let a = artifact();
            let (i, mut t, lobby) = fixture(&a);
            start(&a, &i, &mut t, &lobby);
            t.apply(&a, &i, &[], &[ChainBlock { hash: hash(12), daa: 104, transactions: vec![] }]).unwrap();
            let unrelated = node::audit::ReorgEvent::capture(&i, &t, &[hash(12)], &[], Some(hash(11)));
            assert!(!unrelated.affects_game());
            let game = node::audit::ReorgEvent::capture(&i, &t, &[hash(12), hash(11)], &[], Some(hash(10)));
            assert!(game.affects_game());
            let unknown = node::audit::ReorgEvent::capture(&i, &t, &[hash(99)], &[], None);
            assert!(!unknown.affects_game(), "unknown block history alone is not evidence of a game reorg");
        }

        #[tokio::test]
        #[ignore = "read-only live network test; set PONG_PUBLIC_INVITATION to a public game invitation"]
        async fn moving_public_tip_allows_coherent_game_tracking() {
            let path = std::env::var("PONG_PUBLIC_INVITATION").expect("public invitation path required");
            let invite: Invitation = node::load(std::path::Path::new(&path)).unwrap();
            let a = artifact();
            invite.validate(&a).unwrap();
            let config = node::Config { endpoint: "resolver".into(), network: invite.network.clone(), ..Default::default() };
            let n = node::rpc::Node::connect(&config).await.unwrap();
            let mut cache = Cache::new(&invite);
            let mut ready = 0;
            let mut scores = Vec::new();
            for _ in 0..5 {
                let result = cache.sync(&a, &invite, &n, &config).await.unwrap();
                if result.0 {
                    ready += 1;
                    scores.push(cache.tracker.daa);
                    // The check must still reject incorrect acceptance metadata.
                    if let Stage::Live(s) = &cache.tracker.stage {
                        let mut bad = s.clone();
                        bad.players[0].utxo.block_daa_score += 1;
                        assert!(!n.consistent(&Stage::Live(bad)).await.unwrap());
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            println!("read-only public tracking: {ready}/5 ready; observed DAA {scores:?}; endpoint {}", n.current_endpoint());
            assert!(ready >= 3, "a moving global tip must not indefinitely disable game input");
            assert!(scores.last() > scores.first(), "the chain must advance during the test");
            n.rpc.disconnect().await.unwrap();
        }
    }
    fn movement(a: &argent_runtime::Artifact, t: &Tracker, direction: i64, fee: u8) -> AcceptedTx {
        let Stage::Live(s) = &t.stage else { panic!() };
        let f = funding(fee);
        let (tx, _) = command_tx(&TxBuilder::new(a).unwrap(), &s.players[0], direction, &key(1), &f, &key(3), 100_000).unwrap();
        AcceptedTx { tx, inputs: vec![s.players[0].utxo.clone(), f.utxo] }
    }
    fn resolution(a: &argent_runtime::Artifact, t: &Tracker, fee: u8) -> AcceptedTx {
        let Stage::Live(s) = &t.stage else { panic!() };
        let f = funding(fee);
        let until = s.players.iter().filter(|p| p.state.side == 0).map(|p| p.utxo.block_daa_score as i64).max().unwrap() + 1;
        let (tx, _) = checkpoint_tx(&TxBuilder::new(a).unwrap(), s, 0, until, &key(1), &f, &key(3), 100_000).unwrap();
        AcceptedTx {
            tx,
            inputs: vec![s.court.utxo.clone(), s.players[0].utxo.clone(), s.players[2].utxo.clone(), s.players[4].utxo.clone(), f.utxo],
        }
    }

    mod goal_response_timeout {
        use super::*;

        #[test]
        fn receiver_response_and_opponent_timeout_have_distinct_duties() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let (receiver, deadline) = next_contact(s).unwrap();
            let opponent = (1 - receiver) as i64;
            let expiry = timeout_result(s).unwrap().end_daa as u64;
            assert_eq!(expiry, deadline as u64 + RESPONSE_TIMEOUT_DAA);
            assert_eq!(action(&tracker.stage, receiver as i64, deadline as u64, None).unwrap(), Some(Action::Resolve(false)));
            assert_eq!(action(&tracker.stage, opponent, expiry - 1, None).unwrap(), None);
            assert_eq!(action(&tracker.stage, opponent, expiry, None).unwrap(), Some(Action::Timeout));
            assert_eq!(action(&tracker.stage, receiver as i64, expiry, None).unwrap(), Some(Action::Resolve(false)));
        }

        #[test]
        fn timeout_late_command_refund_and_reorg_replay_exact_winner_and_metadata() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let before = tracker.stage.clone();
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let b = TxBuilder::new(&a).unwrap();
            let result = timeout_result(s).unwrap();
            let expiry = result.end_daa as u64;
            let f = funding(92);
            let (tx, _) = timeout_tx(&b, s, &key(result.winner as u8 + 1), &f, &key(3), 100_000).unwrap();
            let command_funding = funding(93);
            let (command, _) = command_tx(&b, &s.players[0], 1, &key(1), &command_funding, &key(3), 100_000).unwrap();
            let command_inputs = vec![s.players[0].utxo.clone(), command_funding.utxo];
            let timeout_inputs = vec![s.court.utxo.clone(), f.utxo];
            let events = tracker.apply_with_events(&a, &invite, &[], &[ChainBlock {
                hash: hash(12), daa: expiry + 3, transactions: vec![AcceptedTx { tx, inputs: timeout_inputs }],
            }]).unwrap();
            assert_eq!(events[0].event.action, "timeout");
            assert_eq!(events[0].event.side, Some(result.winner));
            assert_eq!(events[0].event.accepted_daa, Some(expiry + 3));
            let Stage::Collecting(c) = &tracker.stage else { panic!() };
            assert_eq!(c.collector.state.finish, result);
            assert_eq!(c.collector.utxo.block_daa_score, expiry + 3);
            tracker.apply(&a, &invite, &[], &[ChainBlock {
                hash: hash(13), daa: expiry + 4, transactions: vec![AcceptedTx { tx: command, inputs: command_inputs }],
            }]).unwrap();
            let Stage::Collecting(c) = &tracker.stage else { panic!() };
            assert_eq!(c.players[0].utxo.block_daa_score, expiry + 4);
            let f = funding(94);
            let refund = reclaim_tx(&b, c, &f, &key(3), 100_000).unwrap();
            assert_eq!(refund.outputs[0].value, GAME_POT);
            let inputs = std::iter::once(c.collector.utxo.clone()).chain(c.players.iter().map(|p| p.utxo.clone()))
                .chain(std::iter::once(f.utxo)).collect();
            tracker.apply(&a, &invite, &[], &[ChainBlock {
                hash: hash(14), daa: expiry + 5, transactions: vec![AcceptedTx { tx: refund, inputs }],
            }]).unwrap();
            let Stage::Reclaimed(finish) = &tracker.stage else { panic!() };
            assert_eq!(finish, &result);
            tracker.apply(&a, &invite, &[hash(14), hash(13), hash(12)], &[]).unwrap();
            let (Stage::Live(restored), Stage::Live(original)) = (&tracker.stage, before) else { panic!() };
            assert_eq!(restored.court.outpoint, original.court.outpoint);
            assert_eq!(restored.court.utxo.block_daa_score, original.court.utxo.block_daa_score);
            assert_eq!(timeout_result(restored).unwrap(), result);
            assert_eq!(restored.players[0].outpoint, original.players[0].outpoint);
        }
    }

    mod concurrent_pipelines {
        use super::*;
        use node::audit::{GameEvent, TransactionRecord, TransactionStatus};
        use std::time::Duration;

        fn live() -> (argent_runtime::Artifact, Invitation, Cache) {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let mut cache = Cache::new(&invite);
            cache.tracker = tracker;
            (a, invite, cache)
        }

        #[test]
        fn fresh_withdrawal_retains_reservations_until_durable_and_recovers_after_crash() {
            let (a, invite, mut cache) = live();
            let command = movement(&a, &cache.tracker, 1, 92);
            let fresh = cache.authorize_unsubmitted(command.tx.clone(), command.inputs.clone(), "move".into(), &node::Config::default()).unwrap();
            let charged = cache.authorized_fees;
            assert!(cache.complete_withdrawal(fresh.id()).is_err());
            cache.begin_withdrawal(&fresh).unwrap();
            assert_eq!(cache.pending.len(), 1);
            assert_eq!(cache.authorized_fees, charged);
            assert_eq!(cache.available_lane(0), Some(2));
            assert!(cache.next_action(0, 102, Some(1)).unwrap().is_none());
            let validating = Cache::new(&invite);
            cache.publish_validated(validating, false);
            assert_eq!(cache.withdrawals, vec![fresh.id()]);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let mut writer = node::pipeline::Persistence::new(path.clone());
            writer.snapshot(&cache, true).unwrap().unwrap().recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            writer.finish().unwrap();
            let mut recovered: Cache = node::load(&path).unwrap();
            recovered.recover_withdrawals().unwrap();
            assert!(recovered.pending.is_empty());
            assert!(recovered.withdrawals.is_empty());
            assert_eq!(recovered.authorized_fees, 0);
            assert!(cache.mark_exposed(fresh).is_err(), "revoked drafts must never reach RPC");
        }

        #[test]
        fn failed_withdrawal_save_keeps_fee_and_lane_reserved() {
            let (a, _, mut cache) = live();
            let command = movement(&a, &cache.tracker, 1, 92);
            let fresh = cache.authorize_unsubmitted(command.tx, command.inputs, "move".into(), &node::Config::default()).unwrap();
            cache.begin_withdrawal(&fresh).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("directory-not-a-file");
            std::fs::create_dir(&path).unwrap();
            let mut writer = node::pipeline::Persistence::new(path);
            assert!(writer.snapshot(&cache, true).unwrap().unwrap().recv_timeout(Duration::from_secs(10)).unwrap().is_err());
            assert_eq!(cache.pending.len(), 1);
            assert_eq!(cache.withdrawals, vec![fresh.id()]);
            assert!(cache.authorized_fees > 0);
            assert!(writer.finish().is_err());
        }

        #[test]
        fn exposed_or_recovered_drafts_cannot_be_withdrawn_as_fresh() {
            let (a, _, mut cache) = live();
            let command = movement(&a, &cache.tracker, 1, 92);
            let fresh = cache.authorize_unsubmitted(command.tx, command.inputs, "move".into(), &node::Config::default()).unwrap();
            let encoded = serde_json::to_vec(&cache).unwrap();
            let mut recovered: Cache = serde_json::from_slice(&encoded).unwrap();
            assert!(recovered.begin_withdrawal(&fresh).is_err(), "saved pending does not recreate runtime proof");
            recovered.recover_withdrawals().unwrap();
            assert_eq!(recovered.pending.len(), 1);
            assert!(cache.authorize_unsubmitted(cache.pending[0].transaction.clone(), cache.pending[0].inputs.clone(), "move".into(), &node::Config::default()).is_err());
            cache.mark_exposed(fresh).unwrap();
            assert!(cache.withdrawals.is_empty());
            assert_eq!(cache.pending.len(), 1);
        }

        #[test]
        fn losing_fee_parent_cannot_release_a_fresh_child_without_withdrawal() {
            let (a, _, mut cache) = live();
            let parent = movement(&a, &cache.tracker, 1, 92);
            cache.authorize(parent.tx.clone(), parent.inputs, "move".into(), &node::Config::default()).unwrap();
            cache.pending[0].attempted_ms = 1;
            let index = parent.tx.outputs.iter().position(|o| o.covenant.is_none()).unwrap();
            let change = &parent.tx.outputs[index];
            let fee = Funding { outpoint: TransactionOutpoint::new(parent.tx.id(), index as u32),
                utxo: UtxoEntry::new(change.value, change.script_public_key.clone(), u64::MAX, false, None) };
            let Stage::Live(s) = &cache.tracker.stage else { panic!() };
            let (child, _) = command_tx(&TxBuilder::new(&a).unwrap(), &s.players[2], 2, &key(1), &fee, &key(3), 100_000).unwrap();
            let entries = vec![s.players[2].utxo.clone(), fee.utxo];
            let fresh = cache.authorize_unsubmitted(child.clone(), entries, "move".into(), &node::Config::default()).unwrap();
            cache.discard_pending(parent.tx.id());
            assert_eq!(cache.pending.len(), 2);
            assert!(cache.pending.iter().all(|p| p.branch_changed));
            cache.begin_withdrawal(&fresh).unwrap();
            // Simulated successful durability acknowledgement; recovery above tests real I/O.
            cache.complete_withdrawal(fresh.id()).unwrap();
            cache.discard_pending(parent.tx.id());
            assert!(cache.pending.is_empty());
            assert_eq!(cache.authorized_fees, 100_000, "only the never-exposed child fee is released");
        }

        #[test]
        fn validation_publication_preserves_new_authorizations_and_tip_ticks_do_not_stale_lanes() {
            let (a, invite, mut cache) = live();
            let mut validating = cache.clone();
            let command = movement(&a, &cache.tracker, 1, 92);
            cache.authorize(command.tx.clone(), command.inputs.clone(), "move".into(), &node::Config::default()).unwrap();
            let authorized = cache.authorized_fees;
            validating.reorg_log_pending.push(node::audit::ReorgEvent::capture(&invite, &validating.tracker, &[], &[], None));
            validating.tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 110, transactions: vec![] }]).unwrap();
            cache.publish_validated(validating, false);
            assert_eq!(cache.authorized_fees, authorized);
            assert_eq!(cache.pending.len(), 1);
            assert!(!cache.pending[0].branch_changed);
            assert_eq!(cache.reorg_log_pending[0].pending_local_transaction, Some(command.tx.id()));
            assert!(cache.draft_current(&command.tx, &command.inputs), "unrelated tip changes must not starve movement");
            let mut reorganization = cache.clone();
            reorganization.last_reorg = reorganization.reorg_log_pending.first().cloned();
            reorganization.last_reorg.as_mut().unwrap().pending_local_transaction = None;
            reorganization.reorg_log_pending.clear();
            cache.publish_validated(reorganization, true);
            assert_eq!(cache.last_reorg.as_ref().unwrap().pending_local_transaction, Some(command.tx.id()));
            let mut later_publication = cache.clone();
            later_publication.last_reorg.as_mut().unwrap().pending_local_transaction = None;
            cache.pending.clear();
            let later = movement(&a, &cache.tracker, 2, 93);
            cache.authorize(later.tx, later.inputs, "move".into(), &node::Config::default()).unwrap();
            cache.publish_validated(later_publication, false);
            assert_eq!(
                cache.last_reorg.as_ref().unwrap().pending_local_transaction,
                Some(command.tx.id()),
                "later snapshots retain old evidence, not the new pending ID"
            );
            let Stage::Live(s) = &mut cache.tracker.stage else { panic!() };
            s.players[0].utxo.block_daa_score += 1;
            assert!(!cache.draft_current(&command.tx, &command.inputs), "acceptance metadata changes do stale the draft");
        }

        #[test]
        fn reservations_keep_fee_change_dependencies_and_exclusive_actions_safe() {
            let (a, _invite, mut cache) = live();
            let parent = movement(&a, &cache.tracker, 1, 92);
            cache.authorize(parent.tx.clone(), parent.inputs.clone(), "move".into(), &node::Config::default()).unwrap();
            cache.pending[0].attempted_ms = 1;
            let index = parent.tx.outputs.iter().position(|o| o.covenant.is_none()).unwrap();
            let change = &parent.tx.outputs[index];
            let fee = Funding {
                outpoint: TransactionOutpoint::new(parent.tx.id(), index as u32),
                utxo: UtxoEntry::new(change.value, change.script_public_key.clone(), u64::MAX, false, None),
            };
            let Stage::Live(s) = &cache.tracker.stage else { panic!() };
            let (child, _) = command_tx(&TxBuilder::new(&a).unwrap(), &s.players[2], 2, &key(1), &fee, &key(3), 100_000).unwrap();
            let entries = vec![s.players[2].utxo.clone(), fee.utxo.clone()];
            let (racing, _) = command_tx(&TxBuilder::new(&a).unwrap(), &s.players[4], 1, &key(1), &fee, &key(3), 100_000).unwrap();
            let racing_entries = vec![s.players[4].utxo.clone(), fee.utxo];
            assert!(cache.draft_current(&child, &entries));
            cache.authorize(child.clone(), entries.clone(), "move".into(), &node::Config::default()).unwrap();
            assert!(cache.authorize(racing, racing_entries, "move".into(), &node::Config::default()).is_err());
            let resolution = resolution(&a, &cache.tracker, 93);
            assert!(cache.authorize(resolution.tx, resolution.inputs, "contact".into(), &node::Config::default()).is_err());
            cache.discard_pending(parent.tx.id());
            assert!(cache.pending.is_empty());
            assert!(!cache.draft_current(&child, &entries), "a lost fee parent invalidates the child draft");
        }

        #[test]
        fn coalesced_snapshots_cannot_overwrite_durable_authorizations_and_logs_drain_once() {
            let (a, invite, mut cache) = live();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let event = GameEvent {
                txid: hash(70),
                accepted_daa: Some(105),
                action: "move".into(),
                side: Some(0),
                direction: Some(1),
                payment: None,
            };
            cache.transaction_log_pending.push(TransactionRecord::new(&invite, event, Some(hash(12)), 0, TransactionStatus::Accepted));
            let mut writer = node::pipeline::Persistence::new(path.clone());
            for _ in 0..64 {
                writer.snapshot(&cache, false).unwrap();
            }
            let command = movement(&a, &cache.tracker, 1, 92);
            cache.authorize(command.tx, command.inputs, "move".into(), &node::Config::default()).unwrap();
            let wait = writer.snapshot(&cache, true).unwrap().unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            writer.timing(node::pipeline::Timing::new("test_shutdown_drain", None, None));
            writer.snapshot(&cache, false).unwrap();
            writer.finish().unwrap();
            let saved: Cache = node::load(&path).unwrap();
            assert_eq!(saved.pending.len(), 1);
            assert_eq!(saved.authorized_fees, cache.authorized_fees);
            saved.validate(&a, &invite).unwrap();
            let records = std::fs::read_to_string(node::audit::transaction_log_path(&path)).unwrap();
            assert_eq!(records.lines().count(), 1, "coalescing and retried snapshots must not duplicate records");
            let timing = std::fs::read_to_string(path.with_extension("timing.jsonl")).unwrap();
            assert!(timing.contains("test_shutdown_drain"));
            assert!(timing.contains("\"node_acceptance_unix_ms\":null"));
        }

        #[test]
        fn diagnostic_failure_is_reported_without_acknowledging_a_failed_required_save() {
            let (_, _, cache) = live();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            std::fs::create_dir(path.with_extension("timing.jsonl")).unwrap();
            let mut writer = node::pipeline::Persistence::new(path.clone());
            let wait = writer.snapshot(&cache, true).unwrap().unwrap();
            assert!(
                wait.recv_timeout(Duration::from_secs(10)).unwrap().is_ok(),
                "cache durability is distinct from diagnostic availability"
            );
            assert!(writer.finish().is_err(), "diagnostic failure must not be silent");
            assert!(path.exists());

            let invalid_path = dir.path().join("directory-not-a-state-file");
            std::fs::create_dir(&invalid_path).unwrap();
            let mut writer = node::pipeline::Persistence::new(invalid_path);
            let wait = writer.snapshot(&cache, true).unwrap().unwrap();
            assert!(wait.recv_timeout(Duration::from_secs(10)).unwrap().is_err(), "a failed durable record never permits submission");
            assert!(writer.finish().is_err());
        }

        #[test]
        fn closing_keeps_inflight_lane_then_refunds_latest_successor_and_replays_reorg() {
            let (a, invite, mut cache) = live();
            let b = TxBuilder::new(&a).unwrap();
            let command = movement(&a, &cache.tracker, 1, 92);
            let mut validator = cache.clone(); // precedes local authorization
            cache.authorize(command.tx.clone(), command.inputs.clone(), "move".into(), &node::Config::default()).unwrap();
            cache.pending[0].attempted_ms = 1;
            let Stage::Live(s) = &validator.tracker.stage else { panic!() };
            let f = funding(93);
            let winner = timeout_result(s).unwrap().winner;
            let close_daa = timeout_result(s).unwrap().end_daa as u64 + 1;
            let (close, _) = timeout_tx(&b, s, &key(winner as u8 + 1), &f, &key(3), 100_000).unwrap();
            let entries = vec![s.court.utxo.clone(), f.utxo];
            let mut closing = validator.tracker.clone();
            let records = closing
                .apply_with_events(
                    &a,
                    &invite,
                    &[],
                    &[ChainBlock { hash: hash(12), daa: close_daa, transactions: vec![AcceptedTx { tx: close, inputs: entries }] }],
                )
                .unwrap();
            validator.commit_sync(closing, vec![], records, true, None);
            cache.publish_validated(validator.clone(), false);
            assert_eq!(cache.pending.len(), 1);
            assert!(!cache.discard_conflicting_pending(), "closing does not consume existing lane commands");
            assert_eq!(cache.next_action(0, close_daa, None).unwrap(), None, "payout waits for reserved lanes");
            cache.validate(&a, &invite).unwrap();
            let Stage::Collecting(before) = &cache.tracker.stage else { panic!() };
            let stale_refund = reclaim_tx(&b, before, &funding(94), &key(3), 100_000).unwrap();
            let stale_entries = std::iter::once(before.collector.utxo.clone())
                .chain(before.players.iter().map(|p| p.utxo.clone()))
                .chain(std::iter::once(funding(94).utxo))
                .collect::<Vec<_>>();

            let mut working = validator.tracker.clone();
            let records = working
                .apply_with_events(&a, &invite, &[], &[ChainBlock { hash: hash(13), daa: close_daa + 1, transactions: vec![command.clone()] }])
                .unwrap();
            validator.commit_sync(working, vec![], records, true, None);
            cache.publish_validated(validator.clone(), false);
            assert!(cache.pending.is_empty());
            assert!(!cache.draft_current(&stale_refund, &stale_entries));
            let Stage::Collecting(current) = &cache.tracker.stage else { panic!() };
            assert_eq!(current.players[0].outpoint.transaction_id, command.tx.id());
            assert_eq!(current.players[0].utxo.block_daa_score, close_daa + 1);
            let f = funding(95);
            let refund = reclaim_tx(&b, current, &f, &key(3), 100_000).unwrap();
            let entries = std::iter::once(current.collector.utxo.clone())
                .chain(current.players.iter().map(|p| p.utxo.clone()))
                .chain(std::iter::once(f.utxo))
                .collect();
            let accepted = AcceptedTx { tx: refund, inputs: entries };
            let mut working = validator.tracker.clone();
            let records = working
                .apply_with_events(&a, &invite, &[], &[ChainBlock { hash: hash(14), daa: close_daa + 2, transactions: vec![accepted] }])
                .unwrap();
            validator.commit_sync(working, vec![], records, true, None);
            cache.publish_validated(validator.clone(), false);
            assert!(matches!(cache.tracker.stage, Stage::Reclaimed(_)));
            let replacement = [ChainBlock { hash: hash(15), daa: close_daa + 3, transactions: vec![] }];
            let report = node::audit::ReorgEvent::capture(&invite, &validator.tracker, &[hash(14)], &replacement, Some(hash(13)));
            let mut working = validator.tracker.clone();
            let records = working.apply_with_events(&a, &invite, &[hash(14)], &replacement).unwrap();
            let reorg = validator.commit_sync(working, vec![report], records, true, None);
            cache.publish_validated(validator, reorg);
            assert!(matches!(cache.tracker.stage, Stage::Collecting(_)));
            cache.validate(&a, &invite).unwrap();
            let dir = tempfile::tempdir().unwrap();
            node::save(&dir.path().join("state.json"), &cache).unwrap();
            let recovered: Cache = node::load(&dir.path().join("state.json")).unwrap();
            recovered.validate(&a, &invite).unwrap();
            assert_eq!(recovered.next_action(0, close_daa + 3, None).unwrap(), Some(Action::Reclaim));
        }
    }

    mod game_reorgs {
        use super::*;
        use node::audit::{ReorgEvent, TransactionRecord, TransactionStatus};

        fn accepted_command() -> (argent_runtime::Artifact, Invitation, Cache, AcceptedTx) {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let command = movement(&a, &tracker, -1, 92);
            tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 190, transactions: vec![command.clone()] }]).unwrap();
            let records = tracker
                .history
                .iter()
                .flat_map(|undo| {
                    undo.events
                        .iter()
                        .map(|event| TransactionRecord::new(&invite, event.clone(), Some(undo.hash), 0, TransactionStatus::Accepted))
                })
                .collect();
            let mut cache = Cache::new(&invite);
            assert!(!cache.commit_sync(tracker, vec![], records, true, None));
            cache.transaction_log_pending.clear();
            cache.recent_transactions.clear();
            // This signed child depends on the command's DAA 190 source metadata.
            let pending = movement(&a, &cache.tracker, 1, 94);
            cache.authorize(pending.tx, pending.inputs, "move".into(), &node::Config::default()).unwrap();
            (a, invite, cache, command)
        }

        fn replace_command(
            a: &argent_runtime::Artifact,
            invite: &Invitation,
            cache: &mut Cache,
            command: Option<AcceptedTx>,
            daa: u64,
            settled: bool,
        ) -> bool {
            let added = [ChainBlock { hash: hash(14), daa, transactions: command.into_iter().collect() }];
            let report = ReorgEvent::capture(invite, &cache.tracker, &[hash(12)], &added, Some(hash(11)));
            let mut working = cache.tracker.clone();
            let records = working.apply_with_events(a, invite, &[hash(12)], &added).unwrap();
            cache.commit_sync(working, vec![report], records, settled, None)
        }

        #[test]
        fn unrelated_tip_changes_do_not_report_or_invalidate_pending_input() {
            let (a, invite, mut cache, _) = accepted_command();
            cache.tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(13), daa: 191, transactions: vec![] }]).unwrap();
            let added = [ChainBlock { hash: hash(14), daa: 192, transactions: vec![] }];
            let report = ReorgEvent::capture(&invite, &cache.tracker, &[hash(13)], &added, Some(hash(12)));
            let mut working = cache.tracker.clone();
            let records = working.apply_with_events(&a, &invite, &[hash(13)], &added).unwrap();
            assert!(!cache.commit_sync(working, vec![report], records, true, None));
            assert_eq!(cache.game_reorgs, 0);
            assert!(cache.last_reorg.is_none() && cache.reorg_log_pending.is_empty());
            assert!(cache.transaction_log_pending.is_empty() && cache.recent_transactions.is_empty());
            assert!(!cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn reacceptance_at_the_same_daa_is_silent_even_in_a_different_block() {
            let (a, invite, mut cache, command) = accepted_command();
            let paid = cache.fee_ledger.paid_by(&p2pk(&pubkey(&key(3))));
            assert!(!replace_command(&a, &invite, &mut cache, Some(command), 190, true));
            assert_eq!(cache.game_reorgs, 0);
            assert!(cache.last_reorg.is_none() && cache.reorg_log_pending.is_empty());
            assert!(cache.transaction_log_pending.is_empty() && cache.recent_transactions.is_empty());
            assert!(!cache.pending.first().unwrap().branch_changed);
            assert_eq!(cache.fee_ledger.paid_by(&p2pk(&pubkey(&key(3)))), paid);
        }

        #[test]
        fn reacceptance_at_a_changed_daa_reports_the_exact_game_change() {
            let (a, invite, mut cache, command) = accepted_command();
            let txid = command.tx.id();
            assert!(replace_command(&a, &invite, &mut cache, Some(command), 192, true));
            let event = cache.last_reorg.as_ref().unwrap();
            assert_eq!(cache.game_reorgs, 1);
            assert_eq!(event.reaccepted.len(), 1);
            assert_eq!((event.reaccepted[0].txid, event.reaccepted[0].old_daa, event.reaccepted[0].new_daa), (txid, Some(190), 192));
            assert!(event.missing_game_transactions.is_empty());
            assert!(event.notice().contains("DAA 190 -> 192"));
            assert_eq!(
                cache.transaction_log_pending.iter().map(|record| record.status).collect::<Vec<_>>(),
                vec![TransactionStatus::Reverted, TransactionStatus::Accepted]
            );
            assert!(cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn removed_command_is_reported_and_its_fee_is_removed() {
            let (a, invite, mut cache, command) = accepted_command();
            let txid = command.tx.id();
            let paid = cache.fee_ledger.paid_by(&p2pk(&pubkey(&key(3))));
            assert!(replace_command(&a, &invite, &mut cache, None, 192, true));
            let event = cache.last_reorg.as_ref().unwrap();
            assert_eq!(event.missing_game_transactions, vec![txid]);
            assert!(event.reaccepted.is_empty());
            assert!(event.notice().contains("1 tx removed"));
            assert_eq!(cache.fee_ledger.paid_by(&p2pk(&pubkey(&key(3)))), paid - 100_000);
            assert_eq!(cache.transaction_log_pending.len(), 1);
            assert_eq!(cache.transaction_log_pending[0].status, TransactionStatus::Reverted);
            assert!(cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn partial_catchup_survives_restart_without_a_false_removed_command_notice() {
            let (a, invite, mut cache, command) = accepted_command();
            assert!(!replace_command(&a, &invite, &mut cache, None, 189, false));
            assert!(cache.last_reorg.is_none() && cache.reorg_log_pending.is_empty());
            assert!(cache.transaction_log_pending.is_empty());
            let mut cache: Cache = serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
            assert_eq!(cache.reorg_candidates.len(), 1);
            let mut working = cache.tracker.clone();
            let records = working
                .apply_with_events(&a, &invite, &[], &[ChainBlock { hash: hash(15), daa: 190, transactions: vec![command] }])
                .unwrap();
            assert!(!cache.commit_sync(working, vec![], records, true, None));
            assert!(cache.reorg_candidates.is_empty() && cache.sync_transactions.is_empty());
            assert!(cache.last_reorg.is_none() && cache.transaction_log_pending.is_empty());
            assert!(!cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn temporary_reacceptance_time_changes_are_compared_with_the_original_view() {
            let (a, invite, mut cache, command) = accepted_command();
            assert!(!replace_command(&a, &invite, &mut cache, Some(command.clone()), 192, false));
            let added = [ChainBlock { hash: hash(15), daa: 190, transactions: vec![command] }];
            let report = ReorgEvent::capture(&invite, &cache.tracker, &[hash(14)], &added, Some(hash(11)));
            let mut working = cache.tracker.clone();
            let records = working.apply_with_events(&a, &invite, &[hash(14)], &added).unwrap();
            assert!(!cache.commit_sync(working, vec![report], records, true, None));
            assert_eq!(cache.game_reorgs, 0);
            assert!(cache.last_reorg.is_none() && cache.transaction_log_pending.is_empty());
            assert!(!cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn deep_replay_reports_known_game_transactions_that_really_disappear() {
            let (_, invite, mut cache, command) = accepted_command();
            let known: std::collections::HashSet<_> = cache.fee_ledger.payments.keys().copied().collect();
            cache.tracker.history.clear();
            let report = ReorgEvent::capture(&invite, &cache.tracker, &[hash(99)], &[], None);
            assert!(!report.history_complete && !report.affects_game());
            assert!(cache.commit_sync(Tracker::new(&invite), vec![report], vec![], true, Some(0)));
            let event = cache.last_reorg.as_ref().unwrap();
            assert!(event.missing_game_transactions.contains(&command.tx.id()));
            assert_eq!(event.missing_game_transactions.iter().copied().collect::<std::collections::HashSet<_>>(), known);
            assert!(cache.fee_ledger.payments.is_empty());
            assert!(cache.pending.first().unwrap().branch_changed);
        }

        #[test]
        fn missing_deep_undo_details_do_not_warn_when_the_game_is_replayed_unchanged() {
            let (_, invite, mut cache, _) = accepted_command();
            let working = cache.tracker.clone();
            let records = working
                .history
                .iter()
                .flat_map(|undo| {
                    undo.events
                        .iter()
                        .map(|event| TransactionRecord::new(&invite, event.clone(), Some(undo.hash), 1, TransactionStatus::Accepted))
                })
                .collect();
            cache.tracker.history.clear();
            let report = ReorgEvent::capture(&invite, &cache.tracker, &[hash(99)], &[], None);
            assert!(!cache.commit_sync(working, vec![report], records, true, Some(0)));
            assert_eq!(cache.game_reorgs, 0);
            assert!(cache.last_reorg.is_none() && cache.reorg_log_pending.is_empty());
            assert!(!cache.pending.first().unwrap().branch_changed);
        }
    }

    mod spending_and_motion {
        use super::*;
        use node::{
            audit::{TransactionRecord, TransactionStatus},
            fees::{Ledger, coins},
        };
        use std::time::{Duration, Instant};

        #[test]
        fn paid_fees_include_setup_and_follow_acceptance_reorgs_and_restart() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let mut ledger = Ledger::new();
            for undo in &tracker.history {
                for event in &undo.events {
                    ledger.apply(&TransactionRecord::new(&invite, event.clone(), Some(undo.hash), 0, TransactionStatus::Accepted));
                }
            }
            let payer = p2pk(&pubkey(&key(3)));
            assert_eq!(ledger.paid_by(&payer), 100_000, "host pays genesis");
            assert_eq!(ledger.paid_by(&p2pk(&pubkey(&key(2)))), 100_000, "guest pays start fee, excluding its stake");
            let command = movement(&a, &tracker, -1, 92);
            let records = tracker
                .apply_with_events(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![command.clone()] }])
                .unwrap();
            for record in &records {
                ledger.apply(record);
                ledger.apply(record);
            }
            assert_eq!(ledger.paid_by(&payer), 200_000, "duplicate observations must not double-charge");
            let records = tracker
                .apply_with_events(&a, &invite, &[hash(12)], &[ChainBlock { hash: hash(13), daa: 106, transactions: vec![] }])
                .unwrap();
            for record in &records {
                ledger.apply(record);
            }
            assert_eq!(ledger.paid_by(&payer), 100_000, "reverted fees are no longer spent");
            let records = tracker
                .apply_with_events(&a, &invite, &[], &[ChainBlock { hash: hash(14), daa: 107, transactions: vec![command] }])
                .unwrap();
            for record in &records {
                ledger.apply(record);
            }
            let restored: Ledger = serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
            assert_eq!(restored.paid_by(&payer), 200_000);
            assert!(restored.complete);
            assert_eq!(coins(300_000), "0.00300000");
        }

        #[test]
        fn completed_screen_keeps_personal_paid_and_pending_fees_visible() {
            let result = Finish { stake_per_player: DEFAULT_STAKE, left_score: 3, right_score: 1, winner: 0, end_daa: 200 };
            let spending = network_ui::Spending { paid: 123456789, pending: 34567, complete: true };
            let frame = network_ui::completed_frame(&result, Some(1), "YOU: RIGHT", "Accepted on chain", Some(&spending), "TKAS");
            let screen = terminal::ScreenFrame::compose(
                &frame,
                Some("ACCEPTED REFUND\ntxid"),
                Some("REORG OCCURRED\nbranch changed\nR acknowledges"),
                (80, 25),
            );
            let visible = screen.rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("\n");
            assert!(visible.contains("Your network fees paid: 1.23456789 TKAS"));
            assert!(visible.contains("Pending fees (not spent yet): 0.00034567 TKAS"));
            let partial = network_ui::Spending { complete: false, ..spending };
            assert!(network_ui::completed_frame(&result, Some(1), "PONG", "", Some(&partial), "TKAS").contains("partial history"));
        }

        #[test]
        fn live_clock_can_advance_with_pending_input_but_stale_peers_cannot_drive_it() {
            let a = artifact();
            let (invite, mut t, lobby) = fixture(&a);
            start(&a, &invite, &mut t, &lobby);
            let now = Instant::now();
            let mut display = network_ui::Display {
                stage: t.stage,
                daa: 160,
                ready: true,
                observed: now,
                clock_epoch: 3,
                pending: 1,
                accepted_previews: Vec::new(),
                queued: 1,
                spending: None,
                reorgs: 0,
                status: String::new(),
                last_reorg: None,
                recent_transactions: Default::default(),
                lobby_hint: None,
            };
            let sample = network_ui::ClockSample { epoch: 3, daa: 165, observed: now };
            assert!(network_ui::clock_matches(&display, &sample, now), "pending input must not pause clock observations");
            let mut clock = network_ui::ObservedClock::default();
            clock.update(160, 102, false, now);
            clock.update(sample.daa, 102, false, now);
            assert!(clock.sample(now + Duration::from_millis(200)) > 160 - network_ui::PLAYBACK_INITIAL_BUFFER_DAA);
            display.clock_epoch = 4;
            assert!(!network_ui::clock_matches(&display, &sample, now), "old connection/fork observations must not drive the new state");
            display.clock_epoch = 3;
            assert!(!network_ui::clock_matches(&display, &sample, now + Duration::from_secs(11)));
            display.ready = false;
            assert!(!network_ui::clock_matches(&display, &sample, now));
        }
    }
    #[test]
    fn activity_and_reorg_panels_keep_the_court_and_error_status_visible() {
        let mut lines = vec!["title".to_string(), "score".into(), "updater".into(), format!("+{}+", "-".repeat(64))];
        for row in 0..16 {
            lines.push(format!("|court row {row:02}|"));
        }
        lines.extend([
            format!("+{}+", "-".repeat(64)),
            "controls".into(),
            "metrics".into(),
            "Spending paused: fee budget exhausted".into(),
        ]);
        let frame = lines.join("\n");
        for height in 20..=24 {
            let body = terminal::panel_body(&frame, height);
            assert_eq!(body.iter().filter(|line| line.starts_with("|court row")).count(), 16);
            assert_eq!(body.last().copied(), Some("Spending paused: fee budget exhausted"));
            assert!(body.len() <= height);
        }
    }

    #[test]
    fn rendering_updates_only_changed_rows_and_erases_shortened_content_without_scrolling() {
        use terminal::ScreenFrame;
        let old = ScreenFrame::compose("PONG\n| ball    |\nlong status", None, None, (80, 25));
        assert_eq!(old.changed_rows(None).len(), 25);
        let same = ScreenFrame::compose("PONG\n| ball    |\nlong status", None, None, (80, 25));
        assert!(same.changed_rows(Some(&old)).is_empty());
        let next = ScreenFrame::compose("PONG\n|    ball |\nOK", None, None, (80, 25));
        assert_eq!(next.changed_rows(Some(&old)), vec![1, 2]);
        assert!(next.rows[2].text.starts_with("OK       "));
        assert!(next.rows.iter().all(|r| r.text.chars().count() == 79 && !r.text.contains(['\n', '\r'])));
        let resized = ScreenFrame::compose("PONG", None, None, (70, 20));
        assert_eq!(resized.changed_rows(Some(&next)).len(), 20);
    }

    #[test]
    fn rendering_removes_activity_and_reorg_colours_when_panels_disappear() {
        use terminal::{RowStyle, ScreenFrame};
        let with = ScreenFrame::compose("PONG", Some("ACCEPTED\ntxid"), Some("REORG OCCURRED\nrollback\nPress R"), (80, 25));
        assert_eq!(with.rows[20].style, RowStyle::Activity);
        assert_eq!(with.rows[22].style, RowStyle::Alert);
        let without = ScreenFrame::compose("PONG", None, None, (80, 25));
        assert_eq!(without.changed_rows(Some(&with)), vec![20, 21, 22, 23, 24]);
        assert!(without.rows[20..].iter().all(|r| r.style == RowStyle::Plain && r.text.trim().is_empty()));
    }

    #[test]
    fn wallet_generation_and_reload_preserve_the_player_key_without_replacing_funded_files() {
        use node::wallet::{Wallet, public_identity};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("player.wallet.json");
        let generated = Wallet::generate().unwrap();
        let independent = Wallet::generate().unwrap();
        assert!(generated.player != independent.player);
        generated.save_new(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let loaded = Wallet::load(&path).unwrap();
        assert_eq!(loaded.player, generated.player);
        let public = public_identity("testnet-10", &loaded.player).unwrap();
        assert!(public["player_address"].as_str().unwrap().starts_with("kaspatest:"));
        assert!(!public.to_string().contains(&generated.player.secret_key().display_secret().to_string()));
        assert!(independent.save_new(&path).is_err());
        assert!(std::fs::read(&path).unwrap() == original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        std::fs::write(&path, r#"{"version":1,"player_key":"secret-that-must-not-be-echoed"}"#).unwrap();
        let error = Wallet::load(&path).err().unwrap().to_string();
        assert!(!error.contains("secret-that-must-not-be-echoed"));
    }

    #[test]
    fn wallet_cli_generates_and_reuses_an_address_offline_without_exposing_secrets() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("player.wallet.json");
        let mut command = Command::new(env!("CARGO_BIN_EXE_pong-node"));
        command
            .current_dir(dir.path())
            .args([
                "--network",
                "testnet-10",
                "--endpoint",
                "grpc://127.0.0.1:1",
                "--artifact",
                "missing-artifact.json",
                "keygen",
                "--out",
            ])
            .arg(&path);
        let generated = command.output().unwrap();
        assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));
        let public: serde_json::Value = serde_json::from_slice(&generated.stdout).unwrap();
        assert_eq!(public["network"], "testnet-10");
        assert_eq!(public["player_address"], public["fee_address"]);
        assert!(public["fee_address"].as_str().unwrap().starts_with("kaspatest:"));
        let wallet = node::wallet::Wallet::load(&path).unwrap();
        let secret = wallet.player.secret_key().display_secret().to_string();
        assert!(!String::from_utf8_lossy(&generated.stdout).contains(&secret));
        assert!(!String::from_utf8_lossy(&generated.stderr).contains(&secret));
        let reused = Command::new(env!("CARGO_BIN_EXE_pong-node"))
            .current_dir(dir.path())
            .env("PONG_PLAYER_KEY", "invalid-env-must-be-ignored")
            .env("PONG_FEE_KEY", "invalid-env-must-be-ignored")
            .args(["--network", "testnet-10", "--wallet"])
            .arg(&path)
            .arg("address")
            .output()
            .unwrap();
        assert!(reused.status.success(), "{}", String::from_utf8_lossy(&reused.stderr));
        let reused: serde_json::Value = serde_json::from_slice(&reused.stdout).unwrap();
        assert_eq!(reused["player"], public["player"]);
        assert_eq!(reused["fee_address"], public["fee_address"]);
        let original = std::fs::read(&path).unwrap();
        assert!(!command.output().unwrap().status.success());
        assert!(std::fs::read(&path).unwrap() == original);
    }

    #[test]
    fn config_accepts_grpc_borsh_and_explicit_public_fallbacks() {
        let mut c = node::Config::default();
        for endpoint in ["grpc://127.0.0.1:16610", "ws://127.0.0.1:17610", "wss://node.example/kaspa/testnet-10/wrpc/borsh", "resolver"] {
            c.endpoint = endpoint.into();
            c.validate().unwrap();
        }
        c.fallback_endpoints = vec!["wss://node.example/kaspa/testnet-10/wrpc/borsh".into()];
        c.resolver_urls = vec!["https://resolver.example".into()];
        c.validate().unwrap();
        c.fallback_endpoints.push("https://not-a-websocket.example".into());
        assert!(c.validate().is_err());
        c.fallback_endpoints.clear();
        c.endpoint = "https://not-a-websocket.example".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepted_observer_replays_moves_resolves_and_reorgs_with_new_timestamps() {
        let a = artifact();
        let (i, mut t, lobby) = fixture(&a);
        start(&a, &i, &mut t, &lobby);
        let command = movement(&a, &t, 1, 92);
        let id = command.tx.id();
        t.apply(&a, &i, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![command.clone()] }]).unwrap();
        let resolve = resolution(&a, &t, 93);
        let resolve_id = resolve.tx.id();
        t.apply(&a, &i, &[], &[ChainBlock { hash: hash(13), daa: 107, transactions: vec![resolve] }]).unwrap();
        let Stage::Live(s) = &t.stage else { panic!() };
        assert_eq!((s.players[0].state.through, s.players[0].state.y), (106, 17));
        let replacement = ChainBlock { hash: hash(14), daa: 106, transactions: vec![command.clone()] };
        let report = node::audit::ReorgEvent::capture(&i, &t, &[hash(13), hash(12)], &[replacement], Some(hash(11)));
        assert_eq!(report.removed_daa_span, Some(5));
        assert_eq!(report.removed_blocks.len(), 2);
        assert!(report.history_complete);
        let command_event = report.removed_game_transactions.iter().find(|e| e.action == "move").unwrap();
        assert_eq!((command_event.side, command_event.direction, command_event.accepted_daa), (Some(0), Some(1), Some(105)));
        assert_eq!(report.reaccepted[0].txid, id);
        assert_eq!((report.reaccepted[0].old_daa, report.reaccepted[0].new_daa), (Some(105), 106));
        assert_eq!(report.missing_game_transactions, vec![resolve_id]);
        // The identical transaction is accepted on a replacement branch one DAA later.
        let records = t
            .apply_with_events(&a, &i, &[hash(13), hash(12)], &[ChainBlock { hash: hash(14), daa: 106, transactions: vec![command] }])
            .unwrap();
        use node::audit::TransactionStatus::{Accepted, Reverted};
        assert_eq!(records.iter().map(|r| r.status).collect::<Vec<_>>(), vec![Reverted, Reverted, Accepted]);
        assert_eq!(records[1].event.txid, id);
        assert_eq!(records[1].event.accepted_daa, Some(105));
        assert_eq!(records[2].event.txid, id);
        assert_eq!(records[2].event.accepted_daa, Some(106));
        let Stage::Live(s) = &t.stage else { panic!() };
        assert_eq!(s.players[0].utxo.block_daa_score, 106);
        assert_eq!(s.court.state.left_y, 16);
        let resolve = resolution(&a, &t, 94);
        t.apply(&a, &i, &[], &[ChainBlock { hash: hash(15), daa: 108, transactions: vec![resolve] }]).unwrap();
        let Stage::Live(s) = &t.stage else { panic!() };
        assert_eq!((s.players[0].state.through, s.players[0].state.y), (107, 17));
        assert_eq!(t.reorgs, 1);
        assert!(t.accepted(id));
        let before = serde_json::to_value(&t).unwrap();
        let mut bad = movement(&a, &t, -1, 95);
        bad.inputs[0].block_daa_score += 1;
        assert!(t.apply(&a, &i, &[], &[ChainBlock { hash: hash(16), daa: 109, transactions: vec![bad] }]).is_err());
        assert_eq!(serde_json::to_value(&t).unwrap(), before, "invalid batch must not partially advance the cache");
    }
    #[test]
    fn accepted_transaction_log_is_durable_and_keeps_reorg_corrections() {
        use node::audit::{ReorgLog, TransactionStatus, transaction_log_path};
        let a = artifact();
        let (i, mut t, lobby) = fixture(&a);
        start(&a, &i, &mut t, &lobby);
        let command = movement(&a, &t, 1, 92);
        let txid = command.tx.id();
        let accepted =
            t.apply_with_events(&a, &i, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![command.clone()] }]).unwrap();
        let corrected =
            t.apply_with_events(&a, &i, &[hash(12)], &[ChainBlock { hash: hash(13), daa: 106, transactions: vec![command] }]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let log_file = transaction_log_path(&path);
        let mut cache = Cache::new(&i);
        cache.tracker = t;
        cache.transaction_log_pending.extend(accepted);
        cache.transaction_log_pending.extend(corrected);
        let unflushed = cache.clone();
        let mut log = ReorgLog::open(&log_file).unwrap();
        let emitted = cache.flush_transaction_log(&path, &mut log).unwrap();
        assert_eq!(emitted.len(), 3);
        assert_eq!(
            emitted.iter().map(|r| r.status).collect::<Vec<_>>(),
            vec![TransactionStatus::Accepted, TransactionStatus::Reverted, TransactionStatus::Accepted]
        );
        assert!(emitted.iter().all(|r| r.event.txid == txid));
        assert_ne!(emitted[0].id, emitted[2].id);
        assert!(emitted[0].terminal_lines().contains(&txid.to_string()));
        assert!(emitted[0].terminal_lines().contains("ACCEPTED MOVE LEFT DOWN | DAA 105"));
        assert!(cache.transaction_log_pending.is_empty());
        drop(log);
        node::save(&path, &unflushed).unwrap();
        let mut recovered: Cache = node::load(&path).unwrap();
        let mut log = ReorgLog::open(&log_file).unwrap();
        assert!(recovered.flush_transaction_log(&path, &mut log).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&log_file).unwrap().lines().count(), 3);
    }

    #[test]
    fn reorg_log_survives_restart_deduplicates_outbox_and_recovers_partial_tail() {
        use node::audit::{ReorgEvent, ReorgLog, log_path};
        use std::io::Write;
        let a = artifact();
        let (i, mut t, lobby) = fixture(&a);
        start(&a, &i, &mut t, &lobby);
        let command = movement(&a, &t, -1, 92);
        t.apply(&a, &i, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![command] }]).unwrap();
        let mut event = ReorgEvent::capture(&i, &t, &[hash(12)], &[], Some(hash(11)));
        event.discarded_local_inputs = 3;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("left-state.json");
        let path = log_path(&state);
        let mut cache = Cache::new(&i);
        cache.tracker = t.clone();
        cache.last_reorg = Some(event.clone());
        cache.reorg_log_pending.push(event.clone());
        let unflushed = cache.clone();
        let mut log = ReorgLog::open(&path).unwrap();
        cache.flush_reorg_log(&state, &mut log).unwrap();
        assert!(cache.reorg_log_pending.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        drop(log);
        // Simulate a crash after the log synced but before the outbox was cleared.
        node::save(&state, &unflushed).unwrap();
        let mut recovered: Cache = node::load(&state).unwrap();
        let mut log = ReorgLog::open(&path).unwrap();
        recovered.flush_reorg_log(&state, &mut log).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        drop(log);
        // An interrupted append must not destroy earlier complete records.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"interrupted\":").unwrap();
        let mut log = ReorgLog::open(&path).unwrap();
        log.append(&event).unwrap();
        let lines = std::fs::read_to_string(&path).unwrap();
        let restored: ReorgEvent = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(restored.id, event.id);
        assert_eq!(restored.discarded_local_inputs, 3);
        assert_eq!(restored.removed_game_transactions[0].direction, Some(-1));

        // Older caches lack DAA/event metadata: report unknown rather than zero.
        let mut old = serde_json::to_value(&t).unwrap();
        for undo in old["history"].as_array_mut().unwrap() {
            undo.as_object_mut().unwrap().remove("daa");
            undo.as_object_mut().unwrap().remove("events");
        }
        let old: Tracker = serde_json::from_value(old).unwrap();
        let unknown = ReorgEvent::capture(&i, &old, &[hash(12)], &[], Some(hash(11)));
        assert!(!unknown.history_complete);
        assert_eq!(unknown.removed_daa_span, None);
        assert!(unknown.notice().contains("unknown"));
        assert!(unknown.notice().contains("incomplete"));
        let mut deep = ReorgEvent::capture(&i, &t, &[hash(99)], &[], None);
        assert!(!deep.history_complete);
        assert_eq!(deep.removed_daa_span, None);
        deep.id.push_str("-new");
        log.append(&deep).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn guest_join_requires_correct_signatures_and_preserves_deposits() {
        let a = artifact();
        let (i, _, lobby) = fixture(&a);
        let p = setup::prepare(&a, &i, &lobby).unwrap();
        assert!(p.right_signature.is_empty());
        assert!(approve_start(&a, &i, p.clone(), &key(1), 100_000).is_err());
        let mut forged = p.clone();
        forged.transaction.outputs[0].value -= 1;
        assert!(approve_start(&a, &i, forged, &key(2), 100_000).is_err());
        let signed = approve_start(&a, &i, p, &key(2), 100_000).unwrap();
        assert_eq!(signed.transaction.outputs.iter().take(1 + QUEUE_COUNT).map(|o| o.value).sum::<u64>(), GAME_POT);
        let entries = signed.inputs();
        let mut tx = signed.transaction;
        argent_runtime::execute_transaction_with_covenants(&mut tx, entries).unwrap();
    }
    #[test]
    fn scheduling_ignores_idle_blocks_and_cache_commitments_are_checked() {
        let a = artifact();
        let (i, mut t, lobby) = fixture(&a);
        start(&a, &i, &mut t, &lobby);
        assert_eq!(action(&t.stage, 0, 110, None).unwrap(), None);
        assert_eq!(action(&t.stage, 0, 110, Some(1)).unwrap(), Some(Action::Move(1)));
        let cmd = movement(&a, &t, 1, 92);
        t.apply(&a, &i, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![cmd] }]).unwrap();
        assert_eq!(action(&t.stage, 0, 105, None).unwrap(), None);
        assert_eq!(action(&t.stage, 0, 135, None).unwrap(), None);
        assert_eq!(action(&t.stage, 0, 135, Some(1)).unwrap(), Some(Action::Move(1)));
        assert_eq!(action(&t.stage, 1, 105, None).unwrap(), None);
        // Timeout is Court age, with an exact boundary, not time since the last
        // command. The rival's cooperation/signature is never required.
        assert_eq!(action(&t.stage, 1, 121, None).unwrap(), None);
        assert_eq!(action(&t.stage, 1, 124, None).unwrap(), None);
        assert_eq!(action(&t.stage, 1, 164, None).unwrap(), None);
        assert_eq!(action(&t.stage, 1, 165, None).unwrap(), None);
        let Stage::Live(s) = &t.stage else { panic!() };
        let (receiver, deadline) = next_contact(s).unwrap();
        assert_eq!(action(&t.stage, receiver as i64, deadline as u64, None).unwrap(), Some(Action::Resolve(false)));
        let mut c = Cache::new(&i);
        c.tracker = t;
        c.validate(&a, &i).unwrap();
        if let Stage::Live(s) = &mut c.tracker.stage {
            s.court.state.ball_x += 1;
        }
        assert!(c.validate(&a, &i).is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        node::save(&file, &i).unwrap();
        node::save(&file, &i).unwrap();
        let _: Invitation = node::load(&file).unwrap();
    }

    #[test]
    fn accepted_competing_commands_release_a_losing_resolve_without_losing_fee_accounting() {
        let a = artifact();
        let (invite, mut tracker, lobby) = fixture(&a);
        start(&a, &invite, &mut tracker, &lobby);
        let first = movement(&a, &tracker, 1, 92);
        tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![first] }]).unwrap();
        let pending = resolution(&a, &tracker, 93);
        let mut cache = Cache::new(&invite);
        cache.tracker = tracker;
        cache.authorize(pending.tx, pending.inputs, "resolve".into(), &node::Config::default()).unwrap();
        let charged = cache.authorized_fees;
        assert!(!cache.discard_conflicting_pending(), "an unchanged accepted snapshot proves no conflict");
        let competing = movement(&a, &cache.tracker, -1, 94);
        cache.tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(13), daa: 106, transactions: vec![competing] }]).unwrap();
        assert!(cache.discard_conflicting_pending());
        assert!(cache.pending.is_empty());
        assert_eq!(cache.authorized_fees, charged);
        assert_eq!(action(&cache.tracker.stage, 0, 135, None).unwrap(), None);
        let rebuilt = resolution(&a, &cache.tracker, 95);
        assert_eq!(rebuilt.inputs.len(), 5);
    }

    #[test]
    fn measured_fees_and_rescan_preserve_spending_limits() {
        use node::client::{priced_transaction, relay_fee, transaction_fee};
        let a = artifact();
        let (i, mut t, lobby) = fixture(&a);
        start(&a, &i, &mut t, &lobby);
        let Stage::Live(s) = &t.stage else { panic!() };
        let f = funding(92);
        let config = node::Config::default();
        let tx = priced_transaction(&config, |fee| Ok(command_tx(&TxBuilder::new(&a)?, &s.players[0], 1, &key(1), &f, &key(3), fee)?.0))
            .unwrap();
        let inputs = vec![s.players[0].utxo.clone(), f.utxo];
        let fee = transaction_fee(&tx, &inputs).unwrap();
        assert_eq!(fee, relay_fee(&tx, &config).unwrap());
        assert!(fee < config.fee_sompi, "small movements should not pay the large resolve cap");
        println!("movement fee: {fee} sompi vs previous fixed {}", config.fee_sompi);
        let mut executed = tx.clone();
        argent_runtime::execute_transaction_with_covenants(&mut executed, inputs.clone()).unwrap();
        let mut low_cap = config.clone();
        low_cap.fee_sompi = fee - 1;
        assert!(relay_fee(&tx, &low_cap).is_err());
        let mut cache = Cache::new(&i);
        cache.tracker = t;
        cache.authorize(tx.clone(), inputs.clone(), "move".into(), &config).unwrap();
        assert_eq!(cache.authorized_fees, fee);
        cache.rescan(&i);
        assert_eq!(cache.authorized_fees, fee);
        assert_eq!(cache.tracker.cursor, i.anchor);
        let pending = cache.pending.first().unwrap();
        assert_eq!(pending.transaction.id(), tx.id());
        assert!(pending.branch_changed);
        cache.validate(&a, &i).unwrap();
        // A full budget must still stop a new authorization after rescan.
        cache.pending.clear();
        cache.authorized_fees = config.fee_budget_sompi;
        assert!(cache.authorize(tx, inputs, "move".into(), &config).is_err());
        assert!(cache.pending.is_empty());
        assert_eq!(cache.authorized_fees, config.fee_budget_sompi);
    }

    mod parallel_queues {
        use super::*;

        #[test]
        fn scheduler_reserves_three_lanes_and_never_overlaps_a_resolve() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let mut cache = Cache::new(&invite);
            cache.tracker = tracker;
            let b = TxBuilder::new(&a).unwrap();
            let config = node::Config::default();
            for lane in 0..QUEUES_PER_PLAYER {
                assert_eq!(cache.next_action(0, 103, Some(1)).unwrap(), Some(Action::Move(1)));
                let index = cache.available_lane(0).unwrap();
                assert_eq!(index, lane * 2);
                let Stage::Live(s) = &cache.tracker.stage else { panic!() };
                let fee = funding(120 + lane as u8);
                let (tx, _) = command_tx(&b, &s.players[index], 1, &key(1), &fee, &key(3), 100_000).unwrap();
                cache.authorize(tx, vec![s.players[index].utxo.clone(), fee.utxo], "move 1".into(), &config).unwrap();
            }
            assert_eq!(cache.pending.len(), QUEUES_PER_PLAYER);
            assert!(cache.next_action(0, 103, Some(1)).unwrap().is_none());
            assert!(cache.available_lane(0).is_none());
            cache.validate(&a, &invite).unwrap();
            let Stage::Live(s) = &cache.tracker.stage else { panic!() };
            let f = funding(130);
            let (tx, _) = checkpoint_tx(&b, s, 0, 103, &key(1), &f, &key(3), 100_000).unwrap();
            let entries =
                vec![s.players[0].utxo.clone(), s.court.utxo.clone(), s.players[2].utxo.clone(), s.players[4].utxo.clone(), f.utxo];
            assert!(cache.authorize(tx, entries, "resolve".into(), &config).is_err());
            let freed = cache.pending[0].transaction.id();
            cache.discard_pending(freed);
            assert_eq!(cache.available_lane(0), Some(0));
            assert_eq!(cache.next_action(0, 103, Some(-1)).unwrap(), Some(Action::Move(-1)));
        }
    }

    mod fee_efficiency {
        use super::*;
        use node::client::{priced_transaction, transaction_fee};

        #[test]
        fn lane_selection_preserves_state_and_fees_for_identical_workloads() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let b = TxBuilder::new(&a).unwrap();
            let config = node::Config::default();
            let run = |prefer_small: bool| {
                let mut cache = Cache::new(&invite);
                cache.tracker = tracker.clone();
                let mut fees = 0;
                let mut lanes = Vec::new();
                for n in 0..9 {
                    let Stage::Live(s) = &cache.tracker.stage else { panic!() };
                    let i = if prefer_small {
                        cache.available_lane(0).unwrap()
                    } else {
                        // The previous scheduler balanced counts starting at lane zero.
                        s.players
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| p.state.side == 0 && p.state.count < 4)
                            .min_by_key(|(_, p)| (p.state.count, p.state.lane))
                            .unwrap()
                            .0
                    };
                    let f = funding(120 + n);
                    let tx =
                        priced_transaction(&config, |fee| Ok(command_tx(&b, &s.players[i], 1, &key(1), &f, &key(3), fee)?.0)).unwrap();
                    fees += transaction_fee(&tx, &[s.players[i].utxo.clone(), f.utxo]).unwrap();
                    let q = s.players[i].state.push(1, s.players[i].utxo.block_daa_score).unwrap();
                    let Stage::Live(s) = &mut cache.tracker.stage else { panic!() };
                    s.players[i] = accepted_cell(&tx, 0, 104 + n as u64, q);
                    lanes.push(i);
                }
                (fees, lanes, cache)
            };
            let (after, lanes, new) = run(true);
            assert_eq!(lanes, vec![0, 2, 4, 0, 2, 4, 0, 2, 4]);
            let (balanced_fees, _, balanced) = run(false);
            assert!(after > 0);
            assert_eq!(after, balanced_fees);
            let Stage::Live(old) = &balanced.tracker.stage else { panic!() };
            let Stage::Live(new) = &new.tracker.stage else { panic!() };
            assert_eq!(project(old, 120).unwrap().world, project(new, 120).unwrap().world);
        }

        #[test]
        fn waits_for_contact_or_queue_pressure_instead_of_quiet_tail() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let cmd = movement(&a, &tracker, 1, 92);
            tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 105, transactions: vec![cmd] }]).unwrap();
            assert_eq!(action(&tracker.stage, 0, 135, None).unwrap(), None);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let (receiver, t) = next_contact(s).unwrap();
            assert_eq!(action(&tracker.stage, receiver as i64, t as u64, None).unwrap(), Some(Action::Resolve(false)));
            assert_eq!(
                action(&tracker.stage, (1 - receiver) as i64, (t + RESPONSE_TIMEOUT_DAA as i64) as u64, None).unwrap(),
                Some(Action::Timeout)
            );
            // Synthetic pressure affects only the scheduling decision; all VM and
            // state-transition rules remain covered by the contract tests.
            let Stage::Live(s) = &mut tracker.stage else { panic!() };
            s.players[2].state.count = 4;
            s.players[4].state.count = 4;
            assert_eq!(action(&tracker.stage, 0, 120, None).unwrap(), Some(Action::Checkpoint(0, 120)));
            let Stage::Live(s) = &mut tracker.stage else { panic!() };
            s.court.state.left_score = WIN_SCORE;
            assert_eq!(action(&tracker.stage, 0, 120, None).unwrap(), Some(Action::Settle));
        }
    }

    mod smooth_play {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn dropped_start_can_be_queued_again_until_the_lobby_is_spent() {
            let a = artifact();
            let (invite, tracker, lobby) = fixture(&a);
            let p = setup::prepare(&a, &invite, &lobby).unwrap();
            let p = approve_start(&a, &invite, p, &key(2), 100_000).unwrap();
            let mut cache = Cache::new(&invite);
            cache.tracker = tracker;
            assert!(p.enqueue(&mut cache, &pubkey(&key(1)), &node::Config::default()).is_err());
            assert!(p.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            assert!(!p.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            cache.discard_pending(p.transaction.id());
            assert!(p.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
            assert_eq!(cache.pending[0].transaction, p.transaction);
            assert_eq!(cache.authorized_fees, 2 * p.fee, "each fresh authorization reserves its bounded external fee");
            cache.pending.clear();
            start(&a, &invite, &mut cache.tracker, &lobby);
            assert!(!p.enqueue(&mut cache, &pubkey(&key(2)), &node::Config::default()).unwrap());
        }

        #[test]
        fn buffered_projection_applies_future_accepted_commands_at_their_real_time() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let command = movement(&a, &tracker, 1, 92);
            tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 160, transactions: vec![command] }]).unwrap();
            let Stage::Live(s) = &tracker.stage else { panic!() };
            assert_eq!(project(s, 159).unwrap().world.left_y, 16);
            assert_eq!(project(s, 160).unwrap().world.left_y, 17);
            let mut newer = s.clone();
            newer.court.state = project(s, 160).unwrap().world;
            assert!(project(&newer, 159).is_err(), "a consumed checkpoint cannot reconstruct its past");
        }

        #[test]
        fn checkpoints_wait_for_playback_and_fractional_clock_updates_do_not_stall() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let now = Instant::now();
            let first = network_ui::Display {
                stage: tracker.stage.clone(),
                daa: 160,
                ready: true,
                observed: now,
                clock_epoch: 0,
                pending: 0,
                accepted_previews: vec![],
                queued: 0,
                spending: None,
                reorgs: 0,
                status: String::new(),
                last_reorg: None,
                recent_transactions: Default::default(),
                lobby_hint: None,
            };
            let mut later = first.clone();
            let Stage::Live(s) = &mut later.stage else { panic!() };
            s.court.state = project(s, 155).unwrap().world;
            s.court.utxo.block_daa_score = 158;
            let mut playback = network_ui::BufferedCourt::default();
            playback.push(first);
            playback.push(later);
            let Stage::Live(s) = &playback.drawing(150).unwrap().stage else { panic!() };
            assert!(!s.court.state.started);
            let Stage::Live(s) = &playback.drawing(155).unwrap().stage else { panic!() };
            assert_eq!(s.court.state.clock, 155);
            let mut clock = network_ui::ObservedClock::default();
            clock.update(120, 100, false, now);
            for tick in 1..=10 {
                clock.update(120 + tick, 100, false, now + Duration::from_millis(tick * 99));
            }
            assert_eq!(clock.sample(now + Duration::from_millis(1000)), 125);
        }

        #[test]
        fn scheduler_prioritizes_input_and_finalizes_an_idle_win() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let command = movement(&a, &tracker, 1, 92);
            tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 160, transactions: vec![command] }]).unwrap();
            assert_eq!(action(&tracker.stage, 0, 161, None).unwrap(), None);
            assert_eq!(action(&tracker.stage, 0, 190, Some(-1)).unwrap(), Some(Action::Move(-1)));
            assert_eq!(action(&tracker.stage, 0, 190, None).unwrap(), None);
            let Stage::Live(s) = &mut tracker.stage else { panic!() };
            s.players[0].state.count = 0;
            s.court.state = project(s, 160).unwrap().world;
            s.court.state.left_score = 3;
            assert_eq!(action(&tracker.stage, 0, 190, None).unwrap(), Some(Action::Settle), "idle terminal results must settle");
        }
    }

    mod launch_storage {
        use super::*;

        fn coin(id: u8, amount: u64) -> Funding {
            let mut f = funding(id);
            f.utxo.amount = amount;
            f
        }

        fn guest_coin(id: u8, amount: u64) -> Funding {
            let mut f = coin(id, amount);
            f.utxo.script_public_key = p2pk(&pubkey(&key(2)));
            f
        }

        #[test]
        fn tiny_guest_change_is_rejected_before_authorizing_launch() {
            let a = artifact();
            let (invite, _, lobby) = fixture(&a);
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            let config = node::Config::default();
            let pilot = setup::approve(&a, &invite, proposal.clone(), vec![guest_coin(92, 100_000_000_000)], &key(2), &key(2), &config).unwrap();
            let tiny = guest_coin(93, DEFAULT_STAKE as u64 + pilot.fee + 1);
            let error = setup::approve(&a, &invite, proposal, vec![tiny], &key(2), &key(2), &config).unwrap_err();
            assert!(error.to_string().contains("valid storage mass"), "{error}");
        }

        #[test]
        fn chooses_minimum_actual_storage_mass_instead_of_smallest_or_largest_coin() {
            let a = artifact();
            let (invite, _, lobby) = fixture(&a);
            let config = node::Config::default();
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            let candidates = vec![guest_coin(92, DEFAULT_STAKE as u64 + 25_958_800), guest_coin(93, DEFAULT_STAKE as u64 + 100_000_000),
                guest_coin(94, DEFAULT_STAKE as u64 + 571_093_000), guest_coin(95, 100_000_000_000)];
            let signed = setup::approve(&a, &invite, proposal.clone(), candidates.clone(), &key(2), &key(2), &config).unwrap();
            let mass = signed.transaction.storage_mass();
            for funding in candidates {
                if let Ok(other) = setup::approve(&a, &invite, proposal.clone(), vec![funding], &key(2), &key(2), &config) {
                    assert!(mass <= other.transaction.storage_mass());
                }
            }
            assert!(mass < 500_000);
            assert_eq!(signed.transaction.outputs.len(), 2 + QUEUE_COUNT);
            assert_eq!(signed.transaction.outputs.iter().take(1 + QUEUE_COUNT).map(|o| o.value).sum::<u64>(),
                GAME_POT);
            let wrong_mass = signed;
            wrong_mass.transaction.set_storage_mass(0);
            assert!(setup::validate(&a, &invite, &wrong_mass, config.fee_sompi).is_err());
        }

        #[test]
        fn local_unfunded_join_draft_exposes_no_host_fee_coin_or_mineability_claim() {
            let a = artifact();
            let (invite, _, lobby) = fixture(&a);
            let proposal = setup::prepare(&a, &invite, &lobby).unwrap();
            assert!(proposal.funding.is_none());
            assert!(!proposal.complete());
            assert_eq!(proposal.transaction.inputs.len(), 1);
            assert_eq!(proposal.lobby.utxo.amount, DEFAULT_STAKE as u64);
            assert_eq!(proposal.transaction.storage_mass(), 0);
            assert_eq!(proposal.transaction.outputs.iter().map(|o| o.value).sum::<u64>(),
                proposal.lobby.utxo.amount + DEFAULT_STAKE as u64);
        }

        #[test]
        fn fee_cap_trial_does_not_manufacture_tiny_change_for_a_valid_move() {
            let a = artifact();
            let (invite, mut t, lobby) = fixture(&a);
            start(&a, &invite, &mut t, &lobby);
            let Stage::Live(s) = &t.stage else { panic!() };
            let f = coin(92, node::Config::default().fee_sompi + 1);
            let b = TxBuilder::new(&a).unwrap();
            assert!(command_tx(&b, &s.players[0], 1, &key(1), &f, &key(3), node::Config::default().fee_sompi).is_err());
            let tx = node::client::priced_transaction(&node::Config::default(), |fee| {
                Ok(command_tx(&b, &s.players[0], 1, &key(1), &f, &key(3), fee)?.0)
            })
            .unwrap();
            assert!(tx.storage_mass() <= 500_000);
        }
    }

    mod fixed_pace {
        use super::*;

        #[test]
        fn current_invitations_use_the_medium_default_pace_and_cli_has_no_speed_selection() {
            let a = artifact();
            let (mut invite, _, _) = fixture(&a);
            assert_eq!(DEFAULT_BALL_TICK_DAA, 7);
            assert_eq!(World::opening(pubkey(&key(1)), pubkey(&key(2))).ball_tick_daa, 7);
            invite.validate(&a).unwrap();
            let mut old_lobby = invite.lobby.clone();
            old_lobby.ball_tick_daa = 6;
            let old_funding = funding(89);
            node::invitations::create(&a, "devnet", hash(0), &old_lobby, &old_funding, &key(1), &key(3), 100_000)
                .unwrap()
                .validate(&a)
                .unwrap();
            invite.lobby.ball_tick_daa = 65;
            assert!(invite.validate(&a).is_err());
            let help = std::process::Command::new(env!("CARGO_BIN_EXE_pong-node")).args(["host", "--help"]).output().unwrap();
            assert!(help.status.success());
            assert!(!String::from_utf8_lossy(&help.stdout).contains("--ball-speed"));
            let rejected =
                std::process::Command::new(env!("CARGO_BIN_EXE_pong-node")).args(["host", "--ball-speed", "slow"]).output().unwrap();
            assert!(!rejected.status.success());
            assert!(String::from_utf8_lossy(&rejected.stderr).contains("unexpected argument '--ball-speed'"));
        }

        #[test]
        fn local_join_rejects_changed_financial_outputs_and_wrong_pace() {
            let a = artifact();
            let (invite, _, lobby) = fixture(&a);
            let draft = setup::prepare(&a, &invite, &lobby).unwrap();
            let mut altered = draft.clone();
            altered.transaction.outputs.last_mut().unwrap().value = 1;
            assert!(setup::validate(&a, &invite, &altered, 100_000).is_err());
            let mut wrong_pace = invite;
            wrong_pace.lobby.ball_tick_daa = 65;
            assert!(setup::prepare(&a, &wrong_pace, &lobby).is_err());
        }
    }

    mod visual_court {
        use super::*;
        fn dots(frame: &str) -> Vec<(usize, usize, char)> {
            frame
                .lines()
                .filter(|row| row.starts_with('|'))
                .enumerate()
                .flat_map(|(y, row)| {
                    row.chars().enumerate().filter_map(move |(x, ch)| ('\u{2801}'..='\u{28ff}').contains(&ch).then_some((x, y, ch)))
                })
                .collect()
        }

        #[test]
        fn braille_ball_moves_inside_cells_and_keeps_sixteen_visible_dots() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let mut subcell_motion = false;
            for daa in 155..190 {
                let render = |fractional_daa| {
                    ascii::court_frame_with_presentation(
                        s,
                        daa,
                        "",
                        "PONG",
                        "controls",
                        "footer",
                        ascii::CourtPresentation { columns: 128, fractional_daa, ..Default::default() },
                    )
                    .unwrap()
                };
                let first = dots(&render(0.0));
                let later = dots(&render(0.9));
                for ball in [&first, &later] {
                    assert_eq!(ball.iter().map(|(_, _, ch)| (*ch as u32 - 0x2800).count_ones()).sum::<u32>(), 16);
                    assert!(ball.iter().all(|(_, _, ch)| terminal::court_cell(*ch).1 == crossterm::style::Color::Yellow));
                }
                // A larger sprite can straddle another character when it moves
                // one dot. Compare masks in shared cells rather than demanding an
                // unchanged set of occupied character cells.
                if first.iter().any(|(x, y, ch)| later.iter().any(|(nx, ny, next)| x == nx && y == ny && ch != next)) {
                    subcell_motion = true;
                }
            }
            assert!(subcell_motion, "the ball must move between dot positions within the same character cells");
        }

        #[test]
        fn fractional_playback_stops_at_observed_time() {
            use std::time::{Duration, Instant};
            let now = Instant::now();
            let mut clock = network_ui::ObservedClock::default();
            clock.update(140, 100, false, now);
            assert_eq!(clock.sample(now + Duration::from_millis(50)), 135);
            assert!((clock.fractional(now + Duration::from_millis(50)) - 0.5).abs() < 0.001);
            assert_eq!(clock.sample(now + Duration::from_secs(3)), 140);
            assert_eq!(clock.fractional(now + Duration::from_secs(3)), 0.0);
        }

        #[test]
        fn wide_court_separates_playback_accepted_and_target_paddles() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let before = project(s, 155).unwrap().world;
            let frame = ascii::court_frame_with_presentation(
                s,
                155,
                "White: accepted | Red: target",
                "PONG",
                "controls",
                "footer",
                ascii::CourtPresentation { columns: 128, accepted_paddles: Some((20, 18)), preview: Some((0, 8)), ..Default::default() },
            )
            .unwrap();
            let rows: Vec<_> = frame.lines().filter(|r| r.starts_with('|')).collect();
            assert_eq!(rows.len(), 16);
            assert!(rows.iter().all(|r| r.chars().count() == 130));
            assert_eq!(rows[8].chars().nth(5), Some('█'));
            assert_eq!(rows[10].chars().nth(4), Some('░'), "accepted marker is outside the collision paddle");
            assert_eq!(rows[4].chars().nth(6), Some('▓'));
            assert_eq!(terminal::court_cell('▓'), ('█', crossterm::style::Color::Red));
            assert_eq!(terminal::court_cell('█'), ('█', crossterm::style::Color::White));
            assert_eq!(terminal::court_cell('░'), ('█', crossterm::style::Color::DarkGrey));
            assert!(frame.chars().any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch)));
            assert_eq!(project(s, 155).unwrap().world, before);
            let fitted = terminal::panel_body(&frame, 21);
            assert!(fitted.iter().any(|r| r.starts_with('|')));
            assert!(fitted.last().unwrap().contains("Red: target"));
        }
    }

    mod input_coalescing {
        use super::*;
        use std::{
            collections::VecDeque,
            time::{Duration, Instant},
        };

        fn inputs(directions: &[i64]) -> (network_ui::InputPreview, VecDeque<network_ui::Movement>) {
            let now = Instant::now();
            let mut preview = network_ui::InputPreview::default();
            let queue = directions
                .iter()
                .enumerate()
                .map(|(i, direction)| {
                    let movement = network_ui::Movement { direction: *direction, pressed: now + Duration::from_millis(i as u64) };
                    preview.push(movement.clone());
                    movement
                })
                .collect();
            (preview, queue)
        }

        #[test]
        fn opposite_unsent_inputs_cancel_without_moving_target_including_at_edges() {
            assert_eq!(DEFAULT_BALL_TICK_DAA, 7);
            for center in PADDLE_MIN_Y..=PADDLE_MAX_Y {
                for directions in [&[-1, 1][..], &[1, -1], &[-1, -1, 1, 1], &[1, -1, 1], &[-1, 1, 1, -1]] {
                    let (mut preview, mut queue) = inputs(directions);
                    let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
                    world.left_y = center;
                    let before = preview.paddle(&world, 0, &[], Instant::now()).0;
                    let removed = preview.cancel_unsent(&mut queue, &world, 0, &[]);
                    assert_eq!(preview.paddle(&world, 0, &[], Instant::now()).0, before);
                    assert_eq!(queue.len() + removed, directions.len());
                    if center == 16 && directions.len() == 2 {
                        assert_eq!(removed, 2);
                        assert!(queue.is_empty());
                    }
                }
            }
        }

        #[test]
        fn nonconsecutive_opposites_cancel_across_issued_commands_and_retries() {
            let world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
            for retry in [false, true] {
                let (mut preview, mut queue) = inputs(&[-1, 1, 1, -1, -1, 1]);
                let first_issued = queue[1].clone();
                let second_issued = queue[4].clone();
                preview.submitted(first_issued.pressed, hash(81));
                preview.submitted(second_issued.pressed, hash(82));
                if retry {
                    preview.requeue(&first_issued);
                    preview.requeue(&second_issued);
                } else {
                    queue.remove(4);
                    queue.remove(1);
                }
                let before = preview.paddle(&world, 0, &[], Instant::now()).0;
                assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 4);
                assert_eq!(preview.paddle(&world, 0, &[], Instant::now()), (before, 2));
                assert_eq!(queue.len(), if retry { 2 } else { 0 });
                if retry {
                    assert_eq!(queue[0].pressed, first_issued.pressed);
                    assert_eq!(queue[1].pressed, second_issued.pressed);
                }
            }
        }

        #[test]
        fn cancellation_leaves_no_neutral_unsent_pair_even_near_edges() {
            for center in [PADDLE_MIN_Y, PADDLE_MIN_Y + 1, 16, PADDLE_MAX_Y - 1, PADDLE_MAX_Y] {
                for pattern in 0..64 {
                    let directions = (0..6).map(|i| if pattern & (1 << i) == 0 { -1 } else { 1 }).collect::<Vec<_>>();
                    let (mut preview, mut queue) = inputs(&directions);
                    let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
                    world.left_y = center;
                    let original = preview.paddle(&world, 0, &[], Instant::now()).0;
                    preview.cancel_unsent(&mut queue, &world, 0, &[]);
                    assert_eq!(preview.paddle(&world, 0, &[], Instant::now()).0, original);
                    for i in 0..queue.len() {
                        for j in i + 1..queue.len() {
                            if queue[i].direction == -queue[j].direction {
                                let without = queue
                                    .iter()
                                    .enumerate()
                                    .filter(|(k, _)| *k != i && *k != j)
                                    .fold(center, |y, (_, movement)| (y + movement.direction).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y));
                                assert_ne!(without, original, "a cancellable pair was left behind");
                            }
                        }
                    }
                }
            }
        }

        #[test]
        fn issued_inputs_and_their_retries_are_never_cancelled() {
            let world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
            let (mut preview, mut queue) = inputs(&[-1, 1]);
            let issued = queue.pop_front().unwrap();
            preview.submitted(issued.pressed, hash(80));
            assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 0);
            assert_eq!(queue.len(), 1);
            assert_eq!(preview.paddle(&world, 0, &[], Instant::now()), (16, 2));
            preview.requeue(&issued);
            queue.push_front(issued);
            assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 0, "previously issued retries remain protected");
            assert_eq!(queue.len(), 2);
        }
    }

    mod playback_catchup {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn ten_second_stall_catches_up_at_most_twice_normal_then_returns_to_normal() {
            let now = Instant::now();
            let mut clock = network_ui::ObservedClock::default();
            clock.update(200, 0, false, now);
            let recovered = now + Duration::from_secs(10);
            let stopped = clock.sample(recovered);
            clock.update(300, 0, false, recovered);
            assert_eq!(clock.sample(recovered), stopped, "recovery must not jump playback");
            let mut previous = stopped;
            for step in 1..=250 {
                let at = recovered + Duration::from_millis(step * 100);
                let target = 300 + step;
                let current = clock.sample(at);
                assert!(current >= previous);
                assert!(current - previous <= 2, "catch-up must remain at or below 2x");
                clock.update(target, 0, false, at);
                assert_eq!(clock.sample(at), current, "fresh samples cannot teleport the ball");
                if step == 150 {
                    assert!(target - current <= 25, "large lag should clear in seconds, not linger for a minute");
                }
                previous = current;
            }
            assert!((5..=6).contains(&(550 - previous)));
            let at = recovered + Duration::from_secs(25);
            assert_eq!(clock.sample(at + Duration::from_secs(1)), 550, "a half-second reserve must stop at the observed tip if no new sample arrives");
        }

        #[test]
        fn speed_boost_stops_when_its_excess_delay_budget_is_consumed() {
            let now = Instant::now();
            let mut clock = network_ui::ObservedClock::default();
            clock.update(120, 0, false, now);
            clock.update(140, 0, false, now);
            assert_eq!(clock.sample(now), 115);
            assert_eq!(clock.sample(now + Duration::from_secs(1)), 135);
            assert_eq!(clock.sample(now + Duration::from_millis(1250)), 137, "exhausted boost returns to 1x even without another update");
            assert!((clock.fractional(now + Duration::from_millis(1250)) - 0.5).abs() < 0.001);
            assert_eq!(clock.sample(now + Duration::from_secs(20)), 140);
            assert_eq!(clock.fractional(now + Duration::from_secs(20)), 0.0);
        }
    }

    mod batched_movement {
        use super::*;
        use std::{
            collections::VecDeque,
            time::{Duration, Instant},
        };

        #[test]
        fn collection_wait_does_not_postpone_a_due_contact_proof() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            start(&a, &invite, &mut tracker, &lobby);
            let Stage::Live(s) = &tracker.stage else { panic!() };
            let (receiver, deadline) = next_contact(s).unwrap();
            let now = Instant::now();
            let movement = network_ui::Movement { direction: 1, pressed: now };
            let mut preview = network_ui::InputPreview::default();
            preview.push(movement.clone());
            let queue = std::collections::VecDeque::from([movement]);
            assert!(preview.ready_movement_batch(&queue, now).is_empty());
            assert_eq!(action(&tracker.stage, receiver as i64, deadline as u64, None).unwrap(), Some(Action::Resolve(false)));
        }

        fn queued(directions: &[i64], interval_ms: u64) -> (network_ui::InputPreview, VecDeque<network_ui::Movement>) {
            let mut preview = network_ui::InputPreview::default();
            let now = Instant::now();
            let queue = directions
                .iter()
                .enumerate()
                .map(|(i, direction)| {
                    let movement =
                        network_ui::Movement { direction: *direction, pressed: now + Duration::from_millis(i as u64 * interval_ms) };
                    preview.push(movement.clone());
                    movement
                })
                .collect();
            (preview, queue)
        }

        #[test]
        fn rapid_presses_batch_without_waiting_and_respect_burst_direction_and_limit() {
            for (directions, interval, expected) in
                [(&[1, 1, 1, 1, 1][..], 20, 5), (&[-1, -1, 1][..], 20, 2),
                 (&[1, 1][..], network_ui::MOVEMENT_BATCH_WINDOW.as_millis() as u64 + 1, 2), (&[1][..], 0, 1)]
            {
                let (preview, queue) = queued(directions, interval);
                assert_eq!(preview.movement_batch(&queue).len(), expected);
            }
            let (preview, queue) = queued(&[1; 30], 1);
            assert_eq!(preview.movement_batch(&queue).len(), 27);
        }

        #[test]
        fn batch_acceptance_reconciles_every_press_and_retry_preserves_original_group() {
            let (mut preview, queue) = queued(&[-1; 5], 20);
            let batch = preview.movement_batch(&queue);
            let mut delivery = network_ui::MovementDelivery::default();
            for movement in &batch {
                preview.submitted(movement.pressed, hash(84));
                delivery.submitted(movement.clone(), hash(84));
            }
            let retry = delivery.reconcile(&[], &[]);
            let mut retry_queue = VecDeque::new();
            for movement in retry {
                preview.requeue(&movement);
                retry_queue.push_back(movement);
            }
            assert_eq!(preview.movement_batch(&retry_queue).len(), 5);
            let later = network_ui::Movement { direction: -1, pressed: queue.back().unwrap().pressed + Duration::from_millis(1) };
            preview.push(later.clone());
            retry_queue.push_back(later);
            assert_eq!(preview.movement_batch(&retry_queue).len(), 5, "new inputs cannot join a previously issued batch");
            let mut world = World::opening(pubkey(&key(1)), pubkey(&key(2)));
            world.left_y = 11;
            for movement in &batch {
                preview.submitted(movement.pressed, hash(85));
            }
            assert_eq!(preview.paddle(&world, 0, &[hash(85)], Instant::now()), (10, 1));
        }

        #[test]
        fn tracker_and_launch_use_three_lanes_and_apply_five_steps_once() {
            let a = artifact();
            let (invite, mut tracker, lobby) = fixture(&a);
            let start_package = setup::prepare(&a, &invite, &lobby).unwrap();
            assert_eq!(start_package.transaction.outputs.len(), 7);
            assert!(start_package.transaction.storage_mass() <= 500_000);
            start(&a, &invite, &mut tracker, &lobby);
            let command = movement(&a, &tracker, 5, 92);
            tracker.apply(&a, &invite, &[], &[ChainBlock { hash: hash(12), daa: 160, transactions: vec![command] }]).unwrap();
            let Stage::Live(s) = &tracker.stage else { panic!() };
            assert_eq!(s.players.len(), 6);
            assert_eq!(s.players[0].state.count, 1);
            assert_eq!(s.players[0].state.commands[0], 5);
            assert_eq!(project(s, 159).unwrap().world.left_y, 16);
            assert_eq!(project(s, 160).unwrap().world.left_y, 21);
            assert_eq!(GAME_POT, 1_000_000_000);
            println!("seven-output start storage mass: {}", start_package.transaction.storage_mass());
        }
    }
}

mod invitations {
    //! Deterministic covenant/discovery tests. No private/user wallets or live nodes.
    use argent_runtime::{Artifact, EntryCall, TxBuilder, TxContext, args};
    use kaspa_consensus_core::{
        Hash,
        tx::{CovenantBinding, Transaction, TransactionOutpoint, UtxoEntry},
    };
    use kaspa_pong::{
        engine::*,
        node::{
            self, Config, invitations, setup,
            tracker::{AcceptedTx, ChainBlock, Invitation, Stage, Tracker},
        },
    };
    use secp256k1::{Keypair, Secp256k1, SecretKey};

    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    fn hash(n: u64) -> Hash {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        Hash::from_bytes(bytes)
    }
    fn artifact() -> Artifact {
        serde_json::from_str(include_str!("../artifacts/artifact.json")).unwrap()
    }
    fn coin(n: u64, payer: u8, amount: u64) -> Funding {
        Funding {
            outpoint: TransactionOutpoint::new(hash(n), 0),
            utxo: UtxoEntry::new(amount, p2pk(&pubkey(&key(payer))), 1, false, None),
        }
    }
    fn fixture(a: &Artifact) -> (Invitation, Funding, Cell<Lobby>) {
        let lobby = Lobby {
            left: pubkey(&key(1)),
            right: pubkey(&key(2)),
            refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64,
            ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE,
        };
        let funding = coin(90, 3, 2_000_000_000);
        let invite = invitations::create(a, "devnet", hash(0), &lobby, &funding, &key(1), &key(3), 100_000).unwrap();
        let accepted = accepted_cell(&invite.genesis, 0, 100, lobby);
        (invite, funding, accepted)
    }
    fn block(n: u64, daa: u64, tx: Transaction, inputs: Vec<UtxoEntry>) -> ChainBlock {
        ChainBlock { hash: hash(n), daa, transactions: vec![AcceptedTx { tx, inputs }] }
    }
    fn approved(a: &Artifact, i: &Invitation, l: &Cell<Lobby>, n: u64) -> setup::StartPackage {
        let c = Config { relay_fee_per_kg: 1, ..Default::default() };
        setup::approve(a, i, setup::prepare(a, i, l).unwrap(), vec![coin(n, 4, 600_000_000)], &key(2), &key(4), &c).unwrap()
    }
    fn tracker(i: &Invitation, l: &Cell<Lobby>) -> Tracker {
        let mut t = Tracker::new(i);
        t.cursor = hash(20);
        t.daa = 200;
        t.stage = Stage::Lobby(l.clone());
        t
    }

    #[test]
    fn authenticates_separate_player_and_funding_keys_and_exact_creation() {
        let a = artifact();
        let (i, f, l) = fixture(&a);
        i.validate(&a).unwrap();
        assert_eq!(i.genesis.payload.len(), 310 + "devnet".len(), "fixed binary format, not transaction JSON");
        assert_eq!(invitations::from_transaction(&a, &i.genesis).unwrap().lobby.state(), i.lobby.state());
        assert_eq!(invitations::creation_payment(&i).unwrap().sompi, 100_000);
        assert_eq!(invitations::creation_payment(&i).unwrap().payer, p2pk(&pubkey(&key(3))));
        assert!(invitations::create(&a, "devnet", hash(0), &i.lobby, &f, &key(5), &key(3), 100_000).is_err());
        assert!(invitations::create(&a, "devnet", hash(0), &i.lobby, &f, &key(1), &key(5), 100_000).is_err());
        let coins = [Funding { outpoint: l.outpoint, utxo: l.utxo.clone() }];
        assert_eq!(invitations::accepted_lobby(&a, &i, &coins).unwrap().outpoint, l.outpoint);
        for field in 0..5 {
            let mut altered = i.clone();
            match field {
                0 => altered.network = "testnet-10".into(),
                1 => altered.version += 1,
                2 => altered.lobby.right = pubkey(&key(5)),
                3 => altered.lobby.stake_per_player += 1,
                _ => altered.anchor = hash(999),
            }
            assert!(altered.validate(&a).is_err(), "altered metadata {field}");
        }
        for offset in [5, 10, 16, 48, 80, 144, 200, i.genesis.payload.len() - 1] {
            let mut altered = i.clone();
            altered.genesis.payload[offset] ^= 1;
            altered.genesis.finalize();
            assert!(altered.validate(&a).is_err(), "altered manifest {offset}");
        }
        let mut forged = i.clone();
        forged.genesis.inputs[0].previous_outpoint = coin(91, 3, 2_000_000_000).outpoint;
        forged.genesis.finalize();
        assert!(forged.validate(&a).is_err());
        let mut raw = i.clone();
        raw.genesis = genesis_tx(&TxBuilder::new(&a).unwrap(), &i.lobby, &f, &key(3), 100_000).unwrap();
        assert!(raw.validate(&a).is_err(), "unsigned player intent must not pass setup");
        assert!(setup::prepare(&a, &raw, &l).is_err());
    }

    #[test]
    fn codes_detect_typos_and_resolve_ambiguity_with_full_identity() {
        let id = hash(7);
        let code = invitations::code(id);
        assert_eq!(invitations::parse_code(&code.to_lowercase()).unwrap().len(), 12);
        let compact = code.replace('-', "");
        for index in 0..compact.len() {
            let mut typo = compact.as_bytes().to_vec();
            typo[index] = if typo[index] == b'0' { b'1' } else { b'0' };
            assert!(invitations::parse_code(std::str::from_utf8(&typo).unwrap()).is_err());
        }
        let mut bytes = id.as_bytes();
        bytes[31] ^= 1;
        let collision = Hash::from_bytes(bytes);
        assert_eq!(invitations::code(id), invitations::code(collision));
        assert!(invitations::resolve_code(&code, [id, collision]).unwrap_err().to_string().contains("ambiguous"));
        assert_eq!(invitations::resolve_code(&invitations::full_code(id), [id, collision]).unwrap(), id);
        assert_eq!(invitations::resolve_code(&code, [id, id]).unwrap(), id);
        assert!(invitations::resolve_code(&code, []).is_err());
    }

    #[test]
    fn guest_alone_funds_signs_and_vm_checks_stakes_change_and_fee() {
        let a = artifact();
        let (i, _, l) = fixture(&a);
        let p = approved(&a, &i, &l, 92);
        setup::validate(&a, &i, &p, 25_000_000).unwrap();
        for input in 0..2 {
            argent_runtime::execute_input_with_covenants(&p.transaction, p.inputs(), input).unwrap();
        }
        assert_eq!(p.transaction.inputs.len(), 2);
        assert_eq!(p.transaction.outputs[0].value, 400_000_000);
        assert!(p.transaction.outputs[1..7].iter().all(|o| o.value == 100_000_000));
        assert_eq!(p.transaction.outputs[7].value, 600_000_000 - 500_000_000 - p.fee);
        assert_eq!(p.transaction.outputs[7].script_public_key, p2pk(&pubkey(&key(4))));
        assert_eq!(node::client::transaction_fee(&p.transaction, &p.inputs()).unwrap(), p.fee);
        let payment = node::fees::Payment::from_transaction(&p.transaction, &p.inputs()).unwrap().unwrap();
        assert_eq!(payment.sompi, p.fee);
        assert_eq!(payment.payer, p2pk(&pubkey(&key(4))));
        let draft = setup::prepare(&a, &i, &l).unwrap();
        let config = Config::default();
        assert!(setup::approve(&a, &i, draft.clone(), vec![coin(93, 4, 600_000_000)], &key(5), &key(4), &config).is_err());
        assert!(setup::approve(&a, &i, draft, vec![coin(93, 4, 500_000_000)], &key(2), &key(4), &config).is_err());
        for field in 0..4 {
            let mut altered = p.clone();
            match field {
                0 => altered.transaction.outputs[0].value -= 1,
                1 => altered.transaction.outputs[1].value -= 1,
                2 => altered.transaction.outputs[7].script_public_key = p2pk(&pubkey(&key(5))),
                _ => altered.right_signature[64] = 0x84,
            }
            assert!(setup::validate(&a, &i, &altered, 25_000_000).is_err());
        }
    }

    #[test]
    fn covenant_independently_rejects_authorized_guest_altering_initial_world_or_deposits() {
        let a = artifact();
        let (_, _, l) = fixture(&a);
        let b = TxBuilder::new(&a).unwrap();
        for attack in 0..4 {
            let mut plan = l.state.opening();
            if attack == 0 {
                plan.world.right = pubkey(&key(5));
            }
            if attack == 1 {
                plan.world.clock = 1;
            }
            let id = l.utxo.covenant_id.unwrap();
            let funding = coin(92, 4, 600_000_000);
            let right = key(2);
            let payer = key(4);
            let mut ctx = TxContext::new()
                .actor_input(
                    "Lobby",
                    l.state.state(),
                    EntryCall::new("start").args_with(|tx, n| args![sign(tx, n, &right)]),
                    l.outpoint,
                    l.utxo.clone(),
                    0,
                )
                .actor_output("Court", plan.world.state(), CovenantBinding::new(0, id), 400_000_000 - u64::from(attack == 2));
            for (n, q) in plan.queues.iter().enumerate() {
                ctx = ctx.actor_output("Player", q.state(), CovenantBinding::new(0, id), 100_000_000 - u64::from(attack == 3 && n == 0));
            }
            let ctx = funded(ctx, &funding, &payer, 500_100_000).unwrap();
            assert!(build(&b, &ctx).is_err(), "covenant must reject fully re-signed attack {attack}");
        }
    }

    #[test]
    fn unspent_lookup_rejects_cancelled_spent_wrong_value_script_covenant_and_metadata() {
        let a = artifact();
        let (i, _, l) = fixture(&a);
        assert!(invitations::accepted_lobby(&a, &i, &[]).is_err());
        for attack in 0..5 {
            let mut coin = Funding { outpoint: l.outpoint, utxo: l.utxo.clone() };
            match attack {
                0 => coin.utxo.amount -= 1,
                1 => coin.utxo.script_public_key = p2pk(&pubkey(&key(5))),
                2 => coin.utxo.covenant_id = Some(hash(123)),
                3 => coin.utxo.block_daa_score = u64::MAX,
                _ => coin.outpoint.index = 1,
            }
            assert!(invitations::accepted_lobby(&a, &i, &[coin]).is_err());
        }
    }

    #[test]
    fn directory_reorgs_creation_and_spend_and_rolls_back_conflicting_joins() {
        let a = artifact();
        let (i, f, l) = fixture(&a);
        let mut index = invitations::Index::new("devnet", hash(0));
        let creation = block(10, 100, i.genesis.clone(), vec![f.utxo]);
        index.apply(&a, &[], &[creation.clone()]).unwrap();
        assert_eq!(index.lookup(&invitations::code(i.genesis.id())).unwrap().len(), 1);
        let first = approved(&a, &i, &l, 92);
        let second = approved(&a, &i, &l, 93);
        index.apply(&a, &[], &[block(11, 101, first.transaction.clone(), first.inputs())]).unwrap();
        assert!(index.lookup(&invitations::code(i.genesis.id())).unwrap().is_empty());
        index.apply(&a, &[hash(11)], &[]).unwrap();
        assert_eq!(index.lookup(&invitations::code(i.genesis.id())).unwrap().len(), 1);
        let mut t = tracker(&i, &l);
        t.apply(&a, &i, &[], &[block(11, 101, first.transaction.clone(), first.inputs())]).unwrap();
        t.apply(&a, &i, &[hash(11)], &[block(12, 102, second.transaction.clone(), second.inputs())]).unwrap();
        let Stage::Live(live) = &t.stage else { panic!("replacement join should be live") };
        assert_eq!(live.court.outpoint.transaction_id, second.transaction.id());
        assert!(!t.accepted(first.transaction.id()));
        index.apply(&a, &[hash(10)], &[]).unwrap();
        assert!(index.records.is_empty());
    }

    #[test]
    fn cold_bootstrap_survives_tip_reorg_and_retimes_creation_before_join() {
        let a = artifact();
        let (i, f, l) = fixture(&a);
        let mut t = tracker(&i, &l);
        t.rewind_empty_history(&i, hash(19));
        assert!(matches!(t.stage, Stage::Lobby(_)), "older accepted Lobby must survive a tip-only rollback");
        let p = approved(&a, &i, &l, 92);
        t.apply(&a, &i, &[], &[block(21, 201, p.transaction.clone(), p.inputs())]).unwrap();
        assert!(matches!(t.stage, Stage::Live(_)));
        let mut retimed = tracker(&i, &l);
        retimed.rewind_empty_history(&i, hash(0));
        let new_lobby = accepted_cell(&i.genesis, 0, 150, i.lobby.clone());
        let p = approved(&a, &i, &new_lobby, 93);
        retimed
            .apply(&a, &i, &[], &[block(30, 150, i.genesis.clone(), vec![f.utxo]), block(31, 151, p.transaction.clone(), p.inputs())])
            .unwrap();
        assert!(matches!(retimed.stage, Stage::Live(_)));
    }

    #[test]
    fn directory_undo_is_bounded_and_deep_reorg_fails_atomically() {
        let a = artifact();
        let mut index = invitations::Index::new("devnet", hash(0));
        let blocks: Vec<_> =
            (1..=invitations::UNDO_LIMIT as u64 + 3).map(|n| ChainBlock { hash: hash(n), daa: n, transactions: vec![] }).collect();
        index.apply(&a, &[], &blocks).unwrap();
        assert_eq!(serde_json::to_value(&index).unwrap()["history"].as_array().unwrap().len(), invitations::UNDO_LIMIT);
        let before = serde_json::to_vec(&index).unwrap();
        let removed: Vec<_> = blocks.iter().rev().map(|b| b.hash).collect();
        assert!(index.apply(&a, &removed, &[]).is_err());
        assert_eq!(serde_json::to_vec(&index).unwrap(), before);
    }

    #[tokio::test]
    async fn late_empty_cache_guest_uses_real_http_with_separate_directories_during_chain_progress() {
        let a = artifact();
        let (i, f, _) = fixture(&a);
        let host = tempfile::tempdir().unwrap();
        let guest = tempfile::tempdir().unwrap();
        let mut index = invitations::Index::new("devnet", hash(0));
        index.apply(&a, &[], &[block(10, 100, i.genesis.clone(), vec![f.utxo])]).unwrap();
        let path = host.path().join("directory.json");
        node::save(&path, &index).unwrap();
        let mut recovered: invitations::Index = node::load(&path).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config { invitation_index: Some(format!("http://{}", listener.local_addr().unwrap())), ..Default::default() };
        let server_artifact = a.clone();
        let server = std::thread::spawn(move || {
            // Every batch advances. HTTP servicing must never require an empty delta.
            for n in 11..=13 {
                recovered.apply(&server_artifact, &[], &[ChainBlock { hash: hash(n), daa: 100 + n, transactions: vec![] }]).unwrap();
                assert_eq!(invitations::serve_requests(&server_artifact, &recovered, &listener, 1).unwrap(), 1);
            }
        });
        for _ in 0..3 {
            let code = format!(" \n{}\t ", invitations::code(i.genesis.id()).to_lowercase());
            let found = invitations::discover(&a, &config, &pubkey(&key(2)), &code).await.unwrap();
            assert_eq!(found.genesis.id(), i.genesis.id());
        }
        server.join().unwrap();
        assert!(std::fs::read_dir(guest.path()).unwrap().next().is_none(), "guest never receives a shared file/cache");
    }

    #[test]
    fn no_implicit_directory_and_wrong_network_or_guest_never_authorize() {
        let a = artifact();
        let (i, _, _) = fixture(&a);
        let code = invitations::code(i.genesis.id());
        assert!(invitations::configured(&Config::default()).is_err());
        assert!(invitations::select(&a, "testnet-10", &pubkey(&key(2)), &code, vec![i.genesis.clone()]).is_err());
        assert!(invitations::select(&a, "devnet", &pubkey(&key(5)), &code, vec![i.genesis]).is_err());
    }

    #[tokio::test]
    async fn guest_lookup_waits_for_indexing_but_rejects_wrong_player() {
        let a = artifact();
        let (i, f, _) = fixture(&a);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config { invitation_index: Some(format!("http://{}", listener.local_addr().unwrap())), ..Default::default() };
        let server_artifact = a.clone();
        let genesis = i.genesis.clone();
        let server = std::thread::spawn(move || {
            let mut index = invitations::Index::new("devnet", hash(0));
            invitations::serve_requests(&server_artifact, &index, &listener, 1).unwrap();
            index.apply(&server_artifact, &[], &[block(10, 100, genesis, vec![f.utxo])]).unwrap();
            for _ in 0..2 { invitations::serve_requests(&server_artifact, &index, &listener, 1).unwrap(); }
        });
        let code = invitations::code(i.genesis.id());
        assert!(invitations::discover_pending(&a, &config, &pubkey(&key(2)), &code).await.unwrap().is_none());
        let found = invitations::discover_pending(&a, &config, &pubkey(&key(2)), &code).await.unwrap().unwrap();
        assert_eq!(found.genesis.id(), i.genesis.id());
        assert!(invitations::discover_pending(&a, &config, &pubkey(&key(5)), &code).await.is_err());
        server.join().unwrap();
    }

    #[test]
    fn guest_start_reserves_sources_and_charges_external_fee_budget_once() {
        let a = artifact();
        let (i, _, l) = fixture(&a);
        let p = approved(&a, &i, &l, 92);
        let mut cache = node::client::Cache::new(&i);
        cache.tracker = tracker(&i, &l);
        let config = Config { fee_budget_sompi: p.fee - 1, ..Default::default() };
        assert!(p.enqueue(&mut cache, &pubkey(&key(2)), &config).is_err());
        assert!(cache.pending.is_empty());
        assert_eq!(cache.authorized_fees, 0);
        let config = Config::default();
        assert!(p.enqueue(&mut cache, &pubkey(&key(2)), &config).unwrap());
        assert_eq!(cache.authorized_fees, p.fee);
        assert_eq!(cache.pending[0].inputs, p.inputs());
        assert!(!p.enqueue(&mut cache, &pubkey(&key(2)), &config).unwrap());
        assert_eq!(cache.authorized_fees, p.fee);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("guest-state.json");
        node::save(&state, &cache).unwrap();
        let restored: node::client::Cache = node::load(&state).unwrap();
        restored.validate(&a, &i).unwrap();
        assert_eq!(restored.pending[0].transaction.id(), p.transaction.id());
        assert_eq!(restored.authorized_fees, p.fee);
    }

    #[test]
    fn cancellation_refunds_exact_host_principal_and_races_same_lobby() {
        let a = artifact();
        let (i, f, l) = fixture(&a);
        let b = TxBuilder::new(&a).unwrap();
        let refund = key(3);
        let external = coin(95, 3, 100_000_000);
        let ctx = TxContext::new()
            .actor_input(
                "Lobby",
                l.state.state(),
                EntryCall::new("cancel").args_with(|tx, n| args![sign(tx, n, &refund)]),
                l.outpoint,
                l.utxo.clone(),
                0,
            )
            .output(p2pk(&pubkey(&refund)), None, l.utxo.amount);
        let cancel = build(&b, &funded(ctx, &external, &refund, 100_000).unwrap()).unwrap();
        assert_eq!(cancel.outputs[0].value, 500_000_000);
        let inputs = vec![l.utxo.clone(), external.utxo];
        argent_runtime::execute_input_with_covenants(&cancel, inputs.clone(), 0).unwrap();
        let mut index = invitations::Index::new("devnet", hash(0));
        index.apply(&a, &[], &[block(10, 100, i.genesis.clone(), vec![f.utxo]), block(11, 101, cancel.clone(), inputs.clone())]).unwrap();
        assert!(index.lookup(&invitations::code(i.genesis.id())).unwrap().is_empty());
        let mut t = tracker(&i, &l);
        t.apply(&a, &i, &[], &[block(11, 101, cancel, inputs)]).unwrap();
        assert!(matches!(t.stage, Stage::Cancelled));
        let p = approved(&a, &i, &l, 92);
        t.apply(&a, &i, &[hash(11)], &[block(12, 102, p.transaction.clone(), p.inputs())]).unwrap();
        assert!(matches!(t.stage, Stage::Live(_)), "join wins replacement branch; cancelled branch was reverted");
    }

    #[test]
    fn http_accepts_fragmented_request_line_with_bounded_input() {
        use std::io::{Read, Write};
        let a = artifact();
        let (i, f, _) = fixture(&a);
        let mut index = invitations::Index::new("devnet", hash(0));
        index.apply(&a, &[], &[block(10, 100, i.genesis.clone(), vec![f.utxo])]).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || invitations::serve_requests(&a, &index, &listener, 1).unwrap());
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        stream.write_all(b"GE").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write!(stream, "T /lookup/{} HTTP/1.0\r\n\r\n", invitations::code(i.genesis.id())).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.0 200 "));
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn hosted_expiry_and_refund_preserve_principal_authority_and_reorgs() {
        use kaspa_pong::node::hosted;
        assert!(!hosted::expired(100, 99));
        assert!(!hosted::expired(100, 100 + hosted::EXPIRY_MS - 1));
        assert!(hosted::expired(100, 100 + hosted::EXPIRY_MS));
        let a = artifact();
        let (i, _, l) = fixture(&a);
        let fee = coin(777, 3, 100_000_000);
        assert!(hosted::cancel_transaction(&a, &l, &key(2), &fee, &key(3), 100_000).is_err());
        let tx = hosted::cancel_transaction(&a, &l, &key(3), &fee, &key(3), 100_000).unwrap();
        assert_eq!(tx.outputs[0].value, l.utxo.amount);
        assert_eq!(tx.outputs[0].script_public_key, p2pk(&i.lobby.refund));
        let inputs = vec![l.utxo.clone(), fee.utxo.clone()];
        for tamper in [0, 1] {
            let mut bad = tx.clone();
            if tamper == 0 { bad.outputs[0].value -= 1; }
            else { bad.outputs[0].script_public_key = p2pk(&pubkey(&key(2))); }
            assert!(argent_runtime::execute_input_with_covenants(&bad, inputs.clone(), 0).is_err());
        }
        let mut cache = node::client::Cache::new(&i);
        cache.tracker = tracker(&i, &l);
        cache.authorize(tx.clone(), inputs.clone(), "unjoined invitation cancel".into(), &Config::default()).unwrap();
        let restored: node::client::Cache = serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();
        restored.validate(&a, &i).unwrap();
        assert_eq!(restored.pending[0].transaction.id(), tx.id());
        assert_eq!(restored.pending[0].transaction.inputs[1].previous_outpoint, fee.outpoint);
        let mut t = restored.tracker;
        t.apply(&a, &i, &[], &[block(21, 201, tx, inputs)]).unwrap();
        assert!(matches!(t.stage, Stage::Cancelled));
        let join = approved(&a, &i, &l, 778);
        t.apply(&a, &i, &[hash(21)], &[block(22, 202, join.transaction.clone(), join.inputs())]).unwrap();
        assert!(matches!(t.stage, Stage::Live(_)), "a competing accepted join is never cancelled as a live game");
    }
}

mod rpc {
    //! Explicit, isolated native-node scenarios. Never use funded user wallets.
    use argent_runtime::{TxBuilder, TxContext};
    use kaspa_pong::{engine::*, network_ui, node};
    use node::{Config, rpc::Node};
    use node::{
        client::{Action, Cache},
        setup,
        tracker::{Invitation, Stage},
    };
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use std::{
        process::{Child, Command, Stdio},
        time::Duration,
    };
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    struct Native {
        dir: tempfile::TempDir,
        n: Node,
        config: Config,
        wrpc_config: Config,
        _server: Server,
    }
    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }
    fn artifact() -> argent_runtime::Artifact {
        node::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts/artifact.json"))).unwrap()
    }
    fn client_binary() -> String {
        std::env::var("PONG_CLIENT_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_pong-node").into())
    }
    async fn native() -> Native {
        let binary = std::env::var("PONG_KASPAD_BIN").expect("set PONG_KASPAD_BIN to the pinned kaspad executable");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new().prefix("pong-rpc-").tempdir_in(root).unwrap();
        let mut blockrate = kaspa_consensus_core::config::params::DEVNET_PARAMS.blockrate;
        blockrate.coinbase_maturity = 0;
        let overrides = dir.path().join("params.json");
        std::fs::write(
            &overrides,
            serde_json::to_vec(
                &serde_json::json!({"skip_proof_of_work":true,"crescendo_activation":0,"toccata_activation":0,"blockrate":blockrate}),
            )
            .unwrap(),
        )
        .unwrap();
        let rpc_port = port();
        let wrpc_port = port();
        let p2p_port = port();
        let log = std::fs::File::create(dir.path().join("node.log")).unwrap();
        let mut process = Command::new(binary);
        process
            .args(["--devnet", "--utxoindex", "--nodnsseed", "--outpeers=0", "--enable-unsynced-mining"])
            .arg(format!("--rpclisten=127.0.0.1:{rpc_port}"))
            .arg(format!("--rpclisten-borsh=127.0.0.1:{wrpc_port}"))
            .arg(format!("--listen=127.0.0.1:{p2p_port}"))
            .arg(format!("--appdir={}", dir.path().join("data").display()))
            .arg(format!("--override-params-file={}", overrides.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x08000000);
        }
        let mut server = Server(process.spawn().unwrap());
        let config = Config {
            endpoint: format!("grpc://127.0.0.1:{rpc_port}"),
            allow_unsynced: true,
            coinbase_maturity_daa: 0,
            fee_sompi: 25_000_000,
            ..Config::default()
        };
        let wrpc_config = Config { endpoint: format!("ws://127.0.0.1:{wrpc_port}"), ..config.clone() };
        let mut connected = None;
        for _ in 0..80 {
            if let Ok(node) = Node::connect(&config).await {
                connected = Some(node);
                break;
            }
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!("node exited {status}: {}", std::fs::read_to_string(dir.path().join("node.log")).unwrap());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let n = connected.expect("node did not start");
        let wrong_network = Config { network: "testnet-10".into(), ..wrpc_config.clone() };
        let error = Node::connect(&wrong_network).await.err().expect("wrong-network node must be rejected");
        assert!(error.to_string().contains("differs from configured"));
        if !n.server_info().await.unwrap().is_synced {
            let strict = Config { allow_unsynced: false, ..wrpc_config.clone() };
            let error = Node::connect(&strict).await.err().expect("unsynchronized candidate must be rejected");
            assert!(error.to_string().contains("not synchronized"));
        }
        let fallback = Config {
            endpoint: "resolver".into(),
            resolver_urls: vec![format!("http://127.0.0.1:{}", port())],
            fallback_endpoints: vec![wrpc_config.endpoint.clone()],
            ..config.clone()
        };
        let recovered = Node::connect(&fallback).await.expect("unavailable resolver must fall back to configured Borsh node");
        assert_eq!(recovered.candidate, 1);
        assert_eq!(recovered.current_endpoint(), wrpc_config.endpoint);
        recovered.rpc.disconnect().await.unwrap();
        Native { dir, n, config, wrpc_config, _server: server }
    }

    async fn mine(n: &Node, address: kaspa_addresses::Address, nonce: &mut u64) {
        let before = n.info().await.unwrap().sink;
        let mut block = n.rpc.get_block_template(address, vec![]).await.unwrap().block;
        *nonce += 1;
        block.header.nonce = *nonce;
        n.rpc.submit_block(block, true).await.unwrap();
        for _ in 0..100 {
            if n.info().await.unwrap().sink != before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("submitted block did not advance virtual sink");
    }

    #[tokio::test]
    #[ignore = "requires PONG_KASPAD_BIN; launches a disposable local devnet"]
    async fn rpc_acceptance_submission_and_reconnect() {
        let Native { dir, n, config, wrpc_config, _server } = native().await;
        let mut nonce = 0;
        let pay = n.address(&p2pk(&pubkey(&key(3)))).unwrap();
        // Slow transaction work must not stop independent clock observations.
        let frontend = network_ui::Frontend::new(false, false, 0, true, "devnet").unwrap();
        let epoch = std::cell::Cell::new(0);
        let before = n.info().await.unwrap().virtual_daa_score;
        {
            let delayed_work = async {
                let builder = tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(600)));
                for _ in 0..5 {
                    mine(&n, pay.clone(), &mut nonce).await;
                    tokio::time::sleep(Duration::from_millis(90)).await;
                }
                builder.await.unwrap();
            };
            tokio::pin!(delayed_work);
            tokio::select! {
                _ = &mut delayed_work => {},
                _ = frontend.follow_clock(&n, &epoch) => unreachable!(),
            }
        }
        let sample = frontend.latest_clock().expect("clock feed must remain active during blocked transaction work");
        assert!(sample.daa > before, "ball clock must advance while transaction processing is pending");
        println!("live clock passed: new node DAA received during delayed transaction work");
        for _ in 0..6 {
            mine(&n, pay.clone(), &mut nonce).await;
        }
        let guest_pay = n.address(&p2pk(&pubkey(&key(2)))).unwrap();
        for _ in 0..3 {
            mine(&n, guest_pay.clone(), &mut nonce).await;
        }
        let a = artifact();
        let b = TxBuilder::new(&a).unwrap();
        let lobby = Lobby {
            left: pubkey(&key(1)),
            right: pubkey(&key(2)),
            refund: pubkey(&key(3)),
            control_value: SIDE_DEPOSIT as i64,
            ball_tick_daa: DEFAULT_BALL_TICK_DAA,
            stake_per_player: DEFAULT_STAKE,
        };
        let fund = n.funding(&pubkey(&key(3)), DEFAULT_STAKE as u64 + 1 + config.fee_sompi, 0).await.unwrap();
        let invite = node::invitations::create(&a, "devnet", n.info().await.unwrap().sink, &lobby, &fund,
            &key(1), &key(3), config.fee_sompi).unwrap();
        let genesis = invite.genesis.clone();
        let mut cache = Cache::new(&invite);
        if !n.server_info().await.unwrap().is_synced {
            let strict = Config { allow_unsynced: false, ..config.clone() };
            let error = cache.sync(&a, &invite, &n, &strict).await.err().expect("loss of sync must trigger peer rotation");
            assert!(error.to_string().contains("lost synchronization"));
            assert_eq!(cache.tracker.cursor, invite.anchor, "failed peer must not commit a new snapshot");
        }
        n.submit(&genesis).await.unwrap();
        for _ in 0..8 {
            mine(&n, pay.clone(), &mut nonce).await;
            let (ready, _) = cache.sync(&a, &invite, &n, &config).await.unwrap();
            if ready && matches!(cache.tracker.stage, Stage::Lobby(_)) {
                break;
            }
        }
        let Stage::Lobby(lobby) = cache.tracker.stage.clone() else { panic!("accepted lobby not observed") };
        let fund = n.funding(&pubkey(&key(2)), DEFAULT_STAKE as u64 + config.fee_sompi + 1, 0).await.unwrap();
        let package = setup::prepare(&a, &invite, &lobby).unwrap();
        let package = setup::approve(&a, &invite, package, vec![fund], &key(2), &key(2), &config).unwrap();
        n.submit(&package.transaction).await.unwrap();
        for _ in 0..8 {
            mine(&n, pay.clone(), &mut nonce).await;
            let (ready, _) = cache.sync(&a, &invite, &n, &config).await.unwrap();
            if ready && matches!(cache.tracker.stage, Stage::Live(_)) {
                break;
            }
        }
        let Stage::Live(s) = cache.tracker.stage.clone() else { panic!("start not observed") };
        assert_eq!(s.court.state.resolves, 0);
        let fund = n.funding(&pubkey(&key(3)), config.fee_sompi + 1, 0).await.unwrap();
        let (tx, _) = command_tx(&b, &s.players[0], 1, &key(1), &fund, &key(3), config.fee_sompi).unwrap();
        n.submit(&tx).await.unwrap();
        // A submission acknowledgment must not mutate accepted state.
        let Stage::Live(before) = &cache.tracker.stage else { panic!() };
        assert_eq!(before.players[0].state.count, 0);
        assert!(!cache.transaction_log_pending.iter().any(|r| r.event.txid == tx.id()));
        for _ in 0..8 {
            mine(&n, pay.clone(), &mut nonce).await;
            let (ready, _) = cache.sync(&a, &invite, &n, &config).await.unwrap();
            if ready && cache.tracker.accepted(tx.id()) {
                break;
            }
        }
        let Stage::Live(s) = cache.tracker.stage.clone() else { panic!() };
        assert_eq!(s.players[0].state.count, 1);
        assert!(
            cache.transaction_log_pending.iter().any(|r| r.event.txid == tx.id() && r.status == node::audit::TransactionStatus::Accepted)
        );
        let (tx, _, _, _claim) =
            node::client::build_action(&a, &n, &cache, &key(1), &key(3), Action::Checkpoint(0, cache.tracker.daa as i64), &config)
                .await
                .unwrap();
        n.submit(&tx).await.unwrap();
        for _ in 0..8 {
            mine(&n, pay.clone(), &mut nonce).await;
            if cache.sync(&a, &invite, &n, &config).await.unwrap().0 && cache.tracker.accepted(tx.id()) {
                break;
            }
        }
        let Stage::Live(s) = &cache.tracker.stage else { panic!() };
        assert_eq!(s.players[0].state.y, 17);
        let guest = Node::connect(&config).await.unwrap();
        let mut guest_cache = Cache::new(&invite);
        assert!(guest_cache.sync(&a, &invite, &guest, &config).await.unwrap().0);
        let (tx, entries, _, _claim) =
            node::client::build_action(&a, &guest, &guest_cache, &key(2), &key(2), Action::Move(-1), &config).await.unwrap();
        assert_eq!(node::client::transaction_fee(&tx, &entries).unwrap(), node::client::relay_fee(&tx, &config).unwrap());
        guest.submit(&tx).await.unwrap();
        for _ in 0..8 {
            mine(&n, pay.clone(), &mut nonce).await;
            guest_cache.sync(&a, &invite, &guest, &config).await.unwrap();
            cache.sync(&a, &invite, &n, &config).await.unwrap();
            if guest_cache.tracker.accepted(tx.id()) && cache.tracker.accepted(tx.id()) {
                break;
            }
        }
        let Stage::Live(host_state) = &cache.tracker.stage else { panic!() };
        let wrpc = Node::connect(&wrpc_config).await.unwrap();
        let mut wrpc_cache = Cache::new(&invite);
        assert!(wrpc_cache.sync(&a, &invite, &wrpc, &wrpc_config).await.unwrap().0);
        let Stage::Live(wrpc_state) = &wrpc_cache.tracker.stage else { panic!() };
        assert_eq!(wrpc_state.court.outpoint, host_state.court.outpoint);
        wrpc.rpc.disconnect().await.unwrap();
        // Exercise the same compiled client used by the launcher.
        {
            let client_binary = client_binary();
            use std::io::{BufRead, Write};
            let config_file = dir.path().join("client-config.json");
            let invite_file = dir.path().join("invitation.json");
            let state_file = dir.path().join("client-state.json");
            let (stalled, _, _, _stalled_claim) =
                node::client::build_action(&a, &guest, &guest_cache, &key(2), &key(2), Action::Move(1), &config).await.unwrap();
            guest.submit(&stalled).await.unwrap();
            for _ in 0..8 {
                mine(&n, pay.clone(), &mut nonce).await;
                guest_cache.sync(&a, &invite, &guest, &config).await.unwrap();
                if guest_cache.tracker.accepted(stalled.id()) {
                    break;
                }
            }
            let Stage::Live(waiting) = &guest_cache.tracker.stage else { panic!() };
            assert_eq!(waiting.players.iter().filter(|p| p.state.side == 1).map(|p| p.state.count).sum::<usize>(), 2);
            let (receiver, deadline) = next_contact(waiting).unwrap();
            assert!(guest_cache.tracker.daa < deadline as u64);
            let (early, _, _, _early_claim) =
                node::client::build_action(&a, &guest, &guest_cache, &key(2), &key(2), Action::Checkpoint(receiver, deadline), &config)
                    .await
                    .unwrap();
            assert!(guest.submit(&early).await.is_err(), "node must reject a checkpoint before its contact deadline");
            while n.info().await.unwrap().virtual_daa_score + 5 < deadline as u64 {
                mine(&n, pay.clone(), &mut nonce).await;
            }
            guest_cache.sync(&a, &invite, &guest, &config).await.unwrap();
            node::save(&config_file, &wrpc_config).unwrap();
            node::save(&invite_file, &invite).unwrap();
            node::save(&state_file, &guest_cache).unwrap();
            let wallet_file = dir.path().join("client.wallet.json");
            node::wallet::Wallet { player: key(2) }.save_new(&wallet_file).unwrap();
            let mut process = Command::new(client_binary);
            process
                .arg("--artifact")
                .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts/artifact.json"))
                .arg("--config")
                .arg(&config_file)
                .arg("--wallet")
                .arg(&wallet_file)
                .args(["run", "--json", "--invitation"])
                .arg(&invite_file)
                .arg("--state")
                .arg(&state_file)
                .env_remove("PONG_PLAYER_KEY")
                .env_remove("PONG_FEE_KEY")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(std::fs::File::create(dir.path().join("client.log")).unwrap()));
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                process.creation_flags(0x08000000);
            }
            let mut client = Server(process.spawn().unwrap());
            let stdout = client.0.stdout.take().unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        if send.send(value).is_err() {
                            break;
                        }
                    }
                }
            });
            let first = loop {
                let value = receive.recv_timeout(Duration::from_secs(10)).expect("CLI did not publish synchronized state");
                if value.get("ready").is_some() {
                    break value;
                }
            };
            assert_eq!(first["ready"], true);
            // No keyboard input and no left process: the right client must take
            // over automatically when consensus permits it, then stop idle spending.
            let mut recovered = false;
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                mine(&n, pay.clone(), &mut nonce).await;
                cache.sync(&a, &invite, &n, &config).await.unwrap();
                if let Stage::Live(s) = &cache.tracker.stage {
                    if s.court.state.resolves >= 1 {
                        recovered = true;
                        break;
                    }
                }
            }
            assert!(recovered, "client did not automatically resolve after opponent timeout");
            // The independent observer may see acceptance before the player does.
            // Wait for the client's own synchronized acknowledgement before typing.
            let until = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let update = receive.recv_timeout(Duration::from_secs(2)).expect("client stopped publishing after takeover");
                if update.get("ready").is_none() {
                    continue;
                }
                let stage = serde_json::from_value::<Stage>(update["accepted"].clone()).unwrap();
                if update["ready"] == true
                    && update["pending"].as_array().is_some_and(|p| p.is_empty())
                    && matches!(stage, Stage::Live(s) if s.court.state.resolves >= 1)
                {
                    break;
                }
                assert!(std::time::Instant::now() < until, "client did not reconcile its own takeover: {}", update["status"]);
            }
            let previous_moves = cache
                .tracker
                .history
                .iter()
                .flat_map(|h| h.events.iter())
                .filter(|e| e.action == "move" && e.side == Some(1))
                .map(|e| e.txid)
                .collect::<std::collections::HashSet<_>>();
            client.0.stdin.as_mut().unwrap().write_all(b"w\n").unwrap();
            client.0.stdin.as_mut().unwrap().flush().unwrap();
            let mut observed = false;
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                mine(&n, pay.clone(), &mut nonce).await;
                cache.sync(&a, &invite, &n, &config).await.unwrap();
                if let Stage::Live(_) = &cache.tracker.stage {
                    if cache
                        .tracker
                        .history
                        .iter()
                        .flat_map(|h| h.events.iter())
                        .any(|e| e.action == "move" && e.side == Some(1) && e.direction == Some(-1) && !previous_moves.contains(&e.txid))
                    {
                        observed = true;
                        break;
                    }
                }
            }
            assert!(
                observed,
                "CLI movement must become an accepted transaction; last status: {:?}",
                receive.try_iter().last().map(|v| v["status"].clone())
            );
            client.0.stdin.as_mut().unwrap().write_all(b"\x1b\n").unwrap();
            client.0.stdin.as_mut().unwrap().flush().unwrap();
            let mut exit = None;
            for _ in 0..40 {
                if let Some(status) = client.0.try_wait().unwrap() {
                    exit = Some(status);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(exit.is_some_and(|s| s.success()), "CLI should quit promptly and restore its durable cache");
            reader.join().unwrap();
            let persisted: Cache = node::load(&state_file).unwrap();
            assert!(persisted.authorized_fees > 0);
            assert!(persisted.authorized_fees < config.fee_budget_sompi);
            let records: Vec<node::audit::TransactionRecord> = std::fs::read_to_string(node::audit::transaction_log_path(&state_file))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(records.iter().any(|r| r.event.action == "resolve" && r.status == node::audit::TransactionStatus::Accepted));
            assert!(records.iter().any(|r| r.event.action == "move" && r.event.side == Some(1) && r.event.direction == Some(-1)));
            assert!(records.iter().all(|r| r.accepting_block.is_some() && r.event.accepted_daa.is_some()));
            println!(
                "actual CLI passed: early checkpoint rejected, automatic contact without rival, keyboard command accepted on chain, clean quit and fee persistence"
            );
        }
        guest.rpc.disconnect().await.unwrap();
        let file = dir.path().join("cache.json");
        node::save(&file, &cache).unwrap();
        n.rpc.disconnect().await.unwrap();
        let mut restored: Cache = node::load(&file).unwrap();
        restored.validate(&a, &invite).unwrap();
        let alternate = Config { fallback_endpoints: vec![wrpc_config.endpoint.clone()], ..config.clone() };
        let second = Node::connect_from(&alternate, 1).await.unwrap();
        assert_eq!(second.candidate, 1);
        assert_eq!(second.current_endpoint(), wrpc_config.endpoint);
        assert!(restored.sync(&a, &invite, &second, &config).await.unwrap().0);
        assert_eq!(restored.tracker.cursor, cache.tracker.cursor);
        second.rpc.disconnect().await.unwrap();
        println!(
            "actual node passed: resolver failure fallback, network/sync checks, signed game transactions and cache reconnect through another endpoint"
        );
    }

    #[tokio::test]
    #[ignore = "requires PONG_KASPAD_BIN; launches a disposable local devnet"]
    async fn host_join_parallel_input_queues() {
        let Native { dir, n, config, wrpc_config, _server } = native().await;
        let mut nonce = 0;
        let pay = n.address(&p2pk(&pubkey(&key(3)))).unwrap();
        let a = artifact();
        {
            let client_binary = client_binary();
            use std::io::Write;
            // Exercise explicit named wallets in their own working directory.
            // There is no third player process to prepare/sign/broadcast the start.
            let home = dir.path().join("simple-launch");
            std::fs::create_dir_all(&home).unwrap();
            let guest_home = dir.path().join("independent-guest");
            std::fs::create_dir_all(&guest_home).unwrap();
            let index_port = port();
            let index_url = format!("http://127.0.0.1:{index_port}");
            let host_config = Config { invitation_index: Some(index_url.clone()), ..wrpc_config.clone() };
            let guest_config = Config { invitation_index: Some(index_url), ..config.clone() };
            node::save(&home.join("node.json"), &host_config).unwrap();
            node::save(&guest_home.join("node.json"), &guest_config).unwrap();
            node::wallet::Wallet { player: key(4) }.save_new(&home.join("left.wallet.json")).unwrap();
            node::wallet::Wallet { player: key(5) }.save_new(&guest_home.join("right.wallet.json")).unwrap();
            for side in [4, 5] {
                let address = n.address(&p2pk(&pubkey(&key(side)))).unwrap();
                mine(&n, address, &mut nonce).await;
            }
            mine(&n, pay.clone(), &mut nonce).await;
            // Fragmented-wallet regression: the small leftover used to be selected
            // first and create a 516064-mass start, above the 500000 limit.
            let tiny_amount = 25_958_800;
            let efficient_amount = 571_093_000;
            // A freshly mined coinbase is accepted by a later block, not by its
            // own header. Give the sponsor an accepted output before splitting.
            mine(&n, pay.clone(), &mut nonce).await;
            mine(&n, pay.clone(), &mut nonce).await;
            let sponsor = n.funding(&pubkey(&key(3)), tiny_amount + efficient_amount + config.fee_sompi, 0).await.unwrap();
            let split = node::client::priced_transaction(&config, |fee| {
                let ctx = TxContext::new().output(p2pk(&pubkey(&key(4))), None, tiny_amount).output(
                    p2pk(&pubkey(&key(4))),
                    None,
                    efficient_amount,
                );
                build(&TxBuilder::new(&a)?, &funded(ctx, &sponsor, &key(3), tiny_amount + efficient_amount + fee)?)
            })
            .unwrap();
            n.submit(&split).await.unwrap();
            let efficient_outpoint = kaspa_consensus_core::tx::TransactionOutpoint::new(split.id(), 1);
            let mut split_accepted = false;
            for _ in 0..12 {
                mine(&n, pay.clone(), &mut nonce).await;
                split_accepted = n.utxos(&[p2pk(&pubkey(&key(4)))]).await.unwrap().iter().any(|f| f.outpoint == efficient_outpoint);
                if split_accepted {
                    break;
                }
            }
            assert!(split_accepted);
            let artifact_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts/artifact.json");
            let spawn = |label: &str, args: &[String]| {
                let mut cmd = Command::new(&client_binary);
                cmd.current_dir(if label.starts_with("join") { &guest_home } else { &home })
                    .arg("--artifact")
                    .arg(&artifact_path)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(std::fs::File::create(home.join(format!("{label}.stdout"))).unwrap())
                    .stderr(std::fs::File::create(home.join(format!("{label}.stderr"))).unwrap());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                Server(cmd.spawn().unwrap())
            };
            let _index = spawn("index", &["--config".into(), home.join("node.json").display().to_string(),
                "index".into(), "--state".into(), "invitation-index.json".into(), "--listen".into(), format!("127.0.0.1:{index_port}")]);
            let mut index_ready = false;
            for _ in 0..100 {
                index_ready = std::net::TcpStream::connect(("127.0.0.1", index_port)).is_ok();
                if index_ready { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(index_ready, "invitation directory failed to listen");
            let mut host = spawn(
                "host",
                &["--player".into(), "left".into(), "host".into(), key(5).x_only_public_key().0.to_string(), "--json".into()],
            );
            let mut offer = None;
            for _ in 0..120 {
                assert!(
                    host.0.try_wait().unwrap().is_none(),
                    "host failed: {}",
                    std::fs::read_to_string(home.join("host.stderr")).unwrap()
                );
                mine(&n, pay.clone(), &mut nonce).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Ok(games) = std::fs::read_dir(home.join("games")) {
                    offer = games.filter_map(|entry| entry.ok()).map(|entry| entry.path().join("invitation.json")).find(|path| path.exists());
                }
                if offer.is_some() {
                    break;
                }
            }
            let offer = offer.expect("host did not record its public creation transaction");
            let public: Invitation = node::load(&offer).unwrap();
            public.validate(&a).unwrap();
            let game_code = node::invitations::code(public.genesis.id());
            let mut indexed = false;
            for _ in 0..80 {
                mine(&n, pay.clone(), &mut nonce).await;
                if node::invitations::discover(&a, &guest_config, &pubkey(&key(5)), &game_code).await.is_ok() { indexed = true; break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(indexed, "accepted invitation was not indexed");

            assert_eq!(public.lobby.ball_tick_daa, DEFAULT_BALL_TICK_DAA);
            let game_dir = guest_home.join("games").join(public.genesis.id().to_string());
            let host_game_dir = offer.parent().unwrap();
            let mut joiner =
                spawn("join", &["--player".into(), "right".into(), "join".into(), game_code, "--json".into()]);
            let read_cache = |side: &str| node::load::<Cache>(&(if side == "left" { host_game_dir.to_path_buf() } else { game_dir.clone() }).join(format!("{side}-state.json"))).ok();
            // Hold the chain still past the start's retry check. The guest must
            // find this HOST-funded transaction in the mempool and retain it.
            let mut waiting = None;
            let start_deadline = std::time::Instant::now() + Duration::from_secs(20);
            while std::time::Instant::now() < start_deadline {
                assert!(
                    joiner.0.try_wait().unwrap().is_none(),
                    "guest exited during start: {}",
                    std::fs::read_to_string(home.join("join.stderr")).unwrap()
                );
                if let Some(c) = read_cache("right") {
                    if let Some(p) = c.pending.iter().find(|p| p.purpose == "guest start") {
                        if n.pending_exists(p.transaction.id(), &pubkey(&key(5))).await.unwrap() {
                            waiting = Some(p.transaction.id());
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let waiting = waiting.unwrap_or_else(|| {
                panic!(
                    "guest never submitted the signed start; binary={}\nstderr={}\noutput={}\ncache pending={:?}",
                    client_binary,
                    std::fs::read_to_string(home.join("join.stderr")).unwrap(),
                    std::fs::read_to_string(home.join("join.stdout")).unwrap(),
                    read_cache("right").map(|c| c
                        .pending
                        .iter()
                        .map(|p| (p.purpose.clone(), p.attempted_ms, p.branch_changed))
                        .collect::<Vec<_>>())
                )
            });
            assert!(!n.pending_exists(waiting, &pubkey(&key(4))).await.unwrap(), "host fee address does not index guest-funded join");
            tokio::time::sleep(Duration::from_secs(6)).await;
            assert!(
                read_cache("right").unwrap().pending.iter().any(|p| p.transaction.id() == waiting),
                "host-funded start was incorrectly forgotten while still in the mempool"
            );
            let mut started = false;
            for _ in 0..120 {
                assert!(
                    host.0.try_wait().unwrap().is_none(),
                    "host failed: {}",
                    std::fs::read_to_string(home.join("host.stderr")).unwrap()
                );
                assert!(
                    joiner.0.try_wait().unwrap().is_none(),
                    "join failed: {}",
                    std::fs::read_to_string(home.join("join.stderr")).unwrap()
                );
                mine(&n, pay.clone(), &mut nonce).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                if ["left", "right"].iter().all(|side| read_cache(side).is_some_and(|c| matches!(c.tracker.stage, Stage::Live(_)))) {
                    started = true;
                    break;
                }
            }
            assert!(
                started,
                "host/join did not start: host stderr={}\nguest stderr={}\nguest output={}",
                std::fs::read_to_string(home.join("host.stderr")).unwrap(),
                std::fs::read_to_string(home.join("join.stderr")).unwrap(),
                std::fs::read_to_string(home.join("join.stdout")).unwrap().lines().rev().take(2).collect::<Vec<_>>().join("\n")
            );
            let guest_before = read_cache("right").unwrap();
            assert!(guest_before.authorized_fees > 0, "guest authorizes and funds the start fee");
            assert!(guest_before.fee_ledger.paid_by(&p2pk(&pubkey(&key(5)))) > 0);
            let host_paid = read_cache("left").unwrap();
            let genesis_fee =
                host_paid.fee_ledger.payments.get(&public.genesis.id()).expect("lobby fee must be recorded").sompi;
            assert_eq!(
                host_paid.fee_ledger.paid_by(&p2pk(&pubkey(&key(4)))),
                genesis_fee as u128
            );
            assert!(host_paid.fee_ledger.complete);
            // No blocks are mined while this burst is submitted. This proves the
            // real client fills three lanes without waiting for command acceptance,
            // even when all three fees descend from the same wallet change output.
            stop(&mut host).await;
            for _ in 0..QUEUES_PER_PLAYER {
                joiner.0.stdin.as_mut().unwrap().write_all(b"w\n").unwrap();
                // Separate bursts so this test still exercises every parallel lane.
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
            let mut burst = None;
            for _ in 0..180 {
                assert!(joiner.0.try_wait().unwrap().is_none(), "joiner exited during burst");
                if let Some(c) = read_cache("right") {
                    if c.pending.len() == QUEUES_PER_PLAYER {
                        let mut all_submitted = true;
                        for p in &c.pending {
                            all_submitted &= n.pending_exists(p.transaction.id(), &pubkey(&key(5))).await.unwrap();
                        }
                        if all_submitted {
                            burst = Some(c);
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let burst = burst.expect("three commands must reach the mempool before any is accepted");
            burst.validate(&a, &public).unwrap();
            let lanes = burst.pending.iter().map(|p| p.transaction.inputs[0].previous_outpoint).collect::<std::collections::HashSet<_>>();
            assert_eq!(lanes.len(), QUEUES_PER_PLAYER);
            let Stage::Live(before_acceptance) = &burst.tracker.stage else { panic!() };
            assert!(before_acceptance.players.iter().all(|p| p.state.count == 0));
            // The fourth key waits only until a lane frees, then appends there.
            joiner.0.stdin.as_mut().unwrap().write_all(b"w\n").unwrap();
            let mut queued = false;
            for _ in 0..80 {
                queued = std::fs::read_to_string(home.join("join.stdout"))
                    .unwrap()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(|v| v["queued_inputs"].as_u64().is_some_and(|n| n > 0));
                if queued {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(queued, "fourth key must be queued before the test advances the chain");
            let mut moved = false;
            let mut appended = false;
            for _ in 0..120 {
                mine(&n, pay.clone(), &mut nonce).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Some(c) = read_cache("right") {
                    if let Stage::Live(s) = &c.tracker.stage {
                        appended |= s.players.iter().any(|p| p.state.count == 2);
                    }
                }
                let log = std::fs::read_to_string(game_dir.join("right-state.transactions.jsonl")).unwrap_or_default();
                moved = log
                    .lines()
                    .filter_map(|line| serde_json::from_str::<node::audit::TransactionRecord>(line).ok())
                    .filter(|r| {
                        r.status == node::audit::TransactionStatus::Accepted && r.event.action == "move" && r.event.side == Some(1)
                    })
                    .count()
                    >= QUEUES_PER_PLAYER + 1;
                if moved {
                    break;
                }
            }
            assert!(
                moved,
                "the fourth command must proceed after a lane frees; guest errors: {}",
                std::fs::read_to_string(home.join("join.stderr")).unwrap()
            );
            assert!(appended, "fourth command must append to an accepted lane, rather than requiring resolution");
            println!("three-lane burst passed: three mempool commands before mining, then fourth command accepted");
            async fn stop(client: &mut Server) {
                client.0.stdin.as_mut().unwrap().write_all(b"\x1b\n").unwrap();
                for _ in 0..60 {
                    if let Some(status) = client.0.try_wait().unwrap() {
                        assert!(status.success());
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("player client failed to quit");
            }
            stop(&mut joiner).await;
            let final_cache = read_cache("right").unwrap();
            assert!(final_cache.fee_ledger.paid_by(&p2pk(&pubkey(&key(5)))) > 0);
            assert!(!home.join("left.session.json").exists() && !home.join("right.session.json").exists());
            let Stage::Live(s) = &final_cache.tracker.stage else { panic!() };
            assert_eq!(s.court.state.ball_tick_daa, DEFAULT_BALL_TICK_DAA);
            println!("host/join passed: fixed game pace, three unaccepted lanes, fourth append, no resume/session files");
            // Quit and start a second fresh match with the same named wallets.
            let mut host2 = spawn(
                "host2",
                &["--player".into(), "left".into(), "host".into(), key(5).x_only_public_key().0.to_string(), "--json".into()],
            );
            let mut offer2 = None;
            for _ in 0..120 {
                assert!(
                    host2.0.try_wait().unwrap().is_none(),
                    "second host failed: {}",
                    std::fs::read_to_string(home.join("host2.stderr")).unwrap()
                );
                mine(&n, pay.clone(), &mut nonce).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                offer2 = std::fs::read_dir(home.join("games"))
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().join("invitation.json"))
                    .find(|p| *p != offer && p.exists());
                if offer2.is_some() {
                    break;
                }
            }
            let offer2 = offer2.expect("second host did not create a new invitation");
            let public2: Invitation = node::load(&offer2).unwrap();
            let code2 = node::invitations::code(public2.genesis.id());
            let mut indexed = false;
            for _ in 0..80 {
                mine(&n, pay.clone(), &mut nonce).await;
                if node::invitations::discover(&a, &guest_config, &pubkey(&key(5)), &code2).await.is_ok() { indexed = true; break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(indexed);
            let mut join2 =
                spawn("join2", &["--player".into(), "right".into(), "join".into(), code2, "--json".into()]);
            let second_dir = offer2.parent().unwrap();
            let mut started2 = false;
            for _ in 0..120 {
                assert!(
                    join2.0.try_wait().unwrap().is_none(),
                    "second guest failed: {}",
                    std::fs::read_to_string(home.join("join2.stderr")).unwrap()
                );
                mine(&n, pay.clone(), &mut nonce).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                started2 = ["left", "right"].iter().all(|side| {
                    let directory = if *side == "left" { second_dir.to_path_buf() } else { guest_home.join("games").join(public2.genesis.id().to_string()) };
                    node::load::<Cache>(&directory.join(format!("{side}-state.json")))
                        .is_ok_and(|c| matches!(c.tracker.stage, Stage::Live(_)))
                });
                if started2 {
                    break;
                }
            }
            assert!(started2, "second match stalled after quitting the first");
            stop(&mut host2).await;
            stop(&mut join2).await;
            println!("start regression passed: delayed host-funded start retained; same wallets start again after quit");
        }
        n.rpc.disconnect().await.unwrap();
    }
}
