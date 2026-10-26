//! Terminal character graphics, independent of transaction construction.
use super::engine::{
    BALL_SPEED, CENTER_X, GameSpeed, COURT_WIDTH, DAA_PER_SECOND, Finish, LEFT_CONTACT_X, PADDLE_MAX_Y, PADDLE_MIN_Y, PADDLE_RADIUS,
    RESPONSE_TIMEOUT_DAA, RIGHT_CONTACT_X, Result, SERVE_DELAY_DAA, Snapshot, WIN_SCORE, World, project,
};
use std::{fmt::Write, time::{Duration, Instant}};

pub fn completed(result: &Finish, title: &str, status: &str, footer: &str) -> String {
    completed_for_side(result, None, title, status, footer)
}

/// `Some(0)` is the left player, `Some(1)` the right; `None` is an observer.
pub fn completed_for_side(result: &Finish, local_side: Option<i64>, title: &str, status: &str, footer: &str) -> String {
    let heading = "GAME COMPLETED";
    let outcome = match result.winner {
        winner @ (0 | 1) => {
            let side = if winner == 0 { "LEFT" } else { "RIGHT" };
            let timeout = result.left_score < WIN_SCORE && result.right_score < WIN_SCORE;
            match local_side.filter(|&side| matches!(side, 0 | 1)) {
                Some(local) if timeout && local == winner =>
                    format!("You won! Your opponent's response timed out ({RESPONSE_TIMEOUT_DAA} DAA). {side} wins."),
                Some(_) if timeout =>
                    format!("You lost! Your response timed out ({RESPONSE_TIMEOUT_DAA} DAA). {side} wins."),
                Some(local) => format!("You {}! {side} wins by score.", if local == winner { "won" } else { "lost" }),
                None if timeout => format!("{side} wins by response timeout ({RESPONSE_TIMEOUT_DAA} DAA)."),
                None => format!("{side} wins by score."),
            }
        },
        _ if result.left_score == result.right_score => "Draw. No winner recorded.".to_string(),
        _ => "No winner recorded.".to_string(),
    };
    let stake = result.stake_per_player as u64;
    let receipt = match result.winner {
        0 | 1 => format!("Winner pot: {} KAS to the winning player address.", 2 * stake / 100_000_000),
        _ => "No payout allocation recorded.".to_string(),
    };
    format!(
        "{title}\n\n\
         +----------------------------------+\n\
         |          {heading:^14}          |\n\
         +----------------------------------+\n\
         {outcome}\n\n\
         Final score\n\
           LEFT       {}\n\
           RIGHT      {}\n\
           Total goals: {}\n\
         End DAA: {}\n\n\
         {status}\n\
         {receipt}\n\
         {footer}\n",
        result.left_score,
        result.right_score,
        result.left_score + result.right_score,
        result.end_daa
    )
}

pub fn frame(snapshot: &Snapshot, daa: u64, status: &str) -> Result<String> {
    frame_with_labels(snapshot, daa, status, "PONG", "Up/Down or W/S: move your paddle; Esc quits", "")
}

pub fn frame_with_labels(snapshot: &Snapshot, daa: u64, status: &str, title: &str, controls: &str, footer: &str) -> Result<String> {
    frame_with_preview(snapshot, daa, status, title, controls, footer, None)
}

/// Add a local target beside the accepted paddle; physics uses accepted state.
pub fn frame_with_preview(
    snapshot: &Snapshot,
    daa: u64,
    status: &str,
    title: &str,
    controls: &str,
    footer: &str,
    paddle: Option<(i64, i64)>,
) -> Result<String> {
    let view = project(snapshot, daa)?;
    if view.world.ended() {
        return Ok(completed(&view.world.result(), title, status, "Esc quits."));
    }
    court_frame(snapshot, daa, status, title, controls, footer, paddle)
}

/// Keep the court visible even at the winning goal; the frontend controls how
/// long to hold it before displaying the detailed results.
pub fn court_frame(
    snapshot: &Snapshot,
    daa: u64,
    status: &str,
    title: &str,
    controls: &str,
    footer: &str,
    paddle: Option<(i64, i64)>,
) -> Result<String> {
    court_frame_with_presentation(
        snapshot,
        daa,
        status,
        title,
        controls,
        footer,
        CourtPresentation { preview: paddle, ..Default::default() },
    )
}

/// Presentation coordinates never feed back into transaction construction.
pub struct CourtPresentation {
    pub columns: usize,
    pub fractional_daa: f64,
    pub accepted_paddles: Option<(i64, i64)>,
    pub debug_paddles: bool,
    pub sparkles: bool,
    pub countdown_pulse: bool,
    pub preview: Option<(i64, i64)>,
    pub impact: Option<ImpactVisual>,
    pub goal_notice: Option<GoalNotice>,
    pub result: Option<Finish>,
}
impl Default for CourtPresentation {
    fn default() -> Self {
        Self { columns: COURT_WIDTH as usize, fractional_daa: 0.0, accepted_paddles: None, debug_paddles: false, sparkles: false, countdown_pulse: false, preview: None, impact: None, goal_notice: None, result: None }
    }
}

