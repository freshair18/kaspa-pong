//! Menu/gameplay controls, result perspective, and persistent goal notices.

mod menu {

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use kaspa_pong::{
        engine::GameSpeed,
        menu::{self, Action, Prompt},
        node::invitations,
        terminal::{PromptKey, PromptKeyFilter},
    };
    use std::{io::{BufRead, Cursor, Write}, process::{Command, Stdio}, time::{Duration, Instant}};

    fn key(ch: char, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(ch), KeyModifiers::NONE, kind)
    }

    fn host_code() -> String {
        let secret = secp256k1::SecretKey::from_slice(&[7; 32]).unwrap();
        let key = secp256k1::Keypair::from_secret_key(&secp256k1::SECP256K1, &secret);
        let mut tag = [0; invitations::RENDEZVOUS_BYTES];
        tag[..6].fill(4);
        invitations::rendezvous_code(&key.x_only_public_key().0.serialize(), &tag).unwrap()
    }

    #[test]
    fn top_menu_commits_options_and_quit_without_enter() {
        assert_eq!(menu::action_key(PromptKey::Character('1')), Some(Prompt::Selected(Action::Host)));
        assert_eq!(menu::action_key(PromptKey::Character('2')), Some(Prompt::Selected(Action::Join)));
        assert_eq!(menu::action_key(PromptKey::Character('3')), Some(Prompt::Selected(Action::ConfigureSpeed)));
        for key in [PromptKey::Character('q'), PromptKey::Character('Q'), PromptKey::Back] {
            assert_eq!(menu::action_key(key), Some(Prompt::Back));
        }
        assert_eq!(menu::action_key(PromptKey::Exit), Some(Prompt::Exit));
        for key in [PromptKey::Enter, PromptKey::Backspace] {
            assert_eq!(menu::action_key(key), None);
        }
    }

    #[test]
    fn game_code_text_requires_a_complete_checksummed_locator() {
        let code = host_code();
        let old_code = invitations::code(kaspa_consensus_core::Hash::from_bytes([7; 32]));
        let mut corrupted = code.clone();
        corrupted.pop();
        corrupted.push('!');
        let mut output = Vec::new();
        assert_eq!(menu::code(&mut Cursor::new(format!("\n1\nbad-code\n{old_code}\n{corrupted}\n{code}\n")), &mut output).unwrap(), Some(code));
        assert!(String::from_utf8(output).unwrap().contains("Game code"));
    }

    #[test]
    fn game_code_prompt_cancels_or_reaches_eof_without_selecting_a_match() {
        for answer in ["q\n", "\u{1b}\n", "", "bad-code\n"] {
            assert_eq!(menu::code(&mut Cursor::new(answer), &mut Vec::new()).unwrap(), None);
        }
    }

    #[test]
    fn windows_press_repeats_cannot_cross_screens_or_choose_a_funded_offer() {
        let now = Instant::now();
        let mut keys = PromptKeyFilter::new(true);
        let first = keys.filter(key('2', KeyEventKind::Press), now, false).unwrap();
        assert_eq!(menu::action_key(first), Some(Prompt::Selected(Action::Join)));
        keys.begin_screen();
        for (millis, kind) in [(10, KeyEventKind::Press), (1000, KeyEventKind::Press), (1050, KeyEventKind::Repeat)] {
            assert_eq!(keys.filter(key('2', kind), now + Duration::from_millis(millis), false), None);
        }
        assert_eq!(keys.filter(key('2', KeyEventKind::Release), now + Duration::from_secs(2), false), None);
        let fresh = keys.filter(key('2', KeyEventKind::Press), now + Duration::from_secs(3), false).unwrap();
        assert_eq!(fresh, PromptKey::Character('2'));

        // Gameplay-to-menu q/Escape transitions are covered through the actual
        // gameplay decoder in controls::ignored_game_q_and_exit_escape_cannot_select_the_next_menu.
    }

    #[test]
    fn text_edits_allow_repeats_but_block_the_incoming_menu_key() {
        let now = Instant::now();
        let mut host = PromptKeyFilter::new(true);
        host.filter(key('1', KeyEventKind::Press), now, false);
        host.begin_screen();
        assert_eq!(host.filter(key('1', KeyEventKind::Press), now, true), None, "held Host must not enter the public key draft");
        host.filter(key('1', KeyEventKind::Release), now, true);
        assert_eq!(host.filter(key('1', KeyEventKind::Press), now, true), Some(PromptKey::Character('1')));
        let mut keys = PromptKeyFilter::new(true);
        keys.filter(key('p', KeyEventKind::Press), now, false);
        keys.begin_screen();
        assert_eq!(keys.filter(key('p', KeyEventKind::Press), now, true), None);
        keys.filter(key('p', KeyEventKind::Release), now, true);
        assert_eq!(keys.filter(key('r', KeyEventKind::Press), now, true), Some(PromptKey::Character('r')));
        assert_eq!(keys.filter(key('r', KeyEventKind::Press), now, true), Some(PromptKey::Character('r')));
        assert_eq!(keys.filter(key('r', KeyEventKind::Repeat), now, true), Some(PromptKey::Character('r')));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(keys.filter(enter, now, true), Some(PromptKey::Enter));
        assert_eq!(keys.filter(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), now, true), Some(PromptKey::Back));
        assert_eq!(keys.filter(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), now, true), Some(PromptKey::Exit));

        let mut legacy = PromptKeyFilter::new(false);
        legacy.filter(key('2', KeyEventKind::Press), now, false);
        legacy.begin_screen();
        for millis in [100, 600, 1200] {
            assert_eq!(legacy.filter(key('2', KeyEventKind::Press), now + Duration::from_millis(millis), false), None);
        }
        assert_eq!(legacy.filter(key('2', KeyEventKind::Press), now + Duration::from_millis(2000), false), Some(PromptKey::Character('2')));
    }

    #[test]
    fn redirected_input_keeps_readline_codes_public_keys_and_eof() {
        let mut input = Cursor::new("2\nq\n");
        assert_eq!(menu::action(&mut input, &mut Vec::new(), GameSpeed::Medium).unwrap(), Some(Action::Join));
        let mut remaining = String::new();
        input.read_line(&mut remaining).unwrap();
        assert_eq!(remaining, "q\n", "readline consumes one complete answer");
        assert_eq!(menu::action(&mut Cursor::new(""), &mut Vec::new(), GameSpeed::Medium).unwrap(), None);
        let dir = tempfile::tempdir().unwrap();
        let code = host_code();
        assert_eq!(menu::code(&mut Cursor::new(format!("{code}\n")), &mut Vec::new()).unwrap(), Some(code));
        assert_eq!(menu::opponent(&mut Cursor::new("q\n"), &mut Vec::new()).unwrap(), None);

        // A pipe must never create a raw terminal/stdin-draining worker. No wallet
        // exists here; cancellation must finish before any wallet load or network.
        for (answer, expected) in [("q\n", "Host a new game"), ("2\nq\n", "Game code"), ("1\nq\n", "Paste opponent's public key"), ("", "Host a new game")] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_pong-node"))
                .args(["--wallet", "must-not-read.wallet.json"])
                .current_dir(dir.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(answer.as_bytes()).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
            assert!(!output.stdout.contains(&0x1b), "redirected output must not contain terminal controls");
        }
    }
}

