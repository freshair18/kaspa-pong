//! Physics/projection agreement and visual continuity across contacts and goals.

mod multiball {
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{ascii::{self, CourtEffects, CourtPresentation, GoalNotice}, engine::*};
    use std::time::{Duration, Instant};

    fn snapshot() -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true; world.clock = 100; world.serve_until = 100;
        world.ball_count = 3; world.win_score = 6; world.launch_epoch = 100;
        world.ball_y = 6; world.ball_vy = 1;
        world.extra_balls = [
            Ball { x: CENTER_X, y: 16, vx: -1, vy: -1, serve_until: 100 },
            Ball { x: CENTER_X, y: 26, vx: 1, vy: 0, serve_until: 100 },
        ];
        let queues = empty_queues(&world.left, &world.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: queues[i].clone() }),
        }
    }
    fn render(s: &Snapshot, daa: u64, presentation: CourtPresentation) -> String {
        ascii::court_frame_with_presentation(s, daa, "status", "PONG", "controls", "footer", presentation).unwrap()
    }
    fn pixels(frame: &str, index: usize) -> Vec<(usize, usize)> {
        let bits = [[0, 3], [1, 4], [2, 5], [6, 7]];
        let mut pixels = Vec::new();
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            for (column, ch) in line.chars().skip(1).enumerate() {
                let Some(mask) = ascii::ball_dots(ch) else { continue; };
                let slot = match ch as u32 { 0xe701..=0xeaff => 1, 0xeb01..=0xeeff => 2, _ => 0 };
                if slot != index { continue; }
                for (dy, pair) in bits.iter().enumerate() {
                    for (dx, bit) in pair.iter().enumerate() {
                        if mask & (1 << bit) != 0 { pixels.push((column * 2 + dx, row * 4 + dy)); }
                    }
                }
            }
        }
        pixels
    }
    #[test]
    fn all_three_balls_interpolate_independently_and_cover_center_text() {
        let s = snapshot();
        for columns in [32, 58, 80, 128] {
            let plain = render(&s, 100, CourtPresentation { columns, ..Default::default() });
            let decorated = render(&s, 100, CourtPresentation { columns,
                goal_notice: Some(GoalNotice { post: 0, y: None, flight: None, age: 1.0 }), ..Default::default() });
            let moved = render(&s, 100, CourtPresentation { columns, fractional_daa: 0.9, ..Default::default() });
            for index in 0..3 {
                let first = pixels(&plain, index); let next = pixels(&moved, index);
                assert_eq!(first.len(), 16); assert_eq!(next.len(), 16);
                assert_eq!(pixels(&decorated, index), first, "KASPA cannot cover any ball");
                let sum = |points: &[(usize, usize)]| points.iter().map(|p| p.0).sum::<usize>();
                if index == 1 { assert!(sum(&next) < sum(&first)); }
                else { assert!(sum(&next) > sum(&first)); }
            }
        }
    }
    #[test]
    fn countdowns_are_per_ball_and_overlap_uses_a_stable_slot_order() {
        let mut s = snapshot(); s.court.state.serve_until = 125; s.court.state.extra_balls[0].serve_until = 140;
        let frame = render(&s, 100, CourtPresentation { countdown_pulse: true, ..Default::default() });
        for index in 0..3 { assert_eq!(pixels(&frame, index).len(), 16); }
        assert!(frame.contains("B1: serve 2.5s"));
        assert!(frame.contains("B2: serve 4.0s"));
        assert!(frame.chars().any(|ch| ('\u{ef08}'..='\u{ef0d}').contains(&ch)));
        s.court.state.serve_until = 100;
        s.court.state.extra_balls = [s.court.state.ball(0); 2];
        let overlap = render(&s, 100, CourtPresentation::default());
        assert_eq!(pixels(&overlap, 0).len(), 16);
        assert!(pixels(&overlap, 1).is_empty()); assert!(pixels(&overlap, 2).is_empty());
    }
    #[cfg(feature = "node")]
    #[test]
    fn all_countdowns_fit_the_default_terminal_even_with_goal_notices() {
        use kaspa_pong::terminal::ScreenFrame;
        for count in [2, 3] {
            let mut s = snapshot();
            s.court.state.started = false; s.court.state.ball_count = count;
            for goals in [false, true] {
                let notice = goals.then_some(GoalNotice { post: 0, y: None, flight: None, age: 1.0 });
                let frame = render(&s, 100, CourtPresentation {
                    goal_notice: notice, extra_goal_notices: [notice; 2], ..Default::default()
                });
                let screen = ScreenFrame::compose(&frame, None, None, (80, 25));
                let line = &screen.rows[22].text;
                assert!(line.contains("B1:") && line.contains("serve 2.5s"));
                assert!(line.contains("B2:") && line.contains(if count == 2 { "serve 12.0s" } else { "serve 8.8s" }));
                if count == 3 { assert!(line.contains("B3:") && line.contains("serve 15.1s")); }
                assert!(frame.lines().nth(22).unwrap().chars().count() <= 79);
                assert_eq!(frame.lines().count(), 24);
            }
        }
        let s = snapshot();
        let footer = "SYNCED | paid: 0 KAS";
        let frame = ascii::court_frame_with_presentation(&s, 100, "status", "PONG", "controls", footer,
            CourtPresentation::default()).unwrap();
        assert_eq!(frame.lines().nth(22).unwrap(), footer, "shared diagnostics remain visible once");
    }
    #[cfg(feature = "node")]
    #[test]
    fn palettes_decode_to_the_same_braille_with_distinct_stable_colors() {
        use crossterm::style::Color;
        use kaspa_pong::terminal::court_cell;
        let s = snapshot(); let frame = render(&s, 100, CourtPresentation::default());
        for (index, expected) in [(0, Color::Yellow), (1, Color::Magenta), (2, Color::Cyan)] {
            for ch in frame.chars().filter(|&ch| ascii::ball_dots(ch).is_some()) {
                let slot = match ch as u32 { 0xe701..=0xeaff => 1, 0xeb01..=0xeeff => 2, _ => 0 };
                if slot == index {
                    let (glyph, color) = court_cell(ch);
                    assert_eq!(color, expected); assert_eq!(ascii::ball_dots(glyph), ascii::ball_dots(ch));
                }
            }
        }
    }
    #[test]
    fn one_goal_keeps_other_balls_live_and_waiting_sprite_returns_after_exit() {
        let now = Instant::now(); let mut s = snapshot();
        s.court.state.ball_x = LEFT_CONTACT_X + BALL_SPEED;
        s.court.state.ball_vx = -1; s.court.state.ball_vy = 0;
        let mut effects = CourtEffects::default();
        effects.observe(&s, &project(&s, 106).unwrap().world, 106, now);
        let scored = project(&s, 107).unwrap().world;
        assert!(scored.serve_until > 107 + SERVE_DELAY_DAA);
        effects.observe(&s, &scored, 107, now);
        let first = effects.sample(107, 0.0, now).unwrap();
        assert!(matches!(first.kind, ascii::ImpactKind::Goal(0)));
        assert!(effects.extra_impacts(107, 0.0, now).iter().all(Option::is_none));
        let late = project(&s, 128).unwrap().world;
        effects.observe(&s, &late, 128, now + Duration::from_secs(2));
        assert!(effects.sample(128, 0.0, now).is_none(), "long phase wait must not extend the exit");
        let frame = render(&s, 128, CourtPresentation { countdown_pulse: true,
            goal_notice: effects.goal_notice(now), extra_goal_notices: effects.extra_goal_notices(now),
            extra_impacts: effects.extra_impacts(128, 0.0, now), ..Default::default() });
        for index in 0..3 { assert_eq!(pixels(&frame, index).len(), 16); }
        assert_ne!(late.ball(1).x, scored.ball(1).x);
        assert_ne!(late.ball(2).x, scored.ball(2).x);
        effects.reset_timeline();
        effects.observe(&s, &project(&s, 106).unwrap().world, 106, now);
        effects.observe(&s, &scored, 107, now);
        assert!(effects.sample(107, 0.0, now).is_none(), "exact replay cannot restart a goal");
    }
}