pub fn court_frame_with_presentation(
    snapshot: &Snapshot,
    daa: u64,
    status: &str,
    title: &str,
    controls: &str,
    footer: &str,
    presentation: CourtPresentation,
) -> Result<String> {
    let view = project(snapshot, daa)?;
    let mut world = view.world;
    if let Some(result) = &presentation.result {
        // Final-score paint only: never used by projection or transaction code.
        world.left_score = result.left_score;
        world.right_score = result.right_score;
    }
    let columns = presentation.columns.clamp(32, 128);
    let scale_x = |x: f64| (x * (columns - 1) as f64 / (COURT_WIDTH - 1) as f64).round() as usize;
    let mut ball_x = world.ball_x as f64;
    let mut ball_y = world.ball_y as f64;
    let mut heading_vx = world.ball_vx;
    let mut heading_vy = world.ball_vy;
    // Interpolate in screen space so a wide court gains intermediate columns,
    // rather than making every physical ball step jump several text cells.
    if !world.ended() && world.clock >= world.serve_until {
        let phase = (world.clock - world.serve_until).rem_euclid(world.ball_tick_daa);
        let sub_tick = phase as f64 + presentation.fractional_daa.clamp(0.0, 0.999999);
        if sub_tick > 0.0 {
            if let Ok(next) = project(snapshot, daa + (world.ball_tick_daa - phase) as u64) {
                let miss = next.world.left_score > world.left_score || next.world.right_score > world.right_score;
                if miss || (next.world.left_score == world.left_score
                    && next.world.right_score == world.right_score
                    && (next.world.ball_x - world.ball_x).abs() <= 2
                    && (next.world.ball_y - world.ball_y).abs() <= 2)
                {
                    let fraction = sub_tick / world.ball_tick_daa as f64;
                    let next_x = if miss {
                        world.ball_x + world.ball_vx * BALL_SPEED
                    } else {
                        next.world.ball_x
                    };
                    ball_x += (next_x - world.ball_x) as f64 * fraction;
                    // Reflect the continuous path at the wall rather than drawing
                    // a straight chord between the two reflected endpoints.
                    let vertical = (world.ball_y as f64 + world.ball_vy as f64 * fraction).rem_euclid(62.0);
                    ball_y = if vertical <= 31.0 { vertical } else { 62.0 - vertical };
                    heading_vy = if vertical <= 31.0 { world.ball_vy } else { -world.ball_vy };
                }
            }
        }
    }
    let goal = presentation.impact.filter(|impact| matches!(impact.kind, ImpactKind::Goal(_)));
    let goal_post = presentation.goal_notice.map(|notice| notice.post);
    // Continue normal flight speed through a miss, including an odd final
    // segment; the rounded consensus contact time must not slow the sprite.
    let exit = goal.and_then(|impact| impact.flight.map(|flight| (impact, flight)))
        .filter(|_| world.ended() || world.clock < world.serve_until)
        .map(|(impact, flight)| {
            let ticks = impact.age * DAA_PER_SECOND as f64 / flight.tick_daa as f64;
            let vertical = (flight.y + flight.vy as f64 * ticks).rem_euclid(62.0);
            let y = if vertical <= 31.0 { vertical } else { 62.0 - vertical };
            let vy = if vertical <= 31.0 { flight.vy } else { -flight.vy };
            let x = screen_ball_x(flight.x + flight.vx as f64 * ticks, columns);
            // Some one-cell final segments cannot leave the screen before the
            // unchanged serve deadline. Occlude them over the last eight DAA,
            // without changing their trajectory or holding up the next serve.
            let end_ticks = SERVE_DELAY_DAA as f64 / flight.tick_daa as f64;
            let end_x = screen_ball_x(flight.x + flight.vx as f64 * end_ticks, columns);
            let needs_fade = end_x + 4.0 > 0.0 && end_x < (columns * 2) as f64;
            let opacity = if needs_fade {
                ((SERVE_DELAY_DAA as f64 - impact.age * DAA_PER_SECOND as f64) / 8.0).clamp(0.0, 1.0)
            } else { 1.0 };
            (x, y, flight.vx, vy, opacity)
        });
    let crossing = exit.filter(|(x, _, _, _, _)| *x < (columns * 2) as f64 && *x + 4.0 > 0.0);
    if let Some((_, y, vx, vy, _)) = crossing {
        ball_y = y;
        heading_vx = vx;
        heading_vy = vy;
    }
    // Collision paddles belong to the SAME playback time as the ball. Latest
    // accepted positions are separate gray markers outside the playing lanes.
    let w = &world;
    let accepted = presentation.accepted_paddles.unwrap_or((w.left_y, w.right_y));
    let accepted = (accepted.0.clamp(PADDLE_MIN_Y, PADDLE_MAX_Y), accepted.1.clamp(PADDLE_MIN_Y, PADDLE_MAX_Y));
    let left_x = scale_x(LEFT_CONTACT_X as f64);
    let right_x = scale_x(RIGHT_CONTACT_X as f64);
    // A one-column inward offset leaves both silhouettes visible even where
    // accepted and predicted positions overlap. Tokens become red in Terminal.
    let preview = presentation.preview.and_then(|(side, y)| {
        let current = if side == 0 { w.left_y } else { w.right_y };
        let y = y.clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
        (y != current).then_some((if side == 0 { left_x + 1 } else { right_x - 1 }, y))
    });
    let heading = {
        if (ball_y <= 0.0 && heading_vy < 0) || (ball_y >= 31.0 && heading_vy > 0) {
            heading_vy = -heading_vy;
        }
        match (heading_vx.signum(), heading_vy.signum()) {
            (-1, -1) => '↖',
            (-1, 1) => '↙',
            (-1, _) => '←',
            (_, -1) => '↗',
            (_, 1) => '↘',
            _ => '→',
        }
    };
    let (dot_x, dot_y) = crossing.map(|(x, _, _, _, _)| (x, ball_origin(0.0, ball_y, columns).1))
        .unwrap_or_else(|| ball_origin(ball_x, ball_y, columns));
    let ball_column = dot_x.floor() as isize / 2;
    let center_x = scale_x(CENTER_X as f64);
    // Each serve has a deterministic but potentially different spawn height.
    // Keep the centre celebration clear around its actual rendered ball and
    // arrow rows, rather than assuming the opening centre height.
    let ball_row = (dot_y / 4.0).floor() as i64;
    let spawn_arrow_row = ((dot_y + 2.0) / 4.0).floor() as i64;
    let direction = if matches!(heading, '←' | '↖' | '↙') { -1 } else { 1 };
    let ahead = ball_column + direction * 3;
    // Keep the arrow beside the ball even at the screen edge, without covering it.
    let arrow_column = if (0..columns as isize).contains(&ahead) { ahead } else { ball_column - direction * 3 };
    let arrow_row = ((dot_y + 2.0) / 4.0).floor().min(15.0) as i64;
    let pulse = (daa % 18) as f64 + presentation.fractional_daa;
    let sparkle = (presentation.sparkles && !w.ended() && w.clock >= w.serve_until && crossing.is_none() && pulse < 2.5)
        .then(|| ((ball_column as f64 - direction as f64 * (2.0 + pulse)).round() as isize,
            (ball_y / 2.0 + if daa / 18 % 2 == 0 { -1.0 - pulse } else { 1.0 + pulse }).floor() as i64));
    let burst = presentation.impact.filter(|impact|
        presentation.sparkles && matches!(impact.kind, ImpactKind::Paddle(_)) && (0.0..0.25).contains(&impact.age))
        .map(|impact| {
            let inward = if impact.kind == ImpactKind::Paddle(0) { 1 } else { -1 };
            let travel = 1 + (impact.age * 12.0).floor() as isize;
            let x = scale_x(impact.x) as isize + inward * travel;
            let y = (impact.y / 2.0).floor() as i64;
            let spread = (impact.age * 12.0).ceil() as i64;
            ([(x, y - spread), (x + inward, y), (x, y + spread)], impact.age < 0.12)
        });
    let checkpoint = &snapshot.court.state;
    let mut out = String::new();
    let pace = GameSpeed::from_tick_daa(w.ball_tick_daa)
        .map(GameSpeed::label)
        .unwrap_or("Custom");
    writeln!(out, "{title}")?;
    writeln!(
        out,
        "LEFT {:2} : {:2} RIGHT  {}  DAA {}  last contact {}  y={}/{}",
        w.left_score, w.right_score, pace, daa, checkpoint.clock, accepted.0, accepted.1
    )?;
    writeln!(
        out,
        "On-chain batches: LEFT {}/12  RIGHT {}/12 | checkpoints: {}",
        snapshot.players.iter().filter(|p| p.state.side == 0).map(|p| p.state.count).sum::<usize>(),
        snapshot.players.iter().filter(|p| p.state.side == 1).map(|p| p.state.count).sum::<usize>(),
        checkpoint.resolves
    )?;
    writeln!(out, "+{}+", "-".repeat(columns))?;
    // Each character holds 2x4 Braille dots: 64 vertical subcells in 16 rows.
    // Keep a larger 4x4-dot ball visible while moving within character cells.
    // The next serve's ball and direction arrow always appear together.
    // While the outgoing goal sprite remains on court, `crossing` supplies
    // its position; once it clears, `dot_x`/`dot_y` are the new serve ball.
    let ball = raster_ball_at(dot_x, dot_y, columns, crossing.map_or(1.0, |exit| exit.4));
    let countdown_tone = (presentation.countdown_pulse && !w.ended() && w.clock < w.serve_until && exit.is_none())
        .then(|| {
            let seconds = (daa as f64 + presentation.fractional_daa.clamp(0.0, 0.999999)) / DAA_PER_SECOND as f64;
            let brightness = (1.0 + (seconds * std::f64::consts::TAU).cos()) * 0.5;
            (brightness * 3.0).floor().min(2.0) as u32
        });
    let goal_row = presentation.goal_notice.and_then(|notice| {
        notice.flight.map(|flight| flight.crossing_y(columns, notice.post)).or(notice.y)
    }).map(|y| (y / 2.0).floor() as i64);
    for row in 0_i64..16 {
        let y = 2 * row;
        let goal_edge = |side| goal_post == Some(side) && goal_row.is_none_or(|at| (row - at).abs() <= 1);
        let border = |side| {
            if goal_edge(side) { return '║'; }
            if let Some(notice) = presentation.goal_notice.filter(|notice| notice.post == side && notice.age < 0.65) {
                let radius = 2 + (notice.age * 10.0).floor() as i64;
                if goal_row.is_some_and(|at| (row - at).abs() == radius) {
                    return if notice.age < 0.35 { '╏' } else { '┊' };
                }
            }
            '|'
        };
        out.push(border(0));
        for x in 0..columns {
            let dots = ball[row as usize * columns + x];
            let post_label = goal_post.filter(|_| crossing.is_none()).and_then(|post| {
                if row != goal_row.unwrap_or(8) {
                    return None;
                }
                // The notice can outlive the serve pause. Leave both live
                // paddle lanes visible while the ball keeps drawing on top.
                let (start, label) = if post == 0 { (left_x + 2, "<GOAL!") } else { (right_x - 7, "GOAL!>") };
                x.checked_sub(start).and_then(|offset| label.chars().nth(offset))
            });
            let ch = if (!w.ended() || crossing.is_some()) && dots != 0 {
                // Private presentation tokens preserve every Braille dot;
                // Terminal decodes them to the same glyph with a warmer tone.
                char::from_u32(countdown_tone.map_or(0x2800, |tone| 0xe100 + tone * 0x100) + dots as u32).unwrap()
            } else if let Some(label) = post_label {
                label
            } else if !w.ended() && w.clock < w.serve_until && crossing.is_none() && x as isize == arrow_column && row == arrow_row {
                heading
            } else if presentation.impact.is_some_and(|impact| {
                !matches!(impact.kind, ImpactKind::Goal(_)) && x == scale_x(impact.x) && row == (impact.y / 2.0).floor() as i64
            }) {
                '✶'
            } else if x == left_x || x == right_x {
                let cell = paddle_cell(y, if x == left_x { w.left_y } else { w.right_y }, false);
                if presentation.impact.is_some_and(|impact| impact.kind == ImpactKind::Paddle(if x == left_x { 0 } else { 1 })) {
                    match cell {
                        '█' => '▉',
                        '▀' => '▔',
                        '▄' => '▁',
                        _ => cell,
                    }
                } else {
                    cell
                }
            } else if presentation.debug_paddles && x + 1 == left_x && accepted.0 != w.left_y {
                accepted_cell(y, accepted.0)
            } else if presentation.debug_paddles && x == right_x + 1 && accepted.1 != w.right_y {
                accepted_cell(y, accepted.1)
            } else if let Some((_, center)) = preview.filter(|(preview_x, _)| *preview_x == x) {
                paddle_cell(y, center, true)
            } else if let Some((_, bright)) = burst.filter(|(points, _)| points.contains(&(x as isize, row))) {
                if bright { '✧' } else { '·' }
            } else if sparkle == Some((x as isize, row)) {
                if pulse < 1.0 { '✧' } else { '·' }
            } else if presentation.goal_notice.is_some()
                && row % 2 == 0
                && (center_x - 2..=center_x + 2).contains(&x)
                && (w.ended() || presentation.result.is_some() || crossing.is_some()
                    || ((row - ball_row).abs() > 1 && (row - spawn_arrow_row).abs() > 1))
            {
                // Decorative only: during the goal exit it fills the divider;
                // once the serve ball appears, its corridor stays clear.
                // Finished games have no next serve, so retain the full word.
                let letter = "KASPA".chars().nth((x - (center_x - 2)) as usize).expect("goal word bounds");
                let color_phase = (daa / 5 % 3) as u32;
                char::from_u32(0xe400 + color_phase * 0x100 + letter as u32).expect("goal token")
            } else if x == center_x && row % 2 == 0 {
                ':'
            } else {
                ' '
            };
            out.push(ch);
        }
        out.push(border(1));
        out.push('\n');
    }
    writeln!(out, "+{}+", "-".repeat(columns))?;
    writeln!(out, "{controls}")?;
    let impact_label = presentation.impact.filter(|impact| !matches!(impact.kind, ImpactKind::Goal(_)));
    if let Some(notice) = presentation.goal_notice {
        write!(out, "{} SCORES!", if notice.post == 0 { "RIGHT" } else { "LEFT" })?;
    } else if let Some(impact) = impact_label {
        match impact.kind {
            ImpactKind::Goal(_) => {},
            ImpactKind::Paddle(0) => write!(out, "LEFT PADDLE HIT  ->  rebound")?,
            ImpactKind::Paddle(_) => write!(out, "<-  RIGHT PADDLE HIT  |  rebound")?,
            ImpactKind::Wall => write!(out, "{} WALL BOUNCE", if impact.y < 16.0 { "TOP" } else { "BOTTOM" })?,
        }
    }
    if !w.ended() && w.clock < w.serve_until {
        if impact_label.is_some() || presentation.goal_notice.is_some() {
            write!(out, " | ")?;
        }
        let remaining = w.serve_until - w.clock;
        if !presentation.debug_paddles {
            let label = if w.left_score + w.right_score == 0 { "Game will start" } else { "Next serve" };
            write!(out, "{label} in {:.1}s ({remaining} DAA); paddles active", remaining as f64 / DAA_PER_SECOND as f64)?;
        } else if w.left_score + w.right_score == 0 {
            write!(
                out,
                "Game will start in {:.1}s... ({remaining} DAA; paddles active; dots=ball)",
                remaining as f64 / DAA_PER_SECOND as f64
            )?;
        } else {
            write!(out, "Next serve in {remaining} DAA; paddles active; dots=ball")?;
        }
    } else if impact_label.is_none() && presentation.goal_notice.is_none() {
        write!(out, "{footer}")?;
    }
    writeln!(out)?;
    // Keep actionable input/RPC errors visible during the opening countdown.
    writeln!(out, "{status:<76}")?;
    Ok(out)
}