mod controls {

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use kaspa_pong::{
        menu::{self, Prompt},
        terminal::{GameplayCommand, PromptKey, PromptKeyFilter, gameplay_key, gameplay_line},
    };
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    #[test]
    fn only_escape_exits_gameplay_and_q_remains_a_menu_choice() {
        for ch in ['q', 'Q', '\u{3}', '\u{4}', '\u{1a}'] {
            for kind in [KeyEventKind::Press, KeyEventKind::Repeat, KeyEventKind::Release] {
                assert_eq!(gameplay_key(key(KeyCode::Char(ch), kind)), None);
            }
            assert_eq!(gameplay_line(&ch.to_string()), None);
        }
        for ch in ['c', 'd', 'z', 'q'] {
            assert_eq!(gameplay_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)), None);
        }
        assert_eq!(gameplay_key(key(KeyCode::Esc, KeyEventKind::Press)), Some(GameplayCommand::Exit));
        assert_eq!(gameplay_key(key(KeyCode::Esc, KeyEventKind::Repeat)), None);
        assert_eq!(gameplay_key(key(KeyCode::Esc, KeyEventKind::Release)), None);
        assert_eq!(gameplay_line("\u{1b}\n"), Some(GameplayCommand::Exit));
        for ch in ['q', 'Q'] {
            assert_eq!(menu::action_key(PromptKey::Character(ch)), Some(Prompt::Back));
        }
    }

    #[test]
    fn movement_preserves_press_and_repeat_events_and_notice_dismissal() {
        let now = Instant::now();
        let mut keys = PromptKeyFilter::new(true);
        for (code, command) in [
            (KeyCode::Up, GameplayCommand::Up),
            (KeyCode::Char('w'), GameplayCommand::Up),
            (KeyCode::Down, GameplayCommand::Down),
            (KeyCode::Char('S'), GameplayCommand::Down),
        ] {
            for kind in [KeyEventKind::Press, KeyEventKind::Press, KeyEventKind::Repeat] {
                assert_eq!(keys.gameplay(key(code, kind), now), Some(command));
            }
            assert_eq!(keys.gameplay(key(code, KeyEventKind::Release), now), None);
        }
        assert_eq!(gameplay_line("up"), Some(GameplayCommand::Up));
        assert_eq!(gameplay_line("down"), Some(GameplayCommand::Down));
        assert_eq!(gameplay_key(key(KeyCode::Char('r'), KeyEventKind::Press)), Some(GameplayCommand::DismissNotice));
    }

    #[test]
    fn sound_toggle_ignores_held_key_repeats_and_requires_a_new_press() {
        let now = Instant::now();
        let mut keys = PromptKeyFilter::new(true);
        let code = KeyCode::Char('m');
        assert_eq!(keys.gameplay(key(code, KeyEventKind::Press), now), Some(GameplayCommand::ToggleSound));
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            assert_eq!(keys.gameplay(key(code, kind), now), None);
        }
        keys.gameplay(key(code, KeyEventKind::Release), now);
        assert_eq!(keys.gameplay(key(code, KeyEventKind::Press), now), Some(GameplayCommand::ToggleSound));
        assert_eq!(gameplay_line("M"), Some(GameplayCommand::ToggleSound));
    }

    #[test]
    fn ignored_game_q_and_exit_escape_cannot_select_the_next_menu() {
        let now = Instant::now();
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut keys = PromptKeyFilter::new(true);
            let command = keys.gameplay(key(code, KeyEventKind::Press), now);
            assert_eq!(command, if code == KeyCode::Esc { Some(GameplayCommand::Exit) } else { None });
            keys.begin_screen();
            for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
                assert_eq!(keys.filter(key(code, kind), now + Duration::from_secs(1), false), None);
            }
            keys.filter(key(code, KeyEventKind::Release), now, false);
            let fresh = keys.filter(key(code, KeyEventKind::Press), now, false).unwrap();
            assert_eq!(menu::action_key(fresh), Some(Prompt::Back));
        }
    }

    #[test]
    fn incoming_menu_keys_require_release_before_gameplay_action() {
        let now = Instant::now();
        for (code, command) in [(KeyCode::Esc, GameplayCommand::Exit), (KeyCode::Char('w'), GameplayCommand::Up)] {
            let mut keys = PromptKeyFilter::new(true);
            keys.filter(key(code, KeyEventKind::Press), now, false);
            keys.begin_screen();
            assert_eq!(keys.gameplay(key(code, KeyEventKind::Press), now), None);
            assert_eq!(keys.gameplay(key(code, KeyEventKind::Repeat), now), None);
            assert_eq!(keys.gameplay(key(code, KeyEventKind::Release), now), None);
            assert_eq!(keys.gameplay(key(code, KeyEventKind::Press), now), Some(command));
        }
    }
}