mod animation {
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{ascii::{CourtPresentation, court_frame_with_presentation}, engine::*};

    fn fixture(side: i64, vy: i64) -> Snapshot {
        let mut w = World::opening(vec![], vec![]);
        w.started = true;
        w.clock = 100;
        w.serve_until = 100;
        w.launch_epoch = 127;
        w.ball_vx = if side == 0 { -1 } else { 1 };
        w.ball_x = if side == 0 { LEFT_CONTACT_X + 2 } else { RIGHT_CONTACT_X - 2 };
        w.ball_y = 16;
        w.ball_vy = vy;
        let q = empty_queues(&w.left, &w.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: w },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: q[i].clone() }),
        }
    }

    fn dots(s: &Snapshot, daa: u64, fraction: f64, columns: usize) -> Vec<(i64, i64)> {
        let frame = court_frame_with_presentation(s, daa, "", "PONG", "controls", "footer", CourtPresentation {
            columns, fractional_daa: fraction, ..Default::default()
        }).unwrap();
        let bits = [[0, 3], [1, 4], [2, 5], [6, 7]];
        let mut dots = vec![];
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            for (col, ch) in line.chars().skip(1).enumerate() {
                let Some(mask) = kaspa_pong::ascii::ball_dots(ch) else { continue };
                for (dy, pair) in bits.iter().enumerate() {
                    for (dx, bit) in pair.iter().enumerate() {
                        if mask & (1 << bit) != 0 { dots.push(((2 * col + dx) as i64, (4 * row + dy) as i64)); }
                    }
                }
            }
        }
        dots
    }

    #[test]
    fn subdot_centroids_move_in_quarters_with_sixteen_dots_and_no_paddle_penetration() {
        for columns in [32, 58, 77, 128] {
            let post = |x| (x as f64 * (columns - 1) as f64 / 57.0).round() as i64;
            let (left, right) = (2 * (post(LEFT_CONTACT_X) + 1), 2 * post(RIGHT_CONTACT_X));
            for side in 0..=1 {
                for vy in [-2, 0, 2] {
                    let s = fixture(side, vy);
                    let mut centers = vec![];
                    for step in 0..=32 {
                        let d = dots(&s, 103, step as f64 / 33.0, columns);
                        assert_eq!(d.len(), 16);
                        assert_eq!(d.iter().collect::<std::collections::BTreeSet<_>>().len(), 16);
                        assert!(d.iter().all(|&(x, y)| left <= x && x < right && (0..64).contains(&y)));
                        let center = (d.iter().map(|p| p.0).sum::<i64>() as f64 / 16.0, d.iter().map(|p| p.1).sum::<i64>() as f64 / 16.0);
                        assert_eq!((center.0 * 4.0).fract(), 0.0);
                        assert_eq!((center.1 * 4.0).fract(), 0.0);
                        centers.push(center);
                    }
                    assert!(centers.windows(2).any(|p| (p[1].0 - p[0].0).abs() == 0.25));
                    if vy != 0 { assert!(centers.windows(2).any(|p| (p[1].1 - p[0].1).abs() == 0.25)); }
                    for (daa, fraction) in [(106, 0.99), (107, 0.0), (107, 0.99), (110, 0.5)] {
                        let d = dots(&s, daa, fraction, columns);
                        assert_eq!(d.len(), 16);
                        assert!(d.iter().all(|&(x, _)| left <= x && x < right));
                    }
                    let contact = dots(&s, 107, 0.0, columns);
                    assert_eq!(contact.iter().map(|p| p.0).max().unwrap() - contact.iter().map(|p| p.0).min().unwrap(), 3);
                    assert_eq!(contact.iter().map(|p| p.1).max().unwrap() - contact.iter().map(|p| p.1).min().unwrap(), 3);
                }
            }
        }
    }

    #[test]
    fn command_and_checkpoint_splits_preserve_fractional_flight_frames() {
        let mut s = fixture(1, 1);
        s.court.state.ball_x = 20;
        s.players[0].state.count = 1;
        s.players[0].state.times[0] = 103;
        s.players[0].state.commands[0] = 1;
        let plan = project(&s, 103).unwrap();
        let mut checkpoint = s.clone();
        checkpoint.court.state = plan.world;
        checkpoint.court.utxo.block_daa_score = 104;
        for (cell, q) in checkpoint.players.iter_mut().zip(plan.queues) { cell.state = q; }
        checkpoint.players[0].state.y = checkpoint.court.state.left_y;
        checkpoint.players[1].state.y = checkpoint.court.state.right_y;
        checkpoint.players[0].state.through = 103;
        for columns in [58, 77, 128] {
            for (daa, fraction) in [(103, 0.25), (105, 0.75), (107, 0.0), (110, 0.5)] {
                assert_eq!(dots(&s, daa, fraction, columns), dots(&checkpoint, daa, fraction, columns));
            }
        }
    }
}