fn paddle_cell(y: i64, center: i64, preview: bool) -> char {
    match ((y - center).abs() <= PADDLE_RADIUS, (y + 1 - center).abs() <= PADDLE_RADIUS, preview) {
        (true, true, false) => '█',
        (true, false, false) => '▀',
        (false, true, false) => '▄',
        (true, true, true) => '▓',
        (true, false, true) => '▴',
        (false, true, true) => '▾',
        _ => ' ',
    }
}

/// A dot mask per character. Braille dot numbering differs from row-major order.
fn raster_ball_at(dot_x: f64, dot_y: f64, columns: usize, opacity: f64) -> Vec<u8> {
    let mut masks = vec![0; columns * 16];
    let bits = [[0, 3], [1, 4], [2, 5], [6, 7]];
    let (base_x, base_y) = (dot_x.floor() as isize, dot_y.floor() as isize);
    let (fraction_x, fraction_y) = (dot_x - dot_x.floor(), dot_y - dot_y.floor());
    let order = [0, 2, 1, 3];
    let fade = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    // Row translations followed by final-column translations are bijective:
    // all sixteen dots survive, with quarter-dot centroid steps and no temporal
    // dithering. At integer origins this is the original solid 4x4 sprite.
    for dy in 0..4 {
        for dx in 0..4 {
            if opacity <= (fade[dy][dx] as f64 + 0.5) / 16.0 {
                continue;
            }
            let x = base_x + dx as isize + isize::from(fraction_x >= (order[dy] as f64 + 0.5) / 4.0);
            let y = base_y + dy as isize
                + isize::from(fraction_y >= (order[x.rem_euclid(4) as usize] as f64 + 0.5) / 4.0);
            if (0..columns as isize * 2).contains(&x) && (0..64).contains(&y) {
                let (x, y) = (x as usize, y as usize);
                masks[(y / 4) * columns + x / 2] |= 1 << bits[y % 4][x % 2];
            }
        }
    }
    masks
}