mod results {

    use kaspa_pong::{ascii, engine::{DEFAULT_STAKE, Finish}, network_ui::{self, Spending}, terminal::ScreenFrame};

    fn result(winner: i64, left_score: i64, right_score: i64) -> Finish {
        Finish { stake_per_player: DEFAULT_STAKE, left_score, right_score, winner, end_daa: 200 }
    }

    fn render(result: &Finish, side: Option<i64>, status: &str, spending: Option<&Spending>) -> String {
        // Deliberately misleading title: outcome must use the explicit perspective.
        network_ui::completed_frame(result, side, "PONG | YOU: RIGHT", status, spending, "TKAS")
    }

    #[test]
    fn both_score_winners_and_losers_see_their_own_outcome() {
        for winner in 0..=1 {
            let result = if winner == 0 { result(winner, 3, 1) } else { result(winner, 1, 3) };
            let side_name = if winner == 0 { "LEFT" } else { "RIGHT" };
            for local in 0..=1 {
                let frame = render(&result, Some(local), "Projected result; on-chain finalization is still pending", None);
                let (expected, absent) = if local == winner { ("You won!", "You lost!") } else { ("You lost!", "You won!") };
                assert!(frame.contains(expected));
                assert!(!frame.contains(absent));
                assert!(frame.contains(&format!("{side_name} wins by score.")));
                assert!(!frame.contains("timeout") && !frame.contains("timed out"));
                assert!(frame.contains("Projected result; on-chain finalization is still pending"));
                assert!(!frame.contains("accepted on chain"));
                assert!(frame.contains("Your network fee total is unavailable"));
                assert!(!frame.contains("Observer:"));
                assert!(frame.contains("Total goals: 4") && frame.contains("End DAA: 200"));
            }
        }
    }