mod collision {
    //! Finite vertical ball overlap, preserving the existing contact and wall clock.
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{ascii, engine::*};

    fn snapshot(side: usize, offset: i64, vy: i64) -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true;
        world.clock = 100;
        world.serve_until = 100;
        world.launch_epoch = 127;
        world.ball_vx = if side == 0 { -1 } else { 1 };
        world.ball_x = if side == 0 { LEFT_CONTACT_X + BALL_SPEED } else { RIGHT_CONTACT_X - BALL_SPEED };
        world.ball_y = 16 + offset - vy;
        world.ball_vy = vy;
        let queues = empty_queues(&world.left, &world.right);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint: TransactionOutpoint::new(Hash::from_bytes([1; 32]), 0), utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell {
                outpoint: TransactionOutpoint::new(Hash::from_bytes([i as u8 + 2; 32]), 0),
                utxo: utxo.clone(), state: queues[i].clone(),
            }),
        }
    }

    // Decode actual ball dots and paddle blocks, including their half-cell edges.
    fn rendered_vertical_overlap(s: &Snapshot, side: usize) -> bool {
        let frame = ascii::frame(s, 100, "collision").unwrap();
        let mut ball_rows = vec![];
        let mut paddle_rows = vec![];
        let post = if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as usize;
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            match line.chars().nth(post + 1).unwrap() {
                '█' => paddle_rows.extend(4 * row..4 * row + 4),
                '▀' => paddle_rows.extend(4 * row..4 * row + 2),
                '▄' => paddle_rows.extend(4 * row + 2..4 * row + 4),
                ' ' => (),
                other => panic!("unexpected paddle glyph {other}"),
            }
            for ch in line.chars() {
                if let Some(mask) = (ch as u32).checked_sub(0x2800).filter(|mask| *mask <= 255) {
                    for (dy, bits) in [[0, 3], [1, 4], [2, 5], [6, 7]].iter().enumerate() {
                        for bit in bits {
                            if mask & (1 << bit) != 0 { ball_rows.push(4 * row + dy); }
                        }
                    }
                }
            }
        }
        assert_eq!(ball_rows.len(), 16);
        assert_eq!(paddle_rows.len(), 18); // Nine game cells, two dots per cell.
        ball_rows.iter().any(|y| paddle_rows.contains(y))
    }

    #[test]
    fn rendered_outer_edges_hit_and_next_integer_heights_miss_on_both_sides() {
        assert_eq!((PADDLE_RADIUS, BALL_RADIUS, PADDLE_MIN_Y, PADDLE_MAX_Y), (4, 1, 4, 27));
        for side in [0, 1] {
            for (offset, hit) in [(-6, false), (-5, true), (5, true), (6, false)] {
                let s = snapshot(side, offset, 0);
                assert_eq!(rendered_vertical_overlap(&s, side), hit, "side {side}, offset {offset}");
                assert_eq!(next_contact(&s).unwrap(), (side, 107));
                let before = project(&s, 106).unwrap().world;
                assert_eq!((before.ball_x, before.ball_vx), (s.court.state.ball_x, s.court.state.ball_vx));
                let contact = resolve(&s).unwrap().world;
                let view = project(&s, 107).unwrap().world;
                assert_eq!((contact.ball_x, contact.ball_y, contact.ball_vx, contact.ball_vy),
                    (view.ball_x, view.ball_y, view.ball_vx, view.ball_vy));
                assert_eq!(contact.clock, 107);
                assert_eq!((contact.left_score, contact.right_score),
                    if hit { (0, 0) } else if side == 0 { (0, 1) } else { (1, 0) });
                if hit {
                    assert_eq!(contact.ball_x, if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X });
                    assert_eq!((contact.ball_vx, contact.ball_vy), (-s.court.state.ball_vx, offset.clamp(-2, 2)));
                } else {
                    assert_eq!((contact.ball_x, contact.serve_until), (CENTER_X, 107 + SERVE_DELAY_DAA));
                }
            }
        }
    }

    #[cfg(feature = "tooling")]
    #[test]
    fn covenant_accepts_exact_ball_edges_and_rejects_forged_edge_outcomes() {
        use argent_runtime::{Artifact, TxBuilder};
        use secp256k1::{Keypair, Secp256k1, SecretKey};

        let artifact: Artifact = serde_json::from_str(include_str!("../artifacts/artifact.json")).unwrap();
        let builder = TxBuilder::new(&artifact).unwrap();
        let key = |n| Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap());
        for side in [0, 1] {
            for offset in [-6, -5, 5, 6] {
                let mut s = snapshot(side, offset, 0);
                s.court.state.left = pubkey(&key(1));
                s.court.state.right = pubkey(&key(2));

                let id = Some(Hash::from_bytes([77; 32]));
                s.court.utxo = builder.covenant_utxo("Court", s.court.state.state(),
                    COURT_VALUE, 100, false, id).unwrap();
                let queues = empty_queues(&s.court.state.left, &s.court.state.right);
                for (player, queue) in s.players.iter_mut().zip(queues) {
                    player.state = queue;
                    player.utxo = builder.covenant_utxo("Player", player.state.state(), QUEUE_DEPOSIT, 100, false, id).unwrap();
                }
                let plan = resolve(&s).unwrap();
                let owner = key(side as u8 + 1);
                assert!(builder.build(&resolve_context(&s, &plan, &owner).unwrap()).is_ok(), "side {side}, offset {offset}");
                // A valid receiver signature cannot turn an edge hit into a goal,
                // nor turn the first separated height into a hit.
                let mut forged = plan.clone();
                let w = &mut forged.world;
                if offset.abs() == 5 {
                    if side == 0 { w.right_score += 1; } else { w.left_score += 1; }
                    w.ball_x = CENTER_X;
                    w.serve_until = 107 + SERVE_DELAY_DAA;
                    let choice = serve_choice(127, 0, &w.left, &w.right);
                    w.ball_y = 2 + choice / 10;
                    w.ball_vx = if choice % 2 == 0 { -1 } else { 1 };
                    w.ball_vy = choice / 2 % 5 - 2;
                } else {
                    w.left_score = 0;
                    w.right_score = 0;
                    w.ball_x = if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X };
                    w.ball_y = 16 + offset;
                    w.ball_vx = -s.court.state.ball_vx;
                    w.ball_vy = offset.clamp(-2, 2);
                    w.serve_until = 100;
                    w.launch_epoch = 127;
                }
                assert!(builder.build(&resolve_context(&s, &forged, &owner).unwrap()).is_err(), "forged side {side}, offset {offset}");
            }
        }
    }
}