// The sprite occupies four dots. Map the horizontal contact
// planes to sprite origins just inside the actual rounded paddle columns, so
// the visible edge touches the paddle instead of painting over it. Extend this
// SAME scale beyond the posts so a missed ball keeps its screen-space slope.
fn screen_ball_x(x: f64, columns: usize) -> f64 {
    let post_column = |post: i64| (post as f64 * (columns - 1) as f64 / (COURT_WIDTH - 1) as f64).round();
    let left = 2.0 * (post_column(LEFT_CONTACT_X) + 1.0);
    let right = 2.0 * post_column(RIGHT_CONTACT_X) - 4.0;
    left + (right - left) * (x - LEFT_CONTACT_X as f64) / (RIGHT_CONTACT_X - LEFT_CONTACT_X) as f64
}

fn ball_origin(x: f64, y: f64, columns: usize) -> (f64, f64) {
    (screen_ball_x(x, columns).clamp(0.0, (columns * 2 - 4) as f64), (y * 2.0 - 1.0).clamp(0.0, 60.0))
}

fn accepted_cell(y: i64, center: i64) -> char {
    match paddle_cell(y, center, false) {
        '█' => '░',
        '▀' => '╵',
        '▄' => '╷',
        _ => ' ',
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImpactKind {
    Paddle(i64),
    Wall,
    Goal(i64),
}
#[derive(Clone, Copy)]
pub struct ImpactVisual {
    pub kind: ImpactKind,
    pub x: f64,
    pub y: f64,
    pub age: f64,
    pub flight: Option<GoalVisual>,
}

/// Normal incoming velocity, retained before scoring selects a new serve.
/// `y` is unfolded so reflection continues through a goal beside either wall.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GoalVisual {
    pub x: f64,
    pub y: f64,
    pub vx: i64,
    pub vy: i64,
    pub tick_daa: i64,
}
impl GoalVisual {
    /// Continue the outgoing trajectory to first contact with the visible goal
    /// edge, using the SAME screen transform and wall reflections as the ball.
    pub fn crossing_y(self, columns: usize, post: i64) -> f64 {
        let columns = columns.clamp(32, 128);
        let origin = screen_ball_x(self.x, columns);
        let velocity = screen_ball_x(self.x + self.vx as f64, columns) - origin;
        let edge = if post == 0 { 0.0 } else { (columns * 2 - 4) as f64 };
        let ticks = if velocity.abs() > f64::EPSILON { ((edge - origin) / velocity).max(0.0) } else { 0.0 };
        let unfolded = (self.y + self.vy as f64 * ticks).rem_euclid(62.0);
        if unfolded <= 31.0 { unfolded } else { 62.0 - unfolded }
    }
}

/// The scored end is independent of the outgoing sprite's playback lifetime.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GoalNotice {
    pub post: i64,
    pub y: Option<f64>,
    pub flight: Option<GoalVisual>,
    /// Wall time since this notice became visible, for the brief border ripple.
    pub age: f64,
}