    #[test]
    fn timeout_messages_follow_the_recorded_winner_even_when_trailing_on_score() {
        for winner in 0..=1 {
            let result = if winner == 0 { result(winner, 0, 2) } else { result(winner, 2, 0) };
            let side_name = if winner == 0 { "LEFT" } else { "RIGHT" };
            let won = render(&result, Some(winner), "Outcome accepted on chain; pot payout pending.", None);
            let lost = render(&result, Some(1 - winner), "Outcome accepted on chain; pot payout pending.", None);
            assert!(won.contains("You won! Your opponent's response timed out (50 DAA)."));
            assert!(lost.contains("You lost! Your response timed out (50 DAA)."));
            for frame in [&won, &lost] {
                assert!(frame.contains(&format!("{side_name} wins.")));
                assert!(frame.contains("Total goals: 2"));
                assert!(frame.contains("pot payout pending."));
                assert!(!frame.contains("Payouts accepted"));
                assert!(frame.contains("Winner pot: 10 KAS to the winning player address."));
                assert!(!frame.contains("collateral"));
            }
        }
    }

    #[test]
    fn observers_and_generic_ascii_results_are_neutral() {
        for winner in 0..=1 {
            let side_name = if winner == 0 { "LEFT" } else { "RIGHT" };
            let scored = if winner == 0 { result(winner, 3, 1) } else { result(winner, 1, 3) };
            let timeout = result(winner, 1, 1);
            for (result, expected) in [
                (&scored, format!("{side_name} wins by score.")),
                (&timeout, format!("{side_name} wins by response timeout (50 DAA).")),
            ] {
                for frame in [render(result, None, "", None), ascii::completed(result, "PONG", "", "Esc quits.")] {
                    assert!(frame.contains(&expected));
                    assert!(!frame.contains("You won") && !frame.contains("You lost"));
                    assert!(!frame.contains("Your response") && !frame.contains("Your opponent"));
                }
            }
        }
        assert!(render(&result(0, 3, 1), None, "", None).contains("Observer: no personal fee total"));
        let spending = Spending { paid: 100_000_000, pending: 0, complete: true };
        let observer = render(&result(0, 3, 1), None, "", Some(&spending));
        assert!(observer.contains("Observer: no personal fee total"));
        assert!(!observer.contains("Your network fees paid:"));
        let invalid_side = render(&result(0, 3, 1), Some(2), "", None);
        assert!(!invalid_side.contains("You won") && !invalid_side.contains("You lost"));
    }