mod goals {
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{ascii::{CourtEffects, CourtPresentation, court_frame_with_presentation}, engine::*};
    use std::time::{Duration, Instant};

    fn fixture(side: i64, distance: i64, y: i64, vy: i64) -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true;
        world.clock = 100;
        world.serve_until = 100;
        world.launch_epoch = 127;
        world.ball_vx = if side == 0 { -1 } else { 1 };
        world.ball_x = if side == 0 { LEFT_CONTACT_X + distance } else { RIGHT_CONTACT_X - distance };
        world.ball_y = y;
        world.ball_vy = vy;
        let queues = empty_queues(&world.left, &world.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: queues[i].clone() }),
        }
    }

    fn render(s: &Snapshot, daa: u64, fraction: f64, effects: Option<&mut CourtEffects>, now: Instant, columns: usize) -> String {
        let (impact, goal_notice) = effects.map(|e| {
            e.observe(s, &project(s, daa).unwrap().world, daa, now);
            (e.sample(daa, fraction, now), e.goal_notice(now))
        }).unwrap_or((None, None));
        court_frame_with_presentation(s, daa, "STATUS RETAINED", "PONG", "controls", "footer", CourtPresentation {
            columns, fractional_daa: fraction, impact, goal_notice, ..Default::default()
        }).unwrap()
    }

    fn dots(frame: &str) -> Vec<(i64, i64)> {
        let bits = [[0, 3], [1, 4], [2, 5], [6, 7]];
        let mut dots = vec![];
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            for (column, ch) in line.chars().skip(1).enumerate() {
                let Some(mask) = kaspa_pong::ascii::ball_dots(ch) else { continue };
                for (dy, pair) in bits.iter().enumerate() {
                    for (dx, bit) in pair.iter().enumerate() {
                        if mask & (1 << bit) != 0 { dots.push(((2 * column + dx) as i64, (4 * row + dy) as i64)); }
                    }
                }
            }
        }
        dots
    }

    fn center(frame: &str) -> (f64, f64) {
        let d = dots(frame);
        assert_eq!(d.len(), 16);
        (d.iter().map(|p| p.0 as f64).sum::<f64>() / 16.0, d.iter().map(|p| p.1 as f64).sum::<f64>() / 16.0)
    }

    fn goal(s: &Snapshot, now: Instant) -> CourtEffects {
        let mut effects = CourtEffects::default();
        effects.observe(s, &project(s, 106).unwrap().world, 106, now - Duration::from_millis(100));
        effects.observe(s, &project(s, 107).unwrap().world, 107, now);
        effects
    }

    #[test]
    fn last_miss_tick_and_exit_keep_pace_slope_and_wall_reflection() {
        let now = Instant::now();
        for side in 0..=1 {
            for distance in [1, 2] {
                for (y, vy) in [(6, 0), (6, 2), (2, -2), (29, 2)] {
                    let s = fixture(side, distance, y, vy);
                    let mut effects = goal(&s, now);
                    let before = center(&render(&s, 105, 0.25, None, now, 128));
                    let contact = center(&render(&s, 107, 0.0, Some(&mut effects), now, 128));
                    let after = center(&render(&s, 108, 0.75, Some(&mut effects), now + Duration::from_millis(175), 128));
                    assert!(((contact.0 - before.0) - (after.0 - contact.0)).abs() <= 0.25);
                    if y == 6 {
                        assert!(((contact.1 - before.1) - (after.1 - contact.1)).abs() <= 0.25);
                    } else if y < 16 {
                        assert!(after.1 >= contact.1);
                    } else {
                        assert!(after.1 <= contact.1);
                    }
                    let a = center(&render(&s, 108, 0.0, Some(&mut effects), now, 128));
                    let b = center(&render(&s, 109, 0.0, Some(&mut effects), now, 128));
                    assert_ne!(a.0, b.0, "exit advances with playback even at identical wall times");
                }
            }
        }
    }

    #[test]
    fn goal_exit_clips_then_waits_at_center_for_the_phase_aligned_serve() {
        let now = Instant::now();
        for columns in [58, 77, 128] {
            for side in 0..=1 {
                let s = fixture(side, 1, 6, 0);
                let mut effects = goal(&s, now);
                let mut counts = vec![];
                for phase in 0..=79 {
                    let elapsed = 12.0 + phase as f64 / 10.0;
                    let daa = 107 + elapsed.floor() as u64;
                    let frame = render(&s, daa, elapsed.fract(), Some(&mut effects), now, columns);
                    counts.push(dots(&frame).iter().filter(|&&(x, _)|
                        if side == 0 { x < columns as i64 / 2 } else { x > columns as i64 * 3 / 2 }).count());
                    assert_eq!(frame.lines().count(), 24);
                    assert!(frame.lines().last().unwrap().contains("STATUS RETAINED"));
                }
                assert!(counts.windows(2).all(|p| p[1] <= p[0]));
                assert_eq!(counts.last(), Some(&0), "outgoing ball clears before the next launch");
                let resumed = render(&s, 127, 0.0, Some(&mut effects), now, columns);
                assert_eq!(dots(&resumed), dots(&render(&s, 127, 0.0, None, now, columns)));
                assert!(resumed.contains("SCORES!"), "notice outlives the exit without delaying the serve");
                assert_eq!(project(&s, 127).unwrap().world.serve_until, 127);
            }
        }
        for side in 0..=1 {
            let s = fixture(side, 2, 6, 0);
            let mut effects = goal(&s, now);
            // At width 128 the exact post origins are 10 and 242 Braille dots.
            let start: f64 = if side == 0 { 10.0 } else { 242.0 };
            let target: f64 = if side == 0 { -2.0 } else { 254.0 };
            let age = (target - start).abs() * 0.7 / (2.0 * 232.0 / 53.0);
            let elapsed = age * DAA_PER_SECOND as f64;
            let clipped = render(&s, 107 + elapsed.floor() as u64, elapsed.fract(), Some(&mut effects), now, 128);
            assert_eq!(dots(&clipped).len(), 8);
            let frame = render(&s, 126, 0.0, Some(&mut effects), now, 128);
            assert_eq!(dots(&frame), dots(&render(&s, 126, 0.0, None, now, 128)),
                "the replacement ball waits at center after the outgoing ball clears");
            assert!(frame.lines().nth(22).unwrap().contains("SCORES! | Next serve in 0.1s"));
        }
    }

    #[test]
    fn two_x_playback_checkpoint_repeats_and_rewinds_keep_one_continuous_goal_clock() {
        let now = Instant::now();
        let s = fixture(0, 1, 6, 2);
        let mut effects = CourtEffects::default();
        effects.observe(&s, &project(&s, 106).unwrap().world, 106, now);
        let scored = project(&s, 107).unwrap().world;
        let mut checkpoint = s.clone();
        checkpoint.court.state = scored.clone();
        checkpoint.court.utxo.block_daa_score = 107;
        effects.observe(&checkpoint, &scored, 107, now);
        let flight = effects.sample(107, 0.0, now).unwrap().flight.unwrap();
        assert_eq!((flight.vx, flight.vy), (-2, 2));
        let fast = render(&checkpoint, 110, 0.5, Some(&mut effects), now + Duration::from_millis(175), 128);
        assert!((effects.sample(110, 0.5, now).unwrap().age - 0.35).abs() < 0.000001);
        let repeat = render(&checkpoint, 110, 0.5, Some(&mut effects), now + Duration::from_secs(3), 128);
        assert_eq!(dots(&fast), dots(&repeat), "cached playback neither drifts nor expires in wall time");
        let mut normal = goal(&s, now);
        assert_eq!(dots(&fast), dots(&render(&s, 110, 0.5, Some(&mut normal), now + Duration::from_millis(350), 128)));
        let before_serve = render(&checkpoint, 126, 0.9, Some(&mut effects), now + Duration::from_millis(995), 58);
        assert_eq!(dots(&before_serve), dots(&render(&checkpoint, 126, 0.9, None, now, 58)));
        assert_eq!(dots(&before_serve).len(), 16, "the replacement waits at center before launch");
        let resumed = render(&checkpoint, 127, 0.0, Some(&mut effects), now + Duration::from_secs(1), 58);
        assert_eq!(dots(&resumed).len(), 16);
        effects.observe(&s, &project(&s, 100).unwrap().world, 100, now);
        assert!(effects.sample(100, 0.0, now).is_none(), "rewind discards the old goal");
        effects.observe(&s, &scored, 107, now);
        assert!(effects.sample(107, 0.0, now).is_none(), "an exact goal replay must not restart the exit");
    }

    #[test]
    fn winning_goal_completes_cosmetically_after_immediate_settlement_without_restarting() {
        let now = Instant::now();
        for columns in [58, 77, 128] {
            for side in 0..=1 {
                let mut s = fixture(side, 1, 6, 2);
                if side == 0 { s.court.state.right_score = WIN_SCORE - 1; }
                else { s.court.state.left_score = WIN_SCORE - 1; }
                let mut effects = CourtEffects::default();
                effects.observe(&s, &project(&s, 106).unwrap().world, 106, now);
                let result = project(&s, 107).unwrap().world.result();
                let first = effects.finished_frame(&result, "PONG", "status", "footer", columns, now).unwrap();
                let next = effects.finished_frame(&result, "PONG", "status", "footer", columns, now + Duration::from_millis(350)).unwrap();
                assert_ne!(center(&first).0, center(&next).0);
                let fading = effects.finished_frame(&result, "PONG", "status", "footer", columns, now + Duration::from_millis(1600)).unwrap();
                assert!(dots(&fading).len() < 16);
                for age in [1990, 2000, 3000] {
                    let expired = effects.finished_frame(&result, "PONG", "status", "footer", columns, now + Duration::from_millis(age)).unwrap();
                    assert!(dots(&expired).is_empty());
                    assert_eq!(expired.lines().count(), 24);
                }
                let mut handover = goal(&s, now);
                let at = now + Duration::from_millis(175);
                let live = render(&s, 110, 0.5, Some(&mut handover), at, columns);
                let settled = handover.finished_frame(&result, "PONG", "status", "footer", columns, at).unwrap();
                assert_eq!(dots(&live), dots(&settled), "settlement continues at the last fractional playback position");
            }
        }
    }
}