pub const GOAL_NOTICE_HOLD: Duration = Duration::from_secs(3);

#[derive(Default)]
struct GoalNotices {
    last: Option<(i64, i64)>,
    seen: std::collections::HashSet<(i64, i64, i64)>,
    pending: std::collections::VecDeque<GoalNotice>,
    visible: Option<(GoalNotice, Instant)>,
}
impl GoalNotices {
    fn reset(&mut self) {
        self.last = None;
        self.pending.clear();
        self.visible = None;
    }
    fn observe(&mut self, left: i64, right: i64, goal_daa: i64, y: Option<f64>, flight: Option<GoalVisual>) {
        let unseen = !self.seen.contains(&(left, right, goal_daa));
        if let Some((previous_left, previous_right)) = self.last {
            if left < previous_left || right < previous_right {
                // Late validated paddle commands can contradict a projected
                // goal without changing the chain epoch or playback DAA.
                self.pending.clear();
                self.visible = None;
            } else {
                for (score, previous, post) in [(left, previous_left, 1), (right, previous_right, 0)] {
                    if score > previous && unseen {
                        self.pending.push_back(GoalNotice { post, y, flight, age: 0.0 });
                    }
                }
            }
        }
        // A first frame is a baseline, including after a rewind/reorg. Keep
        // exact goal identities so a corrected provisional goal cannot suppress
        // a different later goal reaching the same score.
        self.last = Some((left, right));
        self.seen.insert((left, right, goal_daa));
    }
    fn sample(&mut self, now: Instant) -> Option<GoalNotice> {
        if self.visible.is_some_and(|(_, since)| now.saturating_duration_since(since) >= GOAL_NOTICE_HOLD) {
            self.visible = None;
        }
        if self.visible.is_none() {
            self.visible = self.pending.pop_front().map(|notice| (notice, now));
        }
        self.visible.map(|(mut notice, since)| {
            notice.age = now.saturating_duration_since(since).as_secs_f64();
            notice
        })
    }
}