    #[test]
    fn invalid_winner_results_do_not_award_a_personal_win_or_loss() {
        for side in [Some(0), Some(1), None] {
            // The protocol currently has no draw variant. A winnerless generic
            // result must still render neutrally, without inventing a payout.
            let draw = render(&result(-2, 1, 1), side, "", None);
            assert!(draw.contains("Draw. No winner recorded."));
            assert!(draw.contains("No payout allocation recorded."));
            let unknown = render(&result(2, 2, 1), side, "", None);
            assert!(unknown.contains("No winner recorded."));
            for frame in [&draw, &unknown] {
                assert!(!frame.contains("You won") && !frame.contains("You lost"));
                assert!(!frame.contains("Winner pot:") && !frame.contains("timed out"));
            }
        }
    }

    #[test]
    fn fees_settlement_and_escape_help_remain_visible_with_diagnostic_panels() {
        let result = result(0, 0, 2);
        let spending = Spending { paid: 123_456_789, pending: 34_567, complete: true };
        for side in [Some(0), Some(1)] {
            for status in [
                "Projected result; on-chain finalization is still pending",
                "Outcome accepted on chain; pot payout pending.",
                "Payouts accepted on chain.",
            ] {
                let frame = render(&result, side, status, Some(&spending));
                let screen = ScreenFrame::compose(&frame, Some("ACCEPTED ACTION\ntxid"), Some("REORG OCCURRED\nbranch changed\nR acknowledges"), (80, 25));
                let visible = screen.rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>().join("\n");
                for expected in [
                    if side == Some(0) { "You won!" } else { "You lost!" },
                    "Your network fees paid: 1.23456789 TKAS",
                    "Pending fees (not spent yet): 0.00034567 TKAS",
                    "Winner pot: 10 KAS to the winning player address.",
                    "Fees exclude stakes.",
                    "Esc: main menu; no automatic refund.",
                    status,
                ] {
                    assert!(visible.contains(expected), "missing {expected:?} in {visible}");
                }
                assert!(!visible.contains("q:") && !visible.contains("Q:"));
                if !status.starts_with("Payouts") {
                    assert!(!visible.contains("Payouts accepted"));
                }
            }
        }
        let partial = Spending { complete: false, ..spending };
        assert!(render(&result, Some(0), "", Some(&partial)).contains("Known fees paid (partial history): 1.23456789 TKAS"));
    }
}

mod goal_notices {
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{
        ascii::{CourtEffects, CourtPresentation, GoalVisual, GOAL_NOTICE_HOLD, court_frame_with_presentation},
        engine::*,
    };
    use std::time::{Duration, Instant};