mod paddles {
    //! Focused paddle geometry checks without generated artifacts or node access.
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};
    use kaspa_pong::{
        ascii::{CourtPresentation, court_frame_with_presentation},
        engine::*,
    };

    fn snapshot() -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true;
        world.clock = 100;
        world.serve_until = 100;
        world.launch_epoch = 127;
        let queues = empty_queues(&world.left, &world.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: queues[i].clone() }),
        }
    }

    #[cfg(feature = "node")]
    #[test]
    fn countdown_pulse_changes_only_ball_color_and_stops_at_serve() {
        use kaspa_pong::terminal::court_cell;
        let mut s = snapshot();
        s.court.state.serve_until = 125;
        let paint = |s: &Snapshot, daa, pulse| court_frame_with_presentation(s, daa, "", "PONG", "", "",
            CourtPresentation { countdown_pulse: pulse, ..Default::default() }).unwrap();
        for score in [0, 1] {
            s.court.state.left_score = score;
            let mut tones = Vec::new();
            for daa in [100, 102, 105, 108] {
                let plain = paint(&s, daa, false);
                let pulsed = paint(&s, daa, true);
                let decode = |frame: &str| frame.chars().map(|c| court_cell(c).0).collect::<String>();
                assert_eq!(decode(&pulsed), decode(&plain), "pulse preserves the full sprite, layout and text");
                let ball: Vec<_> = pulsed.chars().filter(|c| ('\u{e101}'..='\u{e3ff}').contains(c)).collect();
                assert!(!ball.is_empty(), "opening and between-point waiting balls can pulse");
                let tone = court_cell(ball[0]).1;
                assert!(ball.iter().all(|&c| court_cell(c).1 == tone));
                if !tones.contains(&tone) { tones.push(tone); }
            }
            assert_eq!(tones.len(), 3, "gentle three-tone yellow pulse");
            assert!(!paint(&s, 125, true).chars().any(|c| ('\u{e101}'..='\u{e3ff}').contains(&c)), "normal yellow resumes at the exact serve boundary");
        }
    }

    fn column_cells(frame: &str, column: usize, glyphs: [char; 3]) -> Vec<i64> {
        let mut cells = Vec::new();
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            let ch = line.chars().nth(column + 1).unwrap();
            if ch == glyphs[0] || ch == glyphs[1] {
                cells.push(2 * row as i64);
            }
            if ch == glyphs[0] || ch == glyphs[2] {
                cells.push(2 * row as i64 + 1);
            }
        }
        cells
    }
    #[test]
    fn accepted_target_stays_red_and_normal_hud_shows_game_diagnostics() {
        let mut s = snapshot();
        let draw = |s: &Snapshot, sparks| court_frame_with_presentation(s, 108, "", "PONG", "controls", "footer",
            CourtPresentation { accepted_paddles: Some((20,16)), preview: Some((0,20)), sparkles: sparks, ..Default::default() }).unwrap();
        let before = draw(&s, false);
        assert!(before.chars().any(|c| ['▓','▴','▾'].contains(&c)), "accepted but not played movement stays red");
        for diagnostic in ["DAA 108", "last contact 100", "y=20/16", "On-chain batches:", "checkpoints: 0"] {
            assert!(before.contains(diagnostic), "missing {diagnostic}");
        }
        for noise in ["Gray:", "Recovery:"] { assert!(!before.contains(noise)); }
        assert!(!before.contains('✧'));
        assert!(draw(&s, true).contains('✧'), "brief cosmetic sparkle is opt-in to the renderer");
        s.court.state.left_y = 20;
        s.players[0].state.y = 20;
        assert!(!draw(&s, false).chars().any(|c| ['▓','▴','▾'].contains(&c)), "red retires only at the visible white target");
        s.court.state.serve_until = 125;
        let countdown = draw(&s, false);
        assert!(countdown.contains("Game will start in"));
        assert!(countdown.contains("DAA); paddles active"));
    }

    #[test]
    fn nine_cell_collision_accepted_and_preview_paddles_keep_their_edges() {
        assert_eq!((PADDLE_RADIUS, BALL_RADIUS, PADDLE_MIN_Y, PADDLE_MAX_Y), (4, 1, 4, 27));
        for columns in [32, 58, 80, 128] {
            for side in 0..=1 {
                for center in PADDLE_MIN_Y..=PADDLE_MAX_Y {
                    let mut s = snapshot();
                    s.players[side].state.y = center;
                    // Force accepted/preview inputs through both presentation clamps.
                    let (accepted, preview) = if center <= 16 { (999, -999) } else { (-999, 999) };
                    let mut accepted_paddles = (16, 16);
                    if side == 0 { accepted_paddles.0 = accepted; } else { accepted_paddles.1 = accepted; }
                    let frame = court_frame_with_presentation(
                        &s, 100, "", "PONG", "controls", "footer",
                        CourtPresentation {
                            columns, accepted_paddles: Some(accepted_paddles), preview: Some((side as i64, preview)), debug_paddles: true,
                            ..Default::default()
                        },
                    ).unwrap();
                    let post = if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X };
                    let column = (post as f64 * (columns - 1) as f64 / (COURT_WIDTH - 1) as f64).round() as usize;
                    let accepted_column = if side == 0 { column - 1 } else { column + 1 };
                    let preview_column = if side == 0 { column + 1 } else { column - 1 };
                    let extent = |center| (center - 4..=center + 4).collect::<Vec<i64>>();
                    assert_eq!(column_cells(&frame, column, ['█', '▀', '▄']), extent(center));
                    assert_eq!(column_cells(&frame, accepted_column, ['░', '╵', '╷']), extent(accepted.clamp(4, 27)));
                    let red = if center == preview.clamp(4, 27) { vec![] } else { extent(preview.clamp(4, 27)) };
                    assert_eq!(column_cells(&frame, preview_column, ['▓', '▴', '▾']), red);
                    let normal = court_frame_with_presentation(&s, 100, "", "PONG", "controls", "footer",
                        CourtPresentation { columns, accepted_paddles: Some(accepted_paddles), preview: Some((side as i64, preview)), ..Default::default() }).unwrap();
                    assert!(column_cells(&normal, accepted_column, ['░', '╵', '╷']).is_empty(), "gray is debug-only");
                    assert_eq!(column_cells(&normal, column, ['█', '▀', '▄']), extent(center));
                    assert_eq!(column_cells(&normal, preview_column, ['▓', '▴', '▾']), red);
                }
            }
        }
    }

    #[test]
    fn both_paddles_clamp_movement_to_centers_four_through_twenty_seven() {
        for side in 0..=1 {
            for (direction, edge) in [(-27, 4), (27, 27)] {
                let mut s = snapshot();
                let lane = &mut s.players[side];
                lane.state = lane.state.push(direction, 100).unwrap();
                lane.utxo.block_daa_score = 101;
                let checkpoint = checkpoint_plan(&s, side, 110).unwrap();
                assert_eq!(checkpoint.queues[side].y, edge);
                assert_eq!(checkpoint.queues[side].count, 0);
                let projected = project(&s, 110).unwrap().world;
                assert_eq!(if side == 0 { projected.left_y } else { projected.right_y }, edge);

                // Another outward batch stays at the edge; an inward step moves off it.
                s.players[side].state = checkpoint.queues[side].push(direction, 110).unwrap();
                s.players[side].utxo.block_daa_score = 111;
                assert_eq!(checkpoint_plan(&s, side, 120).unwrap().queues[side].y, edge);
                s.players[side].state = checkpoint.queues[side].push(-direction.signum(), 110).unwrap();
                assert_eq!(checkpoint_plan(&s, side, 120).unwrap().queues[side].y, edge - direction.signum());
            }
        }
    }

    #[test]
    fn both_contacts_include_the_ball_at_offset_five_and_miss_at_six() {
        assert_eq!((LEFT_CONTACT_X, RIGHT_CONTACT_X, BALL_SPEED), (2, 55, 2));
        for side in 0..=1 {
            for offset in [-6_i64, -5, -4, 0, 4, 5, 6] {
                let mut s = snapshot();
                let w = &mut s.court.state;
                let post = if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X };
                let incoming = if side == 0 { -1 } else { 1 };
                w.ball_x = post - incoming * BALL_SPEED;
                w.ball_y = 16 + offset;
                w.ball_vx = incoming;
                let (receiver, deadline) = next_contact(&s).unwrap();
                assert_eq!((receiver, deadline), (side, 100 + DEFAULT_BALL_TICK_DAA));
                let resolved = resolve(&s).unwrap().world;
                let projected = project(&s, deadline as u64).unwrap().world;
                assert_eq!(
                    (resolved.ball_x, resolved.ball_y, resolved.ball_vx, resolved.ball_vy, resolved.left_score, resolved.right_score, resolved.serve_until),
                    (projected.ball_x, projected.ball_y, projected.ball_vx, projected.ball_vy, projected.left_score, projected.right_score, projected.serve_until),
                );
                if offset.abs() <= 5 {
                    assert_eq!((resolved.ball_x, resolved.ball_y, resolved.ball_vx, resolved.ball_vy),
                        (post, 16 + offset, -incoming, offset.clamp(-2, 2)));
                    assert_eq!((resolved.left_score, resolved.right_score), (0, 0));
                } else {
                    assert_eq!((resolved.left_score, resolved.right_score), if side == 0 { (0, 1) } else { (1, 0) });
                    assert_eq!(resolved.ball_x, CENTER_X);
                    let minimum = deadline + SERVE_DELAY_DAA;
                    let phase = s.court.state.launch_phase(0);
                    let period = s.court.state.court_period();
                    assert!(resolved.serve_until >= minimum);
                    assert_eq!((resolved.serve_until - phase).rem_euclid(period), 0);
                    assert!(resolved.serve_until - period < minimum);
                }
            }
        }
    }
}