/// Ephemeral presentation state: flashes use wall time, goal motion uses
/// continuous playback time. It never participates in simulation or settlement.
#[derive(Default)]
pub struct CourtEffects {
    notices: GoalNotices,
    last: Option<World>,
    snapshot: Option<Snapshot>,
    daa: u64,
    active: Option<(ImpactKind, f64, f64, Instant)>,
    flight: Option<GoalVisual>,
    goal_daa: i64,
    goal_age: f64,
    final_clock: Option<(Instant, f64)>,
}
impl CourtEffects {
    /// Discard contradicted paint, retaining only score deduplication history.
    pub fn reset_timeline(&mut self) {
        let mut notices = std::mem::take(&mut self.notices);
        notices.reset();
        *self = Self { notices, ..Default::default() };
    }
    pub fn goal_notice(&mut self, now: Instant) -> Option<GoalNotice> {
        self.notices.sample(now)
    }
    fn goal(&mut self, before: &World, post: i64, now: Instant, at: i64, late_daa: u64) {
        let x = if post == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as f64;
        let distance = (x - before.ball_x as f64).abs() as i64;
        let ticks = ((distance + BALL_SPEED - 1) / BALL_SPEED).max(1);
        let unfolded = before.ball_y as f64 + before.ball_vy as f64 * ticks as f64;
        let vertical = unfolded.rem_euclid(62.0);
        let y = if vertical <= 31.0 { vertical } else { 62.0 - vertical };
        self.flight = Some(GoalVisual {
            x: before.ball_x as f64 + (before.ball_vx * BALL_SPEED * ticks) as f64,
            y: unfolded,
            vx: before.ball_vx * BALL_SPEED,
            vy: before.ball_vy,
            tick_daa: before.ball_tick_daa,
        });
        let age = Duration::from_secs_f64(late_daa as f64 / DAA_PER_SECOND as f64);
        self.active = Some((ImpactKind::Goal(post), x, y, now.checked_sub(age).unwrap_or(now)));
        self.goal_daa = at;
        self.goal_age = age.as_secs_f64();
        self.final_clock = None;
    }
    fn before_goal(&self, snapshot: &Snapshot, at: i64, scores: (i64, i64), post: i64) -> Option<World> {
        let at = u64::try_from(at).ok()?.saturating_sub(1);
        let expected = (scores.0 - i64::from(post == 1), scores.1 - i64::from(post == 0));
        std::iter::once(snapshot).chain(self.snapshot.iter()).find_map(|candidate| {
            let world = project(candidate, at).ok()?.world;
            ((world.left_score, world.right_score) == expected && !world.ended()).then_some(world)
        })
    }
    pub fn observe(&mut self, snapshot: &Snapshot, world: &World, daa: u64, now: Instant) {
        if daa < self.daa {
            self.reset_timeline();
        }
        let goal_daa = if world.ended() { world.clock } else { world.serve_until - SERVE_DELAY_DAA };
        if let Some(previous) = self.last.clone() {
            if world.left_score < previous.left_score || world.right_score < previous.right_score {
                self.active = None;
                self.flight = None;
                self.final_clock = None;
            } else if world.left_score > previous.left_score || world.right_score > previous.right_score {
                let post = if world.right_score > previous.right_score { 0 } else { 1 };
                let scores = (world.left_score, world.right_score);
                let before = (!self.notices.seen.contains(&(scores.0, scores.1, goal_daa)))
                    .then(|| self.before_goal(snapshot, goal_daa, scores, post)).flatten();
                self.active = None;
                self.flight = None;
                self.final_clock = None;
                if let Some(before) = before {
                    self.goal(&before, post, now, goal_daa, world.clock.saturating_sub(goal_daa).max(0) as u64);
                }
            } else if world.serve_until == previous.serve_until && world.clock >= world.serve_until {
                if world.ball_vx != previous.ball_vx {
                    let side = if previous.ball_vx < 0 { 0 } else { 1 };
                    self.active = Some((ImpactKind::Paddle(side), if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as f64, world.ball_y as f64, now));
                } else if world.ball_vy.signum() != previous.ball_vy.signum() && world.ball_vy != 0 && previous.ball_vy != 0 {
                    self.active = Some((ImpactKind::Wall, world.ball_x as f64, if previous.ball_vy < 0 { 0.0 } else { 31.0 }, now));
                }
            }
        }
        let goal_y = self.active.and_then(|(kind, _, y, _)| {
            (matches!(kind, ImpactKind::Goal(_)) && self.goal_daa == goal_daa).then_some(y)
        });
        self.notices.observe(world.left_score, world.right_score, goal_daa, goal_y, goal_y.and(self.flight));
        self.last = Some(world.clone());
        self.snapshot = Some(snapshot.clone());
        self.daa = daa;
    }
    /// Use the same continuous playback coordinate as live-ball interpolation.
    /// Wall time controls only collision flashes and post-settlement cosmetics.
    pub fn sample(&mut self, daa: u64, fractional_daa: f64, now: Instant) -> Option<ImpactVisual> {
        self.goal_age = ((daa as f64 - self.goal_daa as f64 + fractional_daa.clamp(0.0, 0.999999))
            / DAA_PER_SECOND as f64).max(0.0);
        self.sample_at(now, self.goal_age)
    }
    fn sample_at(&self, now: Instant, goal_age: f64) -> Option<ImpactVisual> {
        let (kind, x, y, at) = self.active?;
        let age = if matches!(kind, ImpactKind::Goal(_)) { goal_age } else { now.saturating_duration_since(at).as_secs_f64() };
        let duration = match kind {
            ImpactKind::Goal(_) => {
                // Playback can run faster than wall time. Never cover a serve
                // that has already resumed, even if the exit is still on screen.
                if self.last.as_ref().is_some_and(|w| !w.ended() && w.clock >= w.serve_until) {
                    return None;
                }
                SERVE_DELAY_DAA as f64 / DAA_PER_SECOND as f64
            },
            ImpactKind::Paddle(_) => 0.25,
            ImpactKind::Wall => 0.18,
        };
        (age < duration).then_some(ImpactVisual { kind, x, y, age, flight: matches!(kind, ImpactKind::Goal(_)).then_some(self.flight).flatten() })
    }
    /// Keep animating the final goal even when settlement already replaced the
    /// live Court. Paint the accepted result on the last available visual court.
    pub fn finished_frame(
        &mut self,
        result: &Finish,
        title: &str,
        status: &str,
        footer: &str,
        columns: usize,
        now: Instant,
    ) -> Option<String> {
        if result.winner < 0 {
            self.notices.reset();
            return None;
        }
        let Some(previous) = self.last.clone() else {
            self.notices.observe(result.left_score, result.right_score, result.end_daa, None, None);
            return None;
        };
        if result.left_score < previous.left_score || result.right_score < previous.right_score {
            self.active = None;
            self.flight = None;
            self.final_clock = None;
            if let Some(last) = &mut self.last {
                last.left_score = result.left_score;
                last.right_score = result.right_score;
            }
        } else if result.left_score > previous.left_score || result.right_score > previous.right_score {
            let scores = (result.left_score, result.right_score);
            let post = if result.right_score > previous.right_score { 0 } else { 1 };
            let before = (!self.notices.seen.contains(&(scores.0, scores.1, result.end_daa)))
                .then(|| self.before_goal(self.snapshot.as_ref()?, result.end_daa, scores, post)).flatten();
            self.active = None;
            self.flight = None;
            self.final_clock = None;
            if let Some(before) = before {
                self.goal(&before, post, now, result.end_daa, 0);
            }
            if let Some(last) = &mut self.last {
                last.left_score = result.left_score;
                last.right_score = result.right_score;
            }
        }
        let goal_y = self.active.and_then(|(kind, _, y, _)| {
            (matches!(kind, ImpactKind::Goal(_)) && self.goal_daa == result.end_daa).then_some(y)
        });
        self.notices.observe(result.left_score, result.right_score, result.end_daa, goal_y, goal_y.and(self.flight));
        let (at, age) = *self.final_clock.get_or_insert((now, self.goal_age));
        let impact = self.sample_at(now, age + now.saturating_duration_since(at).as_secs_f64());
        let goal_notice = self.goal_notice(now);
        court_frame_with_presentation(
            self.snapshot.as_ref()?,
            self.daa,
            status,
            title,
            "Game ended; Esc: quit",
            footer,
            CourtPresentation { columns, impact, goal_notice, result: Some(result.clone()), ..Default::default() },
        )
        .ok()
    }
}