    fn fixture(post: i64) -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true;
        world.clock = 100;
        world.serve_until = 100;
        world.ball_x = if post == 0 { LEFT_CONTACT_X + 1 } else { RIGHT_CONTACT_X - 1 };
        world.ball_vx = if post == 0 { -1 } else { 1 };
        world.ball_y = 6;
        let queues = empty_queues(&world.left, &world.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: queues[i].clone() }),
        }
    }

    fn observe(effects: &mut CourtEffects, snapshot: &Snapshot, daa: u64, now: Instant) {
        effects.observe(snapshot, &project(snapshot, daa).unwrap().world, daa, now);
    }

    fn paint(effects: &mut CourtEffects, snapshot: &Snapshot, daa: u64, now: Instant) -> String {
        observe(effects, snapshot, daa, now);
        court_frame_with_presentation(
            snapshot, daa, "STATUS RETAINED", "PONG", "Esc: menu", "footer",
            CourtPresentation {
                impact: effects.sample(daa, 0.0, now),
                goal_notice: effects.goal_notice(now),
                ..Default::default()
            },
        ).unwrap()
    }

    fn ball(frame: &str) -> Vec<(usize, usize, char)> {
        frame.lines().skip(4).take(16).enumerate().flat_map(|(row, line)| {
            line.chars().enumerate().filter_map(move |(column, ch)| {
                ('\u{2801}'..='\u{28ff}').contains(&ch).then_some((row, column, ch))
            })
        }).collect()
    }

    #[test]
    fn goal_highlight_reflects_at_the_wall_before_reaching_either_screen_edge() {
        for post in 0..=1 {
            for (start, slope, expected) in [(30.0, 2, 28.82), (1.0, -2, 2.18)] {
                let flight = GoalVisual { x: if post == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as f64,
                    y: start, vx: if post == 0 { -2 } else { 2 }, vy: slope, tick_daa: DEFAULT_BALL_TICK_DAA };
                // At 58 columns the sprite origin travels six screen dots to the
                // edge, at 200/53 dots per tick: 1.59 ticks, including a wall bounce.
                assert!((flight.crossing_y(58, post) - expected).abs() < 1e-9);
                for columns in [32, 80, 128] {
                    let y = flight.crossing_y(columns, post);
                    assert!((0.0..=31.0).contains(&y));
                    assert!(if slope > 0 { y < start } else { y > start });
                }
            }
        }
    }

    #[test]
    fn angled_goal_highlight_tracks_the_border_intersection_not_paddle_height() {
        let now = Instant::now();
        for post in 0..=1 {
            let mut s = fixture(post);
            s.court.state.ball_vy = 3;
            let mut effects = CourtEffects::default();
            observe(&mut effects, &s, 106, now);
            observe(&mut effects, &s, 107, now);
            let notice = effects.goal_notice(now).unwrap();
            let flight = notice.flight.unwrap();
            for columns in [32, 58, 80, 128] {
                let y = flight.crossing_y(columns, post);
                assert!((y - notice.y.unwrap()).abs() > 1.0, "angled goal must extend beyond the paddle plane");
                let frame = court_frame_with_presentation(&s, 108, "", "", "", "", CourtPresentation {
                    columns, impact: effects.sample(108, 0.0, now), goal_notice: Some(notice), ..Default::default()
                }).unwrap();
                let row = (y / 2.0).floor() as i64;
                for (at, line) in frame.lines().skip(4).take(16).enumerate() {
                    let highlighted = if post == 0 { line.starts_with('║') } else { line.ends_with('║') };
                    assert_eq!(highlighted, (at as i64 - row).abs() <= 1);
                }
            }
        }
    }

    #[test]
    fn skipped_goal_and_serve_keep_notice_three_seconds_without_resurrecting_exit() {
        let now = Instant::now();
        for post in 0..=1 {
            let s = fixture(post);
            let mut effects = CourtEffects::default();
            let mut baseline = s.clone();
            baseline.court.state.clock = 180;
            baseline.court.state.serve_until = 180;
            baseline.court.state.ball_x = CENTER_X;
            baseline.court.utxo.block_daa_score = 180;
            observe(&mut effects, &baseline, 180, now);
            // A newly accepted checkpoint can already be past the entire pause.
            let mut checkpoint = s.clone();
            checkpoint.court.state = project(&s, 128).unwrap().world;
            checkpoint.court.utxo.block_daa_score = 128;
            let observed = now + Duration::from_secs(20);
            let plain = court_frame_with_presentation(&checkpoint, 200, "", "", "", "", CourtPresentation::default()).unwrap();
            for age in [0, 500, 2999, 3000, 8000] {
                let frame = paint(&mut effects, &checkpoint, 200, observed + Duration::from_millis(age));
                assert_eq!(ball(&frame), ball(&plain), "notice cannot move the resumed ball");
                assert!(effects.sample(200, 0.0, observed).is_none());
                let visible = age < 3000;
                assert_eq!(frame.contains(if post == 0 { "RIGHT SCORES!" } else { "LEFT SCORES!" }), visible);
                for row in frame.lines().skip(4).take(16) {
                    assert_eq!(if post == 0 { row.starts_with('║') } else { row.ends_with('║') }, visible);
                    assert!(!if post == 0 { row.ends_with('║') } else { row.starts_with('║') });
                }
                assert_eq!(frame.lines().count(), 24);
                assert!(frame.lines().last().unwrap().contains("STATUS RETAINED"));
            }
            assert_eq!(project(&checkpoint, 128).unwrap().world.serve_until, 127);
        }
    }

    #[test]
    fn initial_load_reset_and_exact_goal_replay_do_not_notify() {
        let now = Instant::now();
        let s = fixture(0);
        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 110, now);
        assert!(effects.goal_notice(now).is_none(), "already-scored first frame is a baseline");
        assert!(effects.sample(110, 0.0, now).is_none());
        effects.reset_timeline();
        observe(&mut effects, &s, 106, now);
        observe(&mut effects, &s, 107, now);
        assert!(effects.goal_notice(now).is_none(), "same score and goal DAA must not replay");

        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 106, now);
        observe(&mut effects, &s, 107, now);
        assert!(effects.goal_notice(now).is_some());
        observe(&mut effects, &s, 100, now);
        assert!(effects.goal_notice(now).is_none(), "DAA rewind clears contradicted paint");
        observe(&mut effects, &s, 107, now);
        assert!(effects.goal_notice(now).is_none());
        effects.reset_timeline();
        observe(&mut effects, &s, 110, now);
        assert!(effects.goal_notice(now).is_none(), "epoch reset also establishes a baseline");
    }

    #[test]
    fn late_paddle_correction_clears_notice_but_different_later_goal_at_same_score_notifies() {
        let now = Instant::now();
        let s = fixture(0);
        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 106, now);
        observe(&mut effects, &s, 107, now);
        let notice = effects.goal_notice(now).unwrap();
        assert_eq!(notice.post, 0);
        assert_eq!(notice.y, Some(6.0), "retain known goal height");

        // Simulate a validated contact correction at the same playback DAA/epoch.
        let mut corrected = s.clone();
        corrected.court.state = project(&s, 107).unwrap().world;
        corrected.court.state.right_score = 0;
        corrected.court.state.serve_until = 100;
        corrected.court.state.ball_x = LEFT_CONTACT_X;
        corrected.court.state.ball_vx = 1;
        observe(&mut effects, &corrected, 107, now);
        assert!(effects.goal_notice(now).is_none());
        assert!(effects.sample(107, 0.0, now).is_none());

        // A different flight scores at 207, reaching the same 0-1 scoreboard.
        let mut later = s.clone();
        later.court.state.clock = 200;
        later.court.state.serve_until = 200;
        later.court.utxo.block_daa_score = 200;
        observe(&mut effects, &later, 206, now);
        observe(&mut effects, &later, 207, now);
        assert_eq!(effects.goal_notice(now).unwrap().post, 0);
        assert!(effects.goal_notice(now + GOAL_NOTICE_HOLD).is_none());
        observe(&mut effects, &later, 207, now + GOAL_NOTICE_HOLD);
        assert!(effects.goal_notice(now + GOAL_NOTICE_HOLD).is_none(), "cached goal cannot renew");
    }

    #[test]
    fn queued_goals_receive_their_own_visible_time() {
        let now = Instant::now();
        let s = fixture(0);
        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 106, now);
        observe(&mut effects, &s, 107, now);
        let first = effects.goal_notice(now).unwrap();
        let mut next = fixture(1);
        next.court.state.clock = 200;
        next.court.state.serve_until = 200;
        next.court.state.right_score = 1;
        next.court.utxo.block_daa_score = 200;
        observe(&mut effects, &next, 207, now + Duration::from_millis(10));
        let held = effects.goal_notice(now + Duration::from_millis(2999)).unwrap();
        assert_eq!((held.post, held.y, held.flight), (first.post, first.y, first.flight));
        assert_eq!(effects.goal_notice(now + GOAL_NOTICE_HOLD).unwrap().post, 1);
        assert!(effects.goal_notice(now + Duration::from_millis(5999)).is_some());
        assert!(effects.goal_notice(now + Duration::from_secs(6)).is_none());
    }

    #[test]
    fn cosmetic_ripple_spreads_from_goal_edge_and_fades_without_moving_ball() {
        let now = Instant::now();
        for post in 0..=1 {
            let s = fixture(post);
            let mut effects = CourtEffects::default();
            observe(&mut effects, &s, 106, now);
            let first = paint(&mut effects, &s, 107, now);
            let later = paint(&mut effects, &s, 107, now + Duration::from_millis(200));
            let settled = paint(&mut effects, &s, 107, now + Duration::from_millis(700));
            let waves = |frame: &str| frame.lines().skip(4).take(16).enumerate().filter_map(|(row, line)| {
                let ch = if post == 0 { line.chars().next() } else { line.chars().last() };
                matches!(ch, Some('╏' | '┊')).then_some(row)
            }).collect::<Vec<_>>();
            assert!(!waves(&first).is_empty());
            assert_ne!(waves(&first), waves(&later));
            assert!(waves(&settled).is_empty());
            assert!(settled.contains("SCORES!"), "notice outlives the short ripple");
            assert_eq!(ball(&first), ball(&later), "ripple age cannot advance the ball");
        }
    }

    #[test]
    fn cosmetic_contact_bursts_are_cyan_brief_and_leave_ball_pixels_intact() {
        use kaspa_pong::{ascii::{ImpactKind, ImpactVisual}, terminal::court_cell};
        use crossterm::style::Color;
        assert_eq!(court_cell('✧'), ('✧', Color::Cyan));
        assert_eq!(court_cell('·'), ('·', Color::DarkCyan));
        assert_eq!(court_cell('╏'), ('|', Color::Green));
        assert_eq!(court_cell('┊'), ('|', Color::DarkGreen));
        for side in 0..=1 {
            let s = fixture(side);
            let draw = |age, enabled| court_frame_with_presentation(&s, 100, "", "PONG", "", "",
                CourtPresentation { sparkles: enabled, impact: Some(ImpactVisual {
                    kind: ImpactKind::Paddle(side), x: if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as f64,
                    y: 16.0, age, flight: None,
                }), ..Default::default() }).unwrap();
            assert!(draw(0.05, true).contains('✧'));
            assert!(draw(0.20, true).contains('·'));
            assert!(!draw(0.30, true).chars().any(|c| ['✧', '·'].contains(&c)));
            assert_eq!(ball(&draw(0.05, true)), ball(&draw(0.05, false)));
        }
    }

    #[test]
    fn rapid_settlement_retains_notice_after_sprite_expires_and_timeout_fabricates_none() {
        let now = Instant::now();
        let mut s = fixture(0);
        s.court.state.right_score = WIN_SCORE - 1;
        let result = project(&s, 107).unwrap().world.result();
        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 106, now);
        for age in [0, 2000, 2999, 3000] {
            let frame = effects.finished_frame(&result, "PONG", "status", "footer", 58, now + Duration::from_millis(age)).unwrap();
            assert_eq!(frame.contains("RIGHT SCORES!"), age < 3000);
            if age >= 2000 { assert!(ball(&frame).is_empty()); }
        }
        let mut initial_finished = CourtEffects::default();
        assert!(initial_finished.finished_frame(&result, "", "", "", 58, now).is_none());
        assert!(initial_finished.goal_notice(now).is_none());

        let mut timeout = s.court.state.result();
        timeout.end_daa = 300;
        let mut effects = CourtEffects::default();
        observe(&mut effects, &s, 106, now);
        effects.finished_frame(&timeout, "", "", "", 58, now);
        assert!(effects.goal_notice(now).is_none(), "timeout with unchanged score is not a goal");
    }
}