#[cfg(test)]
mod visual_collision {
    use super::*;
    use crate::engine::{BALL_RADIUS, Cell, empty_queues};
    use kaspa_consensus_core::{Hash, tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry}};

    fn snapshot(side: i64, vy: i64, offset: i64) -> Snapshot {
        let mut world = World::opening(vec![], vec![]);
        world.started = true;
        world.clock = 100;
        world.serve_until = 100;
        world.ball_tick_daa = 8; // Keep the visual fixture's fractional/contact samples fixed.
        world.ball_vx = if side == 0 { -1 } else { 1 };
        world.ball_vy = vy;
        world.ball_x = if side == 0 { LEFT_CONTACT_X + BALL_SPEED } else { RIGHT_CONTACT_X - BALL_SPEED };
        world.ball_y = 16 + offset - vy;
        let queues = empty_queues(&world.left, &world.right);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([0; 32]), 0);
        let utxo = UtxoEntry::new(1, ScriptPublicKey::new(0, Default::default()), 100, false, None);
        Snapshot {
            court: Cell { outpoint, utxo: utxo.clone(), state: world },
            players: std::array::from_fn(|i| Cell { outpoint, utxo: utxo.clone(), state: queues[i].clone() }),
        }
    }

    // Decode the emitted Braille rather than assuming the sprite's origin is
    // its extent. Coordinates exclude the text border and include all 16 dots.
    fn dots(frame: &str) -> Vec<(usize, usize)> {
        let bits = [[0, 3], [1, 4], [2, 5], [6, 7]];
        let mut dots = vec![];
        for (row, line) in frame.lines().skip(4).take(16).enumerate() {
            for (column, ch) in line.chars().skip(1).enumerate() {
                let Some(mask) = (ch as u32).checked_sub(0x2800).filter(|mask| *mask <= 255) else { continue };
                for (dy, pair) in bits.iter().enumerate() {
                    for (dx, bit) in pair.iter().enumerate() {
                        if mask & (1 << bit) != 0 {
                            dots.push((2 * column + dx, 4 * row + dy));
                        }
                    }
                }
            }
        }
        assert_eq!(dots.len(), 16, "the whole ball must remain visible");
        dots
    }

    fn render(snapshot: &Snapshot, daa: u64, presentation: CourtPresentation) -> String {
        court_frame_with_presentation(snapshot, daa, "", "PONG", "controls", "footer", presentation).unwrap()
    }

    fn faces(columns: usize) -> (usize, usize) {
        let column = |post| (post as f64 * (columns - 1) as f64 / (COURT_WIDTH - 1) as f64).round() as usize;
        (2 * (column(LEFT_CONTACT_X) + 1), 2 * column(RIGHT_CONTACT_X))
    }

    #[test]
    fn final_goal_keeps_every_center_word_after_the_outgoing_ball_expires() {
        let now = Instant::now();
        for side in 0..=1 {
            let mut s = snapshot(side, 0, -10);
            if side == 0 { s.court.state.right_score = WIN_SCORE - 1; }
            else { s.court.state.left_score = WIN_SCORE - 1; }
            let result = project(&s, 108).unwrap().world.result();
            for columns in [58, 80, 128] {
                let mut effects = CourtEffects::default();
                effects.observe(&s, &project(&s, 107).unwrap().world, 107, now);
                let center = (CENTER_X as f64 * (columns - 1) as f64 / (COURT_WIDTH - 1) as f64).round() as usize;
                for age in [0, 500, 1000, 2000, 2999] {
                    let frame = effects.finished_frame(&result, "PONG", "", "", columns, now + Duration::from_millis(age)).unwrap();
                    for (row, line) in frame.lines().skip(4).take(16).enumerate().step_by(2) {
                        let word: String = line.chars().skip(1 + center - 2).take(5).map(|ch| {
                            let code = ch as u32;
                            if (0xe400..=0xe6ff).contains(&code) { char::from_u32((code - 0xe400) % 0x100).unwrap() }
                            else { ch }
                        }).collect();
                        assert_eq!(word, "KASPA", "side={side}, columns={columns}, age={age}, row={row}");
                    }
                    if age >= 2000 { assert!(!frame.chars().any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch))); }
                }
            }
        }
    }

    #[test]
    fn rendered_ball_touches_both_paddle_faces_at_every_width_and_hit_angle() {
        for side in 0..=1 {
            for vy in -2..=2 {
                for offset in -(PADDLE_RADIUS + BALL_RADIUS)..=PADDLE_RADIUS + BALL_RADIUS {
                    let snapshot = snapshot(side, vy, offset);
                    let contact = project(&snapshot, 108).unwrap().world;
                    assert_eq!(contact.ball_x, if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X });
                    assert_eq!(contact.ball_vx, -snapshot.court.state.ball_vx);
                    assert_eq!(contact.ball_vy, offset.clamp(-2, 2));
                    for columns in 32..=128 {
                        let frame = render(&snapshot, 108, CourtPresentation { columns, ..Default::default() });
                        let ball = dots(&frame);
                        let (left, right) = faces(columns);
                        assert!(ball.iter().all(|&(x, _)| left <= x && x < right));
                        if side == 0 {
                            assert_eq!(ball.iter().map(|p| p.0).min(), Some(left));
                        } else {
                            assert_eq!(ball.iter().map(|p| p.0).max(), Some(right - 1));
                        }
                        let post = if side == 0 { left / 2 - 1 } else { right / 2 };
                        let paddle: Vec<_> = frame.lines().skip(4).take(16).map(|row| row.chars().nth(post + 1).unwrap()).collect();
                        assert_eq!(paddle, (0..16).map(|row| paddle_cell(2 * row, 16, false)).collect::<Vec<_>>());
                    }
                }
            }
        }
    }

    #[test]
    fn fractional_frames_approach_then_leave_without_entering_either_paddle() {
        for columns in [32, 58, 80, 128] {
            let (left, right) = faces(columns);
            for side in 0..=1 {
                for vy in -2..=2 {
                    let snapshot = snapshot(side, vy, vy);
                    let mut distances = vec![];
                    for (daa, fractional_daa) in [(100, 0.0), (103, 0.5), (107, 0.999), (108, 0.0), (108, 0.999), (112, 0.5), (116, 0.0)] {
                        let ball = dots(&render(&snapshot, daa, CourtPresentation { columns, fractional_daa, ..Default::default() }));
                        assert!(ball.iter().all(|&(x, _)| left <= x && x < right));
                        distances.push(if side == 0 {
                            ball.iter().map(|p| p.0).min().unwrap() - left
                        } else {
                            right - 1 - ball.iter().map(|p| p.0).max().unwrap()
                        });
                    }
                    assert_eq!(distances[3], 0);
                    assert!(distances[..4].windows(2).all(|pair| pair[0] >= pair[1]));
                    assert!(distances[3..].windows(2).all(|pair| pair[0] <= pair[1]));
                    assert!(distances[0] > 0 && distances[6] > 0);
                }
            }
        }
    }

    #[test]
    fn goal_crossing_keeps_all_dots_until_clipping_at_the_court_edge() {
        for columns in [32, 58, 80, 128] {
            for side in 0..=1 {
                let snapshot = snapshot(side, 0, 0);
                for age in [0.0, 0.2, 0.4] {
                    let x = if side == 0 { LEFT_CONTACT_X } else { RIGHT_CONTACT_X } as f64;
                    let flight = GoalVisual { x, y: 4.0, vx: if side == 0 { -2 } else { 2 }, vy: 0, tick_daa: 8 };
                    let impact = Some(ImpactVisual { kind: ImpactKind::Goal(side), x, y: 4.0, age, flight: Some(flight) });
                    let mut serving = snapshot.clone();
                    serving.court.state.serve_until = 128;
                    dots(&render(&serving, 108, CourtPresentation { columns, impact, ..Default::default() }));
                }
            }
        }
    }
}
