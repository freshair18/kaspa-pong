//! A small terminal frontend on its own thread: RPC latency never blocks keys
//! or quitting. Local paddle previews reconcile with accepted chain state.
use super::{
    ascii,
    engine::*,
    node::{
        audit::{ReorgEvent, TransactionRecord},
        fees::coins,
        tracker::Stage,
    },
    terminal::{Terminal, PromptKey},
};
use std::{
    io::BufRead,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

// Local intent survives RPC delays and unavailable lanes until accepted or the match ends.
pub const SNAPSHOT_LIFETIME: Duration = Duration::from_secs(10);
pub const FINAL_BOARD_HOLD: Duration = Duration::from_secs(2);
pub const PLAYBACK_INITIAL_BUFFER_DAA: u64 = 2;
pub const PLAYBACK_MAX_BUFFER_DAA: u64 = 15;
static PLAYBACK_BASE: AtomicU64 = AtomicU64::new(PLAYBACK_INITIAL_BUFFER_DAA);
pub fn playback_base() -> u64 { PLAYBACK_BASE.load(Ordering::Relaxed) }
pub fn set_playback_base(value: u64) { PLAYBACK_BASE.store(value.min(PLAYBACK_MAX_BUFFER_DAA), Ordering::Relaxed); }
/// A fixed collection window, not a debounce: repeat presses never reset it.
/// OS autorepeat may start later than this bound, so one hold can still split.
pub const MOVEMENT_BATCH_WINDOW: Duration = Duration::from_millis(250);

#[cfg(test)]
mod movement_collection_tests {
    use super::*;
    use std::collections::VecDeque;
    fn press(preview: &mut InputPreview, queue: &mut VecDeque<Movement>, at: Instant, direction: i64) {
        let movement = Movement { pressed: at, direction };
        preview.push(movement.clone());
        queue.push_back(movement);
    }
    #[test]
    fn first_press_waits_exactly_one_collection_window() {
        let now = Instant::now();
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        press(&mut preview, &mut queue, now, 1);
        assert!(preview.ready_movement_batch(&queue, now).is_empty());
        assert!(preview.ready_movement_batch(&queue, now + Duration::from_millis(100)).is_empty());
        assert!(preview.ready_movement_batch(&queue, now + Duration::from_millis(249)).is_empty());
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(250)).len(), 1);
    }
    #[test]
    fn repeats_grow_the_batch_without_restarting_its_deadline() {
        let now = Instant::now();
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        for age in [0, 100, 200, 249] { press(&mut preview, &mut queue, now + Duration::from_millis(age), 1); }
        assert!(preview.ready_movement_batch(&queue, now + Duration::from_millis(249)).is_empty());
        press(&mut preview, &mut queue, now + Duration::from_millis(250), 1);
        assert_eq!(preview.ready_movement_batch(&queue, now + MOVEMENT_BATCH_WINDOW).len(), 5);
        press(&mut preview, &mut queue, now + Duration::from_millis(251), 1);
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(251)).len(), 6);
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_secs(1)).len(), 6);
    }
    #[test]
    fn delayed_autorepeat_can_join_a_still_unissued_batch() {
        let now = Instant::now();
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        press(&mut preview, &mut queue, now, 1);
        assert_eq!(preview.ready_movement_batch(&queue, now + MOVEMENT_BATCH_WINDOW).len(), 1);
        for age in [500, 530, 560] { press(&mut preview, &mut queue, now + Duration::from_millis(age), 1); }
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(560)).len(), 4);
        queue.pop_front();
        assert!(preview.ready_movement_batch(&queue, now + Duration::from_millis(749)).is_empty());
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(750)).len(), 3);
    }
    #[test]
    fn full_batch_can_send_early_and_direction_changes_stay_separate() {
        let now = Instant::now();
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        for age in 0..30 { press(&mut preview, &mut queue, now + Duration::from_millis(age), 1); }
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(29)).len(), 27);
        queue.clear();
        preview.clear();
        press(&mut preview, &mut queue, now, 1);
        press(&mut preview, &mut queue, now + Duration::from_millis(20), -1);
        assert_eq!(preview.ready_movement_batch(&queue, now + MOVEMENT_BATCH_WINDOW).len(), 1);
    }
    #[test]
    fn issued_retry_has_no_new_delay_and_keeps_its_original_members() {
        let now = Instant::now();
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        for age in [0, 20, 40] { press(&mut preview, &mut queue, now + Duration::from_millis(age), -1); }
        let transaction = kaspa_consensus_core::Hash::from_bytes([8; 32]);
        for movement in &queue { preview.submitted(movement.pressed, transaction); preview.requeue(movement); }
        press(&mut preview, &mut queue, now + Duration::from_millis(60), -1);
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_millis(60)).len(), 3);
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_secs(1)).len(), 3);
        for movement in queue.iter().take(3) {
            preview.submitted(movement.pressed, transaction);
            preview.requeue(movement);
        }
        assert_eq!(preview.ready_movement_batch(&queue, now + Duration::from_secs(1)).len(), 3);
    }
    #[test]
    fn collection_wait_keeps_preview_and_unsent_cancellation_immediate() {
        let now = Instant::now();
        let world = World::opening(vec![1; 32], vec![2; 32]);
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        press(&mut preview, &mut queue, now, 1);
        assert!(preview.ready_movement_batch(&queue, now).is_empty());
        assert_eq!(preview.paddle(&world, 0, &[], now), (world.left_y + 1, 1));
        let opposite = now + Duration::from_millis(1);
        press(&mut preview, &mut queue, opposite, -1);
        assert!(preview.ready_movement_batch(&queue, opposite).is_empty());
        assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 2);
        assert!(queue.is_empty());
        assert_eq!(preview.paddle(&world, 0, &[], opposite), (world.left_y, 0));
    }

    #[test]
    fn cancellation_inspection_keeps_intent_reserved_and_prunes_boundary_noops() {
        let now = Instant::now() - Duration::from_secs(1);
        let mut world = World::opening(vec![], vec![]);
        let mut preview = InputPreview::default();
        let mut queue = VecDeque::new();
        press(&mut preview, &mut queue, now, 1);
        let prepared = preview.ready_movement_batch(&queue, Instant::now());
        press(&mut preview, &mut queue, now + Duration::from_millis(300), 1);
        assert!(!InputPreview::same_batch(&prepared, &preview.batch_after_cancellation(&queue, &world, 0, &[])));
        press(&mut preview, &mut queue, now + Duration::from_millis(301), -1);
        press(&mut preview, &mut queue, now + Duration::from_millis(302), -1);
        assert!(preview.batch_after_cancellation(&queue, &world, 0, &[]).is_empty());
        assert_eq!(queue.len(), 4, "inspection alone cannot release reserved intent");
        assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 4);
        world.left_y = PADDLE_MAX_Y;
        for age in 0..100 { press(&mut preview, &mut queue, now + Duration::from_millis(age), 1); }
        assert_eq!(preview.cancel_unsent(&mut queue, &world, 0, &[]), 100);
        assert!(queue.is_empty());
        assert_eq!(preview.paddle(&world, 0, &[], Instant::now()), (PADDLE_MAX_Y, 0));
    }

    #[test]
    fn final_capture_drains_and_commits_under_the_enqueue_mutex() {
        let (send, input) = mpsc::channel();
        let preview = Arc::new(Mutex::new(InputPreview::default()));
        let ui = Frontend { input, quit: Arc::new(AtomicBool::new(false)), stop: Arc::new(AtomicBool::new(false)), display: None, worker: None,
            clock: Arc::new(Mutex::new(None)), preview: preview.clone(), rematch: Arc::new(Mutex::new(RematchShared::default())) };
        let movement = Movement { pressed: Instant::now() - Duration::from_secs(1), direction: 1 };
        {
            let mut state = preview.lock().unwrap();
            state.push(movement.clone());
            send.send(movement).unwrap();
        }
        let mut queue = VecDeque::new();
        let (count, received) = ui.with_inputs(&mut queue, |state, queue| {
            assert!(preview.try_lock().is_err(), "enqueue cannot interleave with the final commit");
            assert_eq!(queue.len(), 1);
            let batch = state.ready_movement_batch(queue, Instant::now());
            assert_eq!(batch.len(), 1);
            state.submitted(batch[0].pressed, kaspa_consensus_core::Hash::from_bytes([9; 32]));
            queue.pop_front();
            batch.len()
        });
        assert_eq!(count, 1);
        assert_eq!(received.len(), 1);
        assert!(queue.is_empty());
        assert!(preview.try_lock().is_ok());
    }
}

/// Approval decoders are shared with tests; only a fresh prompt Enter approves.
pub fn review_key(key: PromptKey) -> Option<bool> {
    match key {
        PromptKey::Enter => Some(true),
        PromptKey::Back | PromptKey::Exit | PromptKey::Character('q' | 'Q') => Some(false),
        _ => None,
    }
}
pub fn review_decision(key: PromptKey, ready: bool) -> Option<bool> {
    match review_key(key) { Some(true) if !ready => None, decision => decision }
}
/// Empty input is EOF, not approval. Whitespace or other text also rejects.
pub fn review_line(line: &str) -> bool { matches!(line, "\n" | "\r\n") }
pub fn review_frame(review: &str, status: &str, approved: bool, ready: bool) -> String {
    let controls = if approved { "Confirmed. Joining this game; q/Esc: cancel before submission." }
        else if !ready { "Preparing your transaction. Wait for confirmation to become available." }
        else { "Enter: approve joining this game; q/Esc: cancel." };
    format!("PONG\n\n{review}\n\n{controls}\n\n{status}")
}

/// Setup owns the sole terminal reader until gameplay takes over. A back
/// request can cancel read-only setup work; never cancel an RPC submission here.
pub struct Setup {
    back: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    worker: Option<JoinHandle<()>>,
    approved: Arc<AtomicBool>,
    confirming: Arc<AtomicBool>,
    review: bool,
}
impl Setup {
    pub fn new(enabled: bool) -> Result<Self> { Self::start(enabled, false, None) }
    pub fn transfer() -> Result<Self> { Self::start(true, true, None) }
    pub fn join_review(enabled: bool, review: String) -> Result<Self> {
        Self::start(enabled, false, Some(review))
    }
    fn start(enabled: bool, transfer: bool, review: Option<String>) -> Result<Self> {
        let back = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(String::from("Preparing/synchronizing game setup. q/Esc: main menu.")));
        let approved = Arc::new(AtomicBool::new(false));
        let confirming = Arc::new(AtomicBool::new(false));
        let mut setup = Self { back: back.clone(), stop: stop.clone(), status: status.clone(), worker: None,
            approved: approved.clone(), confirming: confirming.clone(), review: review.is_some() };
        if !enabled {
            if let Some(review) = &review { eprintln!("{review}"); }
        }
        if enabled {
            let terminal = Terminal::new()?;
            if review.is_some() { terminal.begin_prompt()?; }
            setup.worker = Some(std::thread::spawn(move || {
                let mut confirmation_ready = false;
                while !stop.load(Ordering::Relaxed) {
                    if review.is_some() {
                        let just_armed = !confirmation_ready && confirming.load(Ordering::Acquire);
                        if just_armed {
                            // Discard Enter events queued before the transaction was ready.
                            match terminal.commands() {
                                Ok(commands) if commands.iter().any(|command| command == "q") => back.store(true, Ordering::Relaxed),
                                Err(_) => { back.store(true, Ordering::Relaxed); break; }
                                _ => {}
                            }
                            if terminal.begin_prompt().is_err() { back.store(true, Ordering::Relaxed); break; }
                            confirmation_ready = true;
                        }
                        if !just_armed { match terminal.prompt_key(Duration::ZERO) {
                            Ok(Some(key)) => match review_decision(key, confirmation_ready) {
                                Some(true) => approved.store(true, Ordering::Relaxed),
                                Some(false) => back.store(true, Ordering::Relaxed),
                                _ => {}
                            },
                            Err(_) => { back.store(true, Ordering::Relaxed); break; }
                            _ => {}
                        } }
                    } else {
                        match terminal.commands() {
                            Ok(commands) if commands.iter().any(|command| command == "q") => back.store(true, Ordering::Relaxed),
                            Err(_) => { back.store(true, Ordering::Relaxed); break; }
                            _ => {}
                        }
                    }
                    let status = if back.load(Ordering::Relaxed) {
                        String::from("Returning to menu; only an already-authorized submission may need to finish...")
                    } else {
                        status.lock().unwrap_or_else(|e| e.into_inner()).clone()
                    };
                    let recovery = super::node::hosted::status();
                    let frame = if let Some(review) = &review {
                        review_frame(review, &status, approved.load(Ordering::Relaxed), confirmation_ready)
                    } else if transfer { format!("PONG\n\n{status}") } else { format!("PONG\n\n{status}\n\n{recovery}\nOnly the joining player who created the Lobby can cancel it before the host starts.\nThe host cannot cancel it. Started matches cannot be cancelled.") };
                    if terminal.draw_with_panels(&frame, None, None).is_err() {
                        back.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
                // Include cancellation keys queued while the final frame was
                // being drawn before handing the terminal back to submission.
                if review.is_some() || transfer {
                    match terminal.commands() {
                        Ok(commands) if commands.iter().any(|command| command == "q") => back.store(true, Ordering::Relaxed),
                        Err(_) => back.store(true, Ordering::Relaxed),
                        _ => {}
                    }
                }
            }));
        }
        Ok(setup)
    }
    /// Call after preparation and before authorizing or submitting any transaction.
    pub async fn confirm(&self) -> Result<bool> {
        if !self.review || self.cancelled() { return Ok(false); }
        self.confirming.store(true, Ordering::Release);
        if self.worker.is_none() && !self.approved.load(Ordering::Relaxed) {
            eprintln!("Preparation complete. Enter a fresh blank line to approve; q/EOF cancels.");
            let accepted = tokio::task::spawn_blocking(|| -> std::io::Result<bool> {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                Ok(review_line(&line))
            }).await??;
            if accepted { self.approved.store(true, Ordering::Relaxed); }
            else { self.back.store(true, Ordering::Relaxed); }
        }
        loop {
            if self.cancelled() { return Ok(false); }
            if self.approved.load(Ordering::Relaxed) { return Ok(true); }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    pub fn cancelled(&self) -> bool { self.back.load(Ordering::Relaxed) }
    pub fn finish(mut self) -> bool {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        self.cancelled()
    }
    /// Only wrap cancellable reads/preparation, never required writes/submits.
    pub async fn read<T>(&self, future: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::pin!(future);
        loop {
            if self.cancelled() { return None; }
            tokio::select! {
                result = &mut future => return Some(result),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }
    pub fn set_status(&self, message: &str) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = message.to_string();
    }
}
#[cfg(test)]
mod setup_cancel_tests {
    use super::*;
    #[tokio::test]
    async fn escape_cancels_stalled_read_without_waiting_for_recovery() {
        let setup = Setup::new(false).unwrap();
        let back = setup.back.clone();
        tokio::spawn(async move { tokio::task::yield_now().await; back.store(true, Ordering::Relaxed); });
        let started = Instant::now();
        assert!(tokio::time::timeout(Duration::from_secs(1), setup.read(std::future::pending::<()>())).await.unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

/// Each event gets one wall-clock presentation window, independent of cache
/// refreshes, playback buffering, subsequent events and optional dismissal.
#[derive(Default)]
struct ReorgNotices {
    seen: std::collections::HashSet<String>,
    since: Option<Instant>,
    text: String,
    big: bool,
}
impl ReorgNotices {
    fn observe(&mut self, event: Option<&ReorgEvent>, now: Instant) {
        let Some(event) = event.filter(|event| event.affects_game()) else { return };
        if self.seen.insert(event.id.clone()) {
            self.since = Some(now);
            self.text = event.notice();
            self.big = event.is_big_game_rollback();
        }
    }
    fn visible(&self, now: Instant) -> Option<(&str, bool)> {
        self.since
            .filter(|since| now.saturating_duration_since(*since) < Duration::from_secs(2))
            .map(|_| (self.text.as_str(), self.big))
    }
    fn dismiss(&mut self) {
        self.since = None;
    }
}

/// A presentation timer, never a sleep: input, RPC and settlement keep running.
#[derive(Default)]
pub struct FinalBoard {
    court: Option<String>,
    since: Option<Instant>,
}
impl FinalBoard {
    pub fn show(&mut self, court: Option<String>, finished: bool, now: Instant) -> Option<&str> {
        if let Some(court) = court {
            self.court = Some(court);
        }
        if !finished {
            self.since = None;
            return self.court.as_deref();
        }
        let since = *self.since.get_or_insert(now);
        (now.saturating_duration_since(since) < FINAL_BOARD_HOLD).then_some(self.court.as_deref()).flatten()
    }
}

#[derive(Clone, Copy)]
pub struct ClockSample {
    pub epoch: u64,
    pub daa: u64,
    pub observed: Instant,
}

#[derive(Clone)]
pub struct Spending {
    pub paid: u128,
    pub pending: u64,
    pub complete: bool,
}

/// `local_side` identifies the player independently of the title or fee history.
/// Pass `None` for an observer.
pub fn completed_frame(result: &Finish, local_side: Option<i64>, title: &str, status: &str, spending: Option<&Spending>, unit: &str) -> String {
    let local_side = local_side.filter(|&side| matches!(side, 0 | 1));
    let fees = match (local_side, spending) {
        (Some(_), Some(s)) => format!(
            "{}: {} {unit}\nPending fees (not spent yet): {} {unit}",
            if s.complete { "Your network fees paid" } else { "Known fees paid (partial history)" },
            coins(s.paid),
            coins(s.pending as u128)
        ),
        (Some(_), None) => "Your network fee total is unavailable".to_string(),
        (None, _) => "Observer: no personal fee total".to_string(),
    };
    ascii::completed_for_side(result, local_side, title, &format!("{fees}\n{status}"), "Fees exclude stakes.\nEsc: main menu; no automatic refund.")
}

/// Visual time trails observed chain time by a small adaptive reserve. This
/// covers observation jitter, not late validation of movement/flight details;
/// those can still correct the trajectory after it has been displayed.
/// Playback never predicts chain progress or rewinds outside an explicit reset.
pub struct ObservedClock {
    base: u64,
    origin: u64,
    target: u64,
    at: Option<Instant>,
    rate: u64,
    fraction: u128,
    reserve: u64,
    observation_at: Option<Instant>,
    reserve_at: Option<Instant>,
}
impl Default for ObservedClock {
    fn default() -> Self { Self::with_base(PLAYBACK_INITIAL_BUFFER_DAA) }
}
impl ObservedClock {
    fn with_base(base: u64) -> Self {
        Self { base: base.min(PLAYBACK_MAX_BUFFER_DAA), origin: 0, target: 0, at: None, rate: 0,
            fraction: 0, reserve: 0, observation_at: None, reserve_at: None }
    }
    pub fn sample(&self, now: Instant) -> u64 {
        self.origin.saturating_add((self.progress(now) / 1000) as u64).min(self.target)
    }
    fn progress(&self, now: Instant) -> u128 {
        let elapsed = self.at.map(|at| now.saturating_duration_since(at).as_nanos()).unwrap_or(0);
        let normal = DAA_PER_SECOND as u128;
        let elapsed_progress = elapsed * normal / 1_000_000;
        let capacity = self.target.saturating_sub(self.origin) as u128 * 1000;
        if (self.rate as u128) < normal {
            return (self.fraction + elapsed * self.rate as u128 / 1_000_000).min(capacity);
        }
        let excess = self.target.saturating_sub(self.origin).saturating_sub(self.reserve) as u128;
        // Spend the speed boost only on excess delay. If updates stop, exhaust
        // that budget and continue at normal speed, capped by observed time.
        let extra_rate = (self.rate as u128).saturating_sub(normal);
        let fast_budget = (excess * 1000).saturating_sub(self.fraction);
        let boost_budget = fast_budget * extra_rate / (self.rate as u128).max(1);
        let boost = (elapsed * extra_rate / 1_000_000).min(boost_budget);
        (self.fraction + elapsed_progress + boost).min(capacity)
    }
    /// Fractional playback time is visual only; never passed to game physics.
    pub fn fractional(&self, now: Instant) -> f64 {
        if self.sample(now) >= self.target {
            return 0.0;
        }
        (self.progress(now) % 1000) as f64 / 1000.0
    }
    fn observed_lag(&self, now: Instant) -> f64 {
        self.target.saturating_sub(self.sample(now)) as f64 - self.fractional(now)
    }
    fn observe(&mut self, now: Instant) {
        let normal = DAA_PER_SECOND as u128;
        let interval = self.observation_at.map(|at| now.saturating_duration_since(at).as_millis()).unwrap_or(0);
        // Only advancing observations count. Cached copies cannot lower the
        // reserve or disguise a stall. A burst at the same instant adds no gap.
        let jitter = interval.saturating_sub(1000 / normal);
        let required = self.base
            + jitter.div_ceil(1000 / normal).min((PLAYBACK_MAX_BUFFER_DAA - self.base) as u128) as u64;
        if required >= self.reserve {
            self.reserve = required;
            self.reserve_at = Some(now);
        } else if self.reserve_at.is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(2)) {
            // Peak hold with hysteresis: reclaim only one DAA every two
            // seconds of lower gaps, rather than oscillating after each sample.
            self.reserve -= 1;
            self.reserve_at = Some(now);
        }
        self.observation_at = Some(now);
    }
    pub fn update(&mut self, target: u64, minimum: u64, reset: bool, now: Instant) {
        // Same-epoch delayed samples must not undo already observed progress.
        // Epoch/reorg owners explicitly request reset when history changes.
        let target = if reset || self.at.is_none() { target } else { target.max(self.target) };
        if self.at.is_some() && self.target == target && !reset && self.origin >= minimum {
            return;
        }
        let current = self.sample(now);
        let fraction = self.progress(now) % 1000;
        if reset || self.at.is_none() {
            self.reserve = self.base;
            self.observation_at = Some(now);
            self.reserve_at = Some(now);
        } else if target > self.target {
            self.observe(now);
        }
        self.fraction = if reset || self.at.is_none() || current >= self.target || current < minimum { 0 } else { fraction };
        self.origin = if self.at.is_none() || reset {
            target.saturating_sub(self.reserve).max(minimum).min(target)
        } else {
            current.max(minimum).min(target)
        };
        self.target = target;
        // Refill a depleted reserve at 0.8x without stepping backward. Catch up
        // at up to 2x, easing to 1x near the reserve; a one-DAA deadband avoids
        // slowing for a fraction of a tick. Retained checkpoints can impose a
        // newer minimum, but never advance beyond observed chain time.
        let normal = DAA_PER_SECOND as u64;
        let lag = (target.saturating_sub(self.origin) as u128 * 1000).saturating_sub(self.fraction);
        let reserve = self.reserve as u128 * 1000;
        self.rate = if lag + 1000 < reserve {
            normal * 4 / 5
        } else {
            normal + lag.saturating_sub(reserve).div_ceil(2000).min(normal as u128) as u64
        };
        self.at = Some(now);
    }
}

/// Retain checkpoints until playback reaches them. Installing a newly accepted
/// Court immediately would force the display forward and defeat buffering.
#[derive(Default)]
pub struct BufferedCourt {
    frames: std::collections::VecDeque<Display>,
}
impl BufferedCourt {
    pub fn push(&mut self, display: Display) {
        if !display.ready {
            return;
        }
        self.frames.push_back(display);
        // Bounded presentation history, not a recoverable match/session.
        while self.frames.len() > 256 {
            self.frames.pop_front();
        }
    }
    pub fn minimum(&self) -> u64 {
        self.frames.front().map(Self::starts_at).unwrap_or(0)
    }
    fn starts_at(display: &Display) -> u64 {
        match &display.stage {
            Stage::Live(s) if s.court.state.started => {
                s.court.state.clock.max(s.players[0].state.through).max(s.players[1].state.through).max(0) as u64
            }
            Stage::Live(s) => s.court.utxo.block_daa_score,
            Stage::Collecting(s) => s.collector.state.finish.end_daa.max(0) as u64,
            Stage::Reclaimed(f) => f.end_daa.max(0) as u64,
            _ => 0,
        }
    }
    pub fn drawing(&mut self, daa: u64) -> Option<&Display> {
        while self.frames.len() > 1 && Self::starts_at(&self.frames[1]) <= daa {
            self.frames.pop_front();
        }
        self.frames.front()
    }
}

#[cfg(test)]
mod playback_latency_tests {
    use super::*;

    fn coordinate(clock: &ObservedClock, now: Instant) -> f64 {
        clock.sample(now) as f64 + clock.fractional(now)
    }

    fn display(daa: u64, now: Instant) -> Display {
        Display {
            stage: Stage::Reclaimed(Finish {
                stake_per_player: 0, win_score: 3, left_score: 3, right_score: 0, winner: 0, end_daa: daa as i64,
            }),
            daa, ready: true, observed: now, clock_epoch: 3, pending: 1, accepted_previews: vec![], queued: 1,
            spending: None, reorgs: 0, status: String::new(), last_reorg: None,
            recent_transactions: Default::default(), lobby_hint: None,
        }
    }

    #[test]
    fn steady_observations_keep_half_second_lag_and_normal_speed() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(100, 0, false, now);
        assert_eq!(clock.sample(now), 95);
        for tick in 1..=500 {
            let at = now + Duration::from_millis(tick * 100);
            let before = coordinate(&clock, at);
            clock.update(100 + tick, 0, false, at);
            assert_eq!(coordinate(&clock, at), before);
            assert_eq!(clock.sample(at), 95 + tick);
            assert_eq!(clock.reserve, 5);
            assert_eq!(clock.rate, 10);
            assert!((coordinate(&clock, at + Duration::from_millis(50)) - before - 0.5).abs() < 0.001);
        }
    }

    #[test]
    fn configurable_base_adapts_up_to_fixed_max_and_resets() {
        let now = Instant::now();
        let mut default_clock = ObservedClock::default();
        default_clock.update(100, 0, false, now);
        assert_eq!(default_clock.reserve, 2);
        assert_eq!(default_clock.sample(now), 98);
        for base in [0, 2, 3, 15] {
            let mut clock = ObservedClock::with_base(base);
            clock.update(100, 0, false, now);
            assert_eq!(clock.sample(now), 100 - base);
            clock.update(105, 0, false, now + Duration::from_millis(500));
            assert_eq!(clock.reserve, (base + 4).min(15));
            clock.update(200, 0, false, now + Duration::from_secs(10));
            assert_eq!(clock.reserve, 15);
            clock.update(300, 0, true, now + Duration::from_secs(11));
            assert_eq!(clock.reserve, base);
        }
    }

    #[test]
    fn displayed_lag_tracks_actual_playback_not_the_reserve() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(2);
        clock.update(100, 0, false, now);
        assert_eq!(clock.observed_lag(now), 2.0);
        assert!((clock.observed_lag(now + Duration::from_millis(50)) - 1.5).abs() < 0.001);
        assert_eq!(clock.observed_lag(now + Duration::from_secs(1)), 0.0);
        assert_eq!(clock.reserve, 2);
        clock.update(110, 0, false, now + Duration::from_secs(1));
        assert_eq!(clock.observed_lag(now + Duration::from_secs(1)), 10.0);
    }

    #[test]
    fn jitter_raises_reserve_and_stable_samples_lower_it_with_hysteresis() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(100, 0, false, now);
        let gap = now + Duration::from_millis(500);
        let before = coordinate(&clock, gap);
        clock.update(105, 0, false, gap);
        assert_eq!(coordinate(&clock, gap), before);
        assert_eq!(clock.reserve, 9, "400 ms excess observation gap adds four DAA");
        assert_eq!(clock.rate, 8, "refill by slowing, never by rewinding");
        let mut previous = before;
        for tick in 1..=100 {
            let at = gap + Duration::from_millis(tick * 100);
            let current = coordinate(&clock, at);
            assert!(current >= previous && current - previous <= 2.001);
            clock.update(105 + tick, 0, false, at);
            assert_eq!(coordinate(&clock, at), current);
            assert!(current <= (105 + tick) as f64);
            assert_eq!(clock.reserve, 9_u64.saturating_sub(tick / 20).max(5));
            previous = current;
        }
        assert!((5.0..=6.0).contains(&(205.0 - previous)));
    }

    #[test]
    fn cached_samples_do_not_hide_stalls_or_drain_the_adaptive_reserve() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(100, 0, false, now);
        clock.update(105, 0, false, now + Duration::from_millis(500));
        for tick in 6..=100 {
            clock.update(105, 0, false, now + Duration::from_millis(tick * 100));
        }
        assert_eq!(clock.reserve, 9);
        assert_eq!(clock.sample(now + Duration::from_secs(10)), 105);
        clock.update(200, 0, false, now + Duration::from_secs(10));
        assert_eq!(clock.reserve, PLAYBACK_MAX_BUFFER_DAA);
        assert_eq!(clock.sample(now + Duration::from_secs(10)), 105);
    }

    #[test]
    fn ten_second_stall_recovers_continuously_and_returns_to_small_reserve() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(200, 0, false, now);
        let recovered = now + Duration::from_secs(10);
        assert_eq!(coordinate(&clock, recovered), 200.0);
        clock.update(300, 0, false, recovered);
        assert_eq!(coordinate(&clock, recovered), 200.0);
        assert_eq!(clock.reserve, 15);
        let mut previous = 200.0;
        for tick in 1..=300 {
            let at = recovered + Duration::from_millis(tick * 100);
            let current = coordinate(&clock, at);
            assert!(current >= previous && current - previous <= 2.001);
            clock.update(300 + tick, 0, false, at);
            assert_eq!(coordinate(&clock, at), current);
            assert!(current <= (300 + tick) as f64);
            previous = current;
        }
        assert_eq!(clock.reserve, 5);
        assert!((5.0..=6.0).contains(&(600.0 - previous)));
        assert!((coordinate(&clock, recovered + Duration::from_millis(30_100)) - previous - 1.0).abs() < 0.001);
    }

    #[test]
    fn bursts_catch_up_without_jumps_and_exhaust_their_speed_boost() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(120, 0, false, now);
        for target in [125, 130, 140] {
            clock.update(target, 0, false, now);
            assert_eq!(coordinate(&clock, now), 115.0);
            assert_eq!(clock.reserve, 5, "same-instant arrivals add no observation gap");
        }
        assert_eq!(coordinate(&clock, now + Duration::from_secs(1)), 135.0);
        assert_eq!(coordinate(&clock, now + Duration::from_millis(1250)), 137.5);
        assert_eq!(coordinate(&clock, now + Duration::from_millis(1500)), 140.0);
        assert_eq!(coordinate(&clock, now + Duration::from_secs(20)), 140.0);
    }

    #[test]
    fn fraction_survives_updates_and_render_polls_without_creating_tip_progress() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(100, 0, false, now);
        let at = now + Duration::from_micros(37_500);
        assert!((coordinate(&clock, at) - 95.375).abs() < 0.001);
        let before = coordinate(&clock, at);
        clock.update(101, 0, false, at);
        assert_eq!(coordinate(&clock, at), before);
        for micros in [38_000, 54_667, 71_334, 88_001] {
            let poll = now + Duration::from_micros(micros);
            let before = coordinate(&clock, poll);
            clock.update(101, 0, false, poll);
            assert_eq!(coordinate(&clock, poll), before);
        }
        let stopped = now + Duration::from_secs(20);
        assert_eq!(clock.sample(stopped), 101);
        assert_eq!(clock.fractional(stopped), 0.0);
        clock.update(102, 0, false, stopped);
        assert_eq!(coordinate(&clock, stopped), 101.0, "an exhausted fraction cannot carry into a fresh tip");
    }

    #[test]
    fn delayed_same_epoch_targets_do_not_rewind_but_explicit_resets_do() {
        let now = Instant::now();
        let mut clock = ObservedClock::with_base(5);
        clock.update(100, 0, false, now);
        let later = now + Duration::from_secs(10);
        clock.update(200, 0, false, later);
        assert_eq!(clock.reserve, 15);
        let at = later + Duration::from_millis(25);
        let before = coordinate(&clock, at);
        clock.update(90, 0, false, at);
        assert_eq!(coordinate(&clock, at), before);
        assert_eq!(clock.target, 200);
        clock.update(80, 0, true, at);
        assert_eq!(coordinate(&clock, at), 75.0);
        assert_eq!(clock.reserve, 5);
        clock.update(80, 79, true, at);
        assert_eq!(coordinate(&clock, at), 79.0);
        clock.update(80, 100, false, at);
        assert_eq!(coordinate(&clock, at), 80.0, "a checkpoint minimum cannot invent observed time");
        clock.update(0, 0, true, at);
        assert_eq!(coordinate(&clock, at), 0.0);
        assert_eq!(clock.reserve, 5);
        let mut zero = ObservedClock::with_base(5);
        zero.update(0, 0, false, now);
        assert_eq!(zero.reserve, 5, "zero-valued initial tips still initialize the policy");
    }

    #[test]
    fn clock_following_requires_fresh_validated_snapshot_and_matching_epoch() {
        let now = Instant::now();
        let mut display = display(100, now);
        let mut sample = ClockSample { epoch: 3, daa: 101, observed: now };
        assert!(clock_matches(&display, &sample, now));
        assert!(clock_matches(&display, &sample, now + Duration::from_secs(2)));
        assert!(!clock_matches(&display, &sample, now + Duration::from_millis(2001)));
        sample.observed = now + SNAPSHOT_LIFETIME;
        assert!(clock_matches(&display, &sample, now + SNAPSHOT_LIFETIME));
        assert!(!clock_matches(&display, &sample, now + SNAPSHOT_LIFETIME + Duration::from_millis(1)));
        sample.observed = now;
        sample.epoch += 1;
        assert!(!clock_matches(&display, &sample, now));
        sample.epoch = 3;
        sample.daa = 99;
        assert!(!clock_matches(&display, &sample, now));
        sample.daa = 101;
        display.ready = false;
        assert!(!clock_matches(&display, &sample, now));
    }

    #[test]
    fn buffered_checkpoints_wait_for_their_time_and_unvalidated_frames_are_ignored() {
        let now = Instant::now();
        let mut playback = BufferedCourt::default();
        playback.push(display(100, now));
        let mut incomplete = display(105, now);
        incomplete.ready = false;
        playback.push(incomplete);
        playback.push(display(110, now));
        assert_eq!(playback.frames.len(), 2);
        assert_eq!(playback.minimum(), 100);
        assert_eq!(playback.drawing(109).unwrap().daa, 100);
        assert_eq!(playback.drawing(110).unwrap().daa, 110);
        assert_eq!(playback.minimum(), 110);
        assert_eq!(playback.drawing(150).unwrap().daa, 110);
    }
}
#[derive(Clone)]
pub struct Movement {
    pub direction: i64,
    pub pressed: Instant,
}

#[derive(Clone)]
struct PreviewInput {
    movement: Movement,
    transaction: Option<kaspa_consensus_core::Hash>,
    issued: bool,
    issued_batch: Option<kaspa_consensus_core::Hash>,
}

#[derive(Clone, Default)]
pub struct InputPreview {
    inputs: std::collections::VecDeque<PreviewInput>,
}
impl InputPreview {
    pub fn push(&mut self, movement: Movement) -> bool {
        self.inputs.push_back(PreviewInput { movement, transaction: None, issued: false, issued_batch: None });
        true
    }
    pub fn submitted(&mut self, pressed: Instant, transaction: kaspa_consensus_core::Hash) {
        if let Some(input) = self.inputs.iter_mut().find(|input| input.movement.pressed == pressed) {
            input.transaction = Some(transaction);
            input.issued = true;
            input.issued_batch = Some(transaction);
        }
    }
    pub fn discard(&mut self, pressed: Instant) {
        self.inputs.retain(|input| input.movement.pressed != pressed);
    }
    pub fn requeue(&mut self, movement: &Movement) {
        if let Some(input) = self.inputs.iter_mut().find(|input| input.movement.pressed == movement.pressed) {
            input.transaction = None;
            input.issued = true;
        } else {
            // An earlier coherent frame may have acknowledged this move before
            // a reorg removed it. Restore its target together with the retry.
            self.push(movement.clone());
            self.inputs.back_mut().unwrap().issued = true;
            self.inputs.make_contiguous().sort_by_key(|input| input.movement.pressed);
        }
    }
    /// Select a queued same-direction burst. Readiness is checked separately.
    /// Previously issued moves can only regroup with other
    /// members of THEIR original transaction when it needs rebuilding.
    pub fn movement_batch(&self, queue: &std::collections::VecDeque<Movement>) -> Vec<Movement> {
        let Some(first) = queue.front() else { return Vec::new() };
        let Some(first_index) = self.inputs.iter().position(|input| input.movement.pressed == first.pressed) else {
            return vec![first.clone()];
        };
        let origin = &self.inputs[first_index];
        queue
            .iter()
            .zip(self.inputs.iter().skip(first_index))
            .take(MAX_MOVE_STEPS as usize)
            .take_while(|(movement, input)| {
                movement.pressed == input.movement.pressed
                    && movement.direction == first.direction
                    && if origin.issued {
                        origin.issued_batch.is_some() && input.issued_batch == origin.issued_batch
                    } else {
                        !input.issued
                    }
            })
            .map(|(movement, _)| movement.clone())
            .collect()
    }
    /// Wait once from the first unissued press, without blocking other work.
    /// Full batches and previously issued retries need no collection delay.
    pub fn ready_movement_batch(&self, queue: &std::collections::VecDeque<Movement>, now: Instant) -> Vec<Movement> {
        let batch = self.movement_batch(queue);
        let Some(first) = batch.first() else { return batch };
        let issued = self.inputs.iter().any(|input| input.movement.pressed == first.pressed && input.issued);
        if issued || batch.len() == MAX_MOVE_STEPS as usize || now.saturating_duration_since(first.pressed) >= MOVEMENT_BATCH_WINDOW {
            batch
        } else {
            Vec::new()
        }
    }
    pub fn unissued_batch(&self, batch: &[Movement]) -> bool {
        !batch.is_empty() && batch.iter().all(|m| self.inputs.iter().any(|i| i.movement.pressed == m.pressed && !i.issued))
    }
    pub fn same_batch(left: &[Movement], right: &[Movement]) -> bool {
        left.len() == right.len() && left.iter().zip(right).all(|(a, b)| a.pressed == b.pressed && a.direction == b.direction)
    }
    /// Inspect a possible replacement without releasing intent while its old
    /// authorization still has a durable reservation.
    pub fn batch_after_cancellation(
        &self, queue: &std::collections::VecDeque<Movement>, world: &World, side: i64,
        accepted: &[kaspa_consensus_core::Hash],
    ) -> Vec<Movement> {
        let mut preview = self.clone();
        let mut waiting = queue.clone();
        preview.cancel_unsent(&mut waiting, world, side, accepted);
        preview.ready_movement_batch(&waiting, Instant::now())
    }
    /// Cancel any neutral opposite pair from the waiting queue, even across
    /// issued moves. Issued/retried moves stay in place. Check the complete
    /// clipped paddle path so cancellation cannot change the displayed target.
    pub fn cancel_unsent(
        &mut self,
        queue: &mut std::collections::VecDeque<Movement>,
        world: &World,
        side: i64,
        accepted: &[kaspa_consensus_core::Hash],
    ) -> usize {
        let (target, _) = self.paddle(world, side, accepted, Instant::now());
        let start = if side == 0 { world.left_y } else { world.right_y };
        let waiting = queue.iter().map(|movement| movement.pressed).collect::<std::collections::BTreeSet<_>>();
        let mut inputs = self.inputs.drain(..).collect::<Vec<_>>();
        let mut removed = std::collections::BTreeSet::new();
        loop {
            // A linear pass also bounds ordinary held-key backlogs at a wall.
            // Removing a clipped step leaves its entire suffix unchanged.
            let mut y = start;
            inputs.retain(|input| {
                let next = (y + input.movement.direction).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
                let noop = !input.issued && waiting.contains(&input.movement.pressed) && next == y;
                y = next;
                if noop { removed.insert(input.movement.pressed); }
                !noop
            });
            let eligible = inputs.iter().map(|input| !input.issued && waiting.contains(&input.movement.pressed)).collect::<Vec<_>>();
            if !inputs.iter().zip(&eligible).any(|(input, eligible)| *eligible && input.movement.direction == -1)
                || !inputs.iter().zip(&eligible).any(|(input, eligible)| *eligible && input.movement.direction == 1)
            {
                break;
            }
            // Every suffix is clamp(y + shift, low, high). Precompute these
            // maps to test a nonadjacent pair without replaying the whole tail.
            let mut suffix = vec![(0_i64, PADDLE_MIN_Y, PADDLE_MAX_Y); inputs.len() + 1];
            for i in (0..inputs.len()).rev() {
                let (shift, low, high) = suffix[i + 1];
                suffix[i] = (
                    inputs[i].movement.direction + shift,
                    (PADDLE_MIN_Y + shift).clamp(low, high),
                    (PADDLE_MAX_Y + shift).clamp(low, high),
                );
            }
            let mut pair = None;
            let mut before = start;
            'search: for i in 0..inputs.len() {
                if eligible[i] {
                    let mut without_first = before;
                    for j in i + 1..inputs.len() {
                        if eligible[j] && inputs[i].movement.direction == -inputs[j].movement.direction {
                            let (shift, low, high) = suffix[j + 1];
                            if (without_first + shift).clamp(low, high) == target {
                                pair = Some((i, j));
                                break 'search;
                            }
                        }
                        without_first = (without_first + inputs[j].movement.direction).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
                    }
                }
                before = (before + inputs[i].movement.direction).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y);
            }
            let Some((first, second)) = pair else { break };
            removed.insert(inputs.remove(second).movement.pressed);
            removed.insert(inputs.remove(first).movement.pressed);
        }
        self.inputs = inputs.into();
        queue.retain(|movement| !removed.contains(&movement.pressed));
        removed.len()
    }
    pub fn clear(&mut self) {
        self.inputs.clear();
    }
    pub fn accepted(&self, tracker: &super::node::tracker::Tracker) -> Vec<kaspa_consensus_core::Hash> {
        self.inputs.iter().filter_map(|input| input.transaction.filter(|txid| tracker.accepted(*txid))).collect()
    }
    pub fn paddle(&mut self, world: &World, side: i64, accepted: &[kaspa_consensus_core::Hash], _now: Instant) -> (i64, usize) {
        self.inputs.retain(|input| match input.transaction {
            Some(txid) => !accepted.contains(&txid),
            None => true,
        });
        let accepted = if side == 0 { world.left_y } else { world.right_y };
        let predicted = self.inputs.iter().fold(accepted, |y, input| (y + input.movement.direction).clamp(PADDLE_MIN_Y, PADDLE_MAX_Y));
        (predicted, self.inputs.len())
    }
}
/// Retain authorized movements separately from their replaceable transactions.
/// Rebuild only after coherent replay proves the old transaction is neither
/// accepted nor retained as pending (including its fee descendants).
#[derive(Default)]
pub struct MovementDelivery {
    submitted: Vec<(Movement, kaspa_consensus_core::Hash)>,
    accepted: Vec<(Movement, kaspa_consensus_core::Hash)>,
}
impl MovementDelivery {
    pub fn submitted(&mut self, movement: Movement, txid: kaspa_consensus_core::Hash) {
        self.submitted.push((movement, txid));
    }
    pub fn reconcile(&mut self, accepted: &[kaspa_consensus_core::Hash], pending: &[kaspa_consensus_core::Hash]) -> Vec<Movement> {
        let mut retry = Vec::new();
        self.submitted.retain(|(movement, txid)| {
            if accepted.contains(txid) {
                self.accepted.push((movement.clone(), *txid));
                false
            } else if pending.contains(txid) {
                true
            } else {
                retry.push(movement.clone());
                false
            }
        });
        retry
    }
    pub fn reorged(&mut self, removed: &[kaspa_consensus_core::Hash]) -> Vec<(Movement, kaspa_consensus_core::Hash)> {
        let mut restored = Vec::new();
        self.accepted.retain(|(movement, txid)| {
            if removed.contains(txid) {
                restored.push((movement.clone(), *txid));
                false
            } else {
                true
            }
        });
        self.submitted.extend(restored.iter().cloned());
        restored
    }
    pub fn accepted(&self, tracker: &super::node::tracker::Tracker) -> Vec<kaspa_consensus_core::Hash> {
        self.submitted.iter().filter_map(|(_, txid)| tracker.accepted(*txid).then_some(*txid)).collect()
    }
}
#[derive(Clone)]
pub struct Display {
    pub stage: Stage,
    pub daa: u64,
    pub ready: bool,
    pub observed: Instant,
    pub clock_epoch: u64,
    pub pending: usize,
    pub accepted_previews: Vec<kaspa_consensus_core::Hash>,
    pub queued: usize,
    pub spending: Option<Spending>,
    pub reorgs: u64,
    pub status: String,
    pub last_reorg: Option<ReorgEvent>,
    pub recent_transactions: std::collections::VecDeque<TransactionRecord>,
    pub lobby_hint: Option<String>,
}
/// Incomplete replay may temporarily remove actors that a later batch restores.
/// Keep drawing the last coherent state, labelled as synchronizing; spending
/// still uses the current display's readiness. Epoch changes forbid reuse.
pub fn court_display<'a>(current: &'a Display, coherent: Option<&'a Display>) -> &'a Display {
    if !current.ready {
        if let Some(previous) = coherent.filter(|previous| previous.clock_epoch == current.clock_epoch) {
            return previous;
        }
    }
    current
}
static DEBUG_PADDLES: AtomicBool = AtomicBool::new(false);
pub fn set_debug_paddles(enabled: bool) { DEBUG_PADDLES.store(enabled, Ordering::Relaxed); }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RematchOffer { Host { code: String, summary: String }, Guest }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RematchRequest { Host, Join(String) }

#[cfg(test)]
mod rematch_request_tests {
    use super::*;
    #[test]
    fn invalidating_offer_clears_an_already_queued_request() {
        let ui = Frontend::new(false, false, 0, false, "devnet").unwrap();
        for offer in [None, Some(RematchOffer::Guest)] {
            let mut shared = ui.rematch.lock().unwrap();
            shared.offer = offer;
            shared.request = Some(RematchRequest::Join("queued before rollback".into()));
            drop(shared);
            ui.offer_rematch(None);
            assert_eq!(ui.take_rematch_request(), None);
        }
    }
}

pub fn rematch_usable(d: &Display, watch: bool) -> bool {
    !watch && rematch_state_ready(&d.stage, d.pending, d.ready, d.observed)
}
pub fn rematch_state_ready(stage: &Stage, pending: usize, ready: bool, observed: Instant) -> bool {
    ready && observed.elapsed() <= SNAPSHOT_LIFETIME && pending == 0 && matches!(stage, Stage::Reclaimed(_))
}
/// Compact the receipt and wrap codes instead of truncating them at the edge.
pub fn wrap_rematch_frame(frame: &str, width: usize) -> String {
    frame.lines().filter(|line| !line.is_empty() && !line.starts_with("+---")).flat_map(|line| {
        line.chars().collect::<Vec<_>>().chunks(width.max(1))
            .map(|chunk| chunk.iter().collect::<String>()).collect::<Vec<_>>()
    }).collect::<Vec<_>>().join("\n")
}
#[derive(Default)]
struct RematchShared {
    offer: Option<RematchOffer>,
    revision: u64,
    request: Option<RematchRequest>,
}
/// Local draft only; never authorizes funding or submits an RPC.
#[derive(Default)]
pub struct RematchControls {
    revision: Option<u64>,
    offer: Option<RematchOffer>,
    draft: String,
    message: String,
}
impl RematchControls {
    /// Returns true once on entry so the terminal owner can call begin_prompt.
    pub fn sync(&mut self, offer: Option<&RematchOffer>, usable: bool, revision: u64) -> bool {
        let offer = offer.filter(|_| usable);
        if self.revision == offer.map(|_| revision) && self.offer.as_ref() == offer { return false; }
        self.draft.clear();
        self.message.clear();
        self.offer = offer.cloned();
        self.revision = offer.map(|_| revision);
        offer.is_some()
    }
    pub fn key(&mut self, key: PromptKey) -> Option<RematchRequest> {
        match self.offer.as_ref()? {
            RematchOffer::Host { .. } => matches!(key, PromptKey::Enter).then_some(RematchRequest::Host),
            RematchOffer::Guest => {
                match key {
                    PromptKey::Character(ch) if !ch.is_control() => { self.draft.push(ch); self.message.clear(); }
                    PromptKey::Backspace => { self.draft.pop(); self.message.clear(); }
                    PromptKey::Enter => {
                        let code = self.draft.trim();
                        if super::node::invitations::parse_rendezvous_code(code).is_ok() {
                            return Some(RematchRequest::Join(code.to_string()));
                        }
                        self.message = "Invalid game code; paste a complete rematch code.".into();
                    }
                    _ => {}
                }
                None
            }
        }
    }
    pub fn text(&self) -> String {
        match &self.offer {
            Some(RematchOffer::Host { code, summary }) => format!("Rematch: {summary}\nShare rematch code:\n{code}\nEnter: host rematch; Esc: main menu"),
            Some(RematchOffer::Guest) => format!("Paste rematch code, Enter to review; Esc menu\n{}\n{}", self.draft, self.message),
            None => String::new(),
        }
    }
}

pub struct Frontend {
    pub input: Receiver<Movement>,
    pub quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    display: Option<Arc<Mutex<Option<Display>>>>,
    worker: Option<JoinHandle<()>>,
    clock: Arc<Mutex<Option<ClockSample>>>,
    preview: Arc<Mutex<InputPreview>>,
    rematch: Arc<Mutex<RematchShared>>,
}
impl Frontend {
    pub fn new(play: bool, interactive: bool, side: i64, watch: bool, network: &str) -> Result<Self> {
        let debug_paddles = DEBUG_PADDLES.load(Ordering::Relaxed);
        use super::terminal::{GameplayCommand, gameplay_line};
        let (send, input) = mpsc::channel();
        let quit = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let clock_feed = Arc::new(Mutex::new(None::<ClockSample>));
        let preview = Arc::new(Mutex::new(InputPreview::default()));
        let rematch = Arc::new(Mutex::new(RematchShared::default()));
        let mut frontend =
            Self { input, quit: quit.clone(), stop: stop.clone(), display: None, worker: None, clock: clock_feed.clone(), preview: preview.clone(), rematch: rematch.clone() };
        if play {
            let terminal = Terminal::new()?;
            if !terminal.is_screen() {
                return Err("ASCII play requires a terminal; use --json for pipes".into());
            }
            terminal.begin_prompt()?;
            let display = Arc::new(Mutex::new(None));
            let receive = display.clone();
            frontend.display = Some(display);
            let title = format!(
                "PONG | KASPA {} | {}",
                network.to_uppercase(),
                if watch {
                    "OBSERVER"
                } else if side == 0 {
                    "YOU: LEFT"
                } else {
                    "YOU: RIGHT"
                }
            );
            let unit = if network.starts_with("testnet") { "TKAS" } else { "KAS" };
            frontend.worker = Some(std::thread::spawn(move || {
                let mut rematch_controls = RematchControls::default();
                let mut current: Option<Display> = None;
                let mut coherent: Option<Display> = None;
                let mut playback = BufferedCourt::default();
                let mut final_board = FinalBoard::default();
                let mut effects = ascii::CourtEffects::default();
                let mut audio = crate::sound::GameAudio::new();
                let mut clock = ObservedClock::with_base(playback_base());
                let mut notice = String::new();
                let mut notice_at = Instant::now();
                let mut reorg_notices = ReorgNotices::default();
                let mut first_live = None;
                let mut rendered_start = false;
                while !stop.load(Ordering::Relaxed) {
                    if quit.load(Ordering::Relaxed) {
                        let _ = terminal.draw_with_panels("Returning to main menu...\n\nFinishing game cleanup.", None, None);
                        while !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(20)); }
                        break;
                    }
                    let latest = receive.lock().expect("display mutex").take();
                    if let Some(display) = latest {
                        reorg_notices.observe(display.last_reorg.as_ref(), Instant::now());
                        let game_reorg = display.last_reorg.as_ref().is_some_and(|event| {
                            event.affects_game()
                                && current.as_ref().and_then(|d| d.last_reorg.as_ref()).map(|r| &r.id) != Some(&event.id)
                        });
                        let reset = game_reorg || current.as_ref().is_some_and(|d| d.clock_epoch != display.clock_epoch);
                        if reset {
                            let mut shared = rematch.lock().expect("rematch mutex");
                            shared.offer = None;
                            shared.revision = shared.revision.wrapping_add(1);
                            rematch_controls.sync(None, false, shared.revision);
                            coherent = None;
                            playback = BufferedCourt::default();
                            final_board = FinalBoard::default();
                            effects.reset_timeline();
                            audio.reset_timeline();
                        }
                        if display.ready {
                            if first_live.is_none() && matches!(display.stage, Stage::Live(_)) { first_live = Some(Instant::now()); }
                            coherent = Some(display.clone());
                            playback.push(display.clone());
                        }
                        let drawing = court_display(&display, coherent.as_ref());
                        let minimum = playback.minimum();
                        clock.update(if reset { drawing.daa } else { drawing.daa.max(clock.target) }, minimum, reset, Instant::now());
                        current = Some(display);
                    }
                    if let Some(d) = &current {
                        let d = court_display(d, coherent.as_ref());
                        let sample = *clock_feed.lock().expect("clock mutex");
                        if let Some(sample) = sample.filter(|sample| clock_matches(d, sample, Instant::now())) {
                            let minimum = playback.minimum();
                            clock.update(sample.daa.max(d.daa), minimum, false, Instant::now());
                        }
                    }
                    // Use only the current accepted state, never coherent/playback fallback.
                    let mut shared = rematch.lock().expect("rematch mutex");
                    let usable = current.as_ref().is_some_and(|d| rematch_usable(d, watch));
                    if rematch_controls.sync(shared.offer.as_ref(), usable, shared.revision) {
                        if terminal.begin_prompt().is_err() { quit.store(true, Ordering::Relaxed); break; }
                    }
                    let rematch_active = usable && shared.offer.is_some();
                    if rematch_active {
                        let key = if matches!(shared.offer, Some(RematchOffer::Guest)) {
                            terminal.prompt_text_key(Duration::ZERO)
                        } else { terminal.prompt_key(Duration::ZERO) };
                        match key {
                            Ok(Some(PromptKey::Back | PromptKey::Exit)) => quit.store(true, Ordering::Relaxed),
                            Ok(Some(key)) => {
                                if let Some(request) = rematch_controls.key(key) {
                                    shared.request = Some(request);
                                    quit.store(true, Ordering::Relaxed);
                                }
                            }
                            Err(_) => quit.store(true, Ordering::Relaxed),
                            _ => {}
                        }
                    }
                    drop(shared);
                    let commands = if rematch_active { Vec::new() } else { match terminal.gameplay_commands() {
                        Ok(commands) => commands,
                        Err(_) => {
                            quit.store(true, Ordering::Relaxed);
                            break;
                        }
                    } };
                    for command in commands {
                        if command == GameplayCommand::Exit {
                            quit.store(true, Ordering::Relaxed);
                            break;
                        }
                        if command == GameplayCommand::DismissNotice {
                            reorg_notices.dismiss();
                            continue;
                        }
                        if command == GameplayCommand::ToggleSound {
                            notice = if audio.toggle() { "Sound muted" } else { "Sound on" }.into();
                            notice_at = Instant::now();
                            continue;
                        }
                        let direction = match command {
                            GameplayCommand::Up => -1,
                            GameplayCommand::Down => 1,
                            _ => continue,
                        };
                        let can_move = !watch
                            && current.as_ref().is_some_and(|d| {
                                let accepted = court_display(d, coherent.as_ref());
                                matches!(&accepted.stage, Stage::Live(s) if project(s,accepted.daa).is_ok_and(|v| !v.world.ended()))
                            });
                        if !can_move {
                            notice = "Input not sent: waiting for a synchronized live game.".into();
                        } else {
                            let movement = Movement { direction, pressed: Instant::now() };
                            let mut preview = preview.lock().expect("preview mutex");
                            preview.push(movement.clone());
                            if send.send(movement.clone()).is_err() {
                                preview.discard(movement.pressed);
                                notice = "Input not sent: game worker has stopped.".into();
                            } else {
                                notice.clear();
                            }
                        }
                        notice_at = Instant::now();
                    }
                    let columns = crossterm::terminal::size().map(|(w, _)| w.saturating_sub(3) as usize).unwrap_or(128);
                    let mut start_frame = None;
                    let frame = if let Some(d) = &current {
                        let frame_time = Instant::now();
                        let live = d.ready && d.observed.elapsed() <= SNAPSHOT_LIFETIME;
                        let responsive = court_display(d, coherent.as_ref()).observed.elapsed() <= SNAPSHOT_LIFETIME;
                        let footer = format!(
                            "{} | render lag: {:.0} DAA | pending tx: {}/3 | keys: {} | paid: {} {unit} | reorgs: {}",
                            if live {
                                "SYNCED"
                            } else if responsive {
                                "CATCHING UP"
                            } else {
                                "PAUSED"
                            },
                            clock.observed_lag(frame_time),
                            d.pending,
                            d.queued,
                            d.spending.as_ref().map(|s| coins(s.paid)).unwrap_or_else(|| "observer".into()),
                            d.reorgs
                        );
                        let ended = matches!(&d.stage, Stage::Live(s) if d.ready && project(s,d.daa).is_ok_and(|v| v.world.ended()));
                        let status = if ended {
                            "Projected result; on-chain finalization is still pending"
                        } else if !live && d.observed.elapsed() > SNAPSHOT_LIFETIME {
                            "RPC delayed; inputs paused"
                        } else if !d.ready {
                            "Catching up; controls retained locally"
                        } else if notice_at.elapsed() < Duration::from_secs(3) {
                            &notice
                        } else if !debug_paddles && d.status == "Following accepted chain" {
                            ""
                        } else {
                            &d.status
                        };
                        let daa = clock.sample(frame_time);
                        let fractional_daa = clock.fractional(frame_time);
                        let drawing = playback.drawing(daa).unwrap_or_else(|| court_display(d, coherent.as_ref()));
                        if let Stage::Live(s) = &drawing.stage {
                            // Clock samples are bounded by snapshot freshness and
                            // epoch. A brief replay gap must not rewind the court.
                            if let Ok(view) = project(s, daa) {
                                if !rendered_start {
                                    start_frame = Some((s.court.outpoint.transaction_id, view.world.serve_until.saturating_sub(daa as i64).max(0)));
                                }
                                effects.observe(s, &view.world, daa, frame_time);
                                audio.world(s, &view.world, daa);
                                let goal_notice = effects.goal_notice(frame_time);
                                let extra_goal_notices = effects.extra_goal_notices(frame_time);
                                let has_goal_notice = goal_notice.is_some() || extra_goal_notices.iter().any(Option::is_some);
                                if view.world.ended() {
                                    let board = ascii::court_frame_with_presentation(
                                        s,
                                        daa,
                                        status,
                                        &title,
                                        "Game ended; Esc: main menu; no automatic refund",
                                        &footer,
                                        ascii::CourtPresentation {
                                            columns,
                                            fractional_daa,
                                            impact: effects.sample(daa, fractional_daa, frame_time),
                                            goal_notice,
                                            extra_impacts: effects.extra_impacts(daa, fractional_daa, frame_time),
                                            extra_goal_notices,
                                            ..Default::default()
                                        },
                                    )
                                    .ok();
                                    let held = final_board.show(board.clone(), true, frame_time).map(str::to_owned);
                                    has_goal_notice.then_some(board).flatten().or(held).unwrap_or_else(|| {
                                        completed_frame(
                                            &view.world.result(),
                                            (!watch).then_some(side),
                                            &title,
                                            "Projected result; on-chain finalization is still pending",
                                            d.spending.as_ref(),
                                            unit,
                                        )
                                    })
                                } else {
                                    // Own-paddle feedback follows current accepted
                                    // state plus local intent, not the delayed ball.
                                    // Otherwise an accepted key would snap back
                                    // until playback reached its timestamp.
                                    let accepted = court_display(d, coherent.as_ref());
                                    let own_world = match &accepted.stage {
                                        Stage::Live(current) => project(current, accepted.daa).ok().map(|p| p.world),
                                        _ => None,
                                    };
                                    let (y, count) = if !watch {
                                        preview.lock().expect("preview mutex").paddle(
                                            own_world.as_ref().unwrap_or(&view.world),
                                            side,
                                            &accepted.accepted_previews,
                                            Instant::now(),
                                        )
                                    } else {
                                        (if side == 0 { view.world.left_y } else { view.world.right_y }, 0)
                                    };
                                    let gray = if debug_paddles { " | Gray: accepted" } else { "" };
                                    let shown_y = if side == 0 { view.world.left_y } else { view.world.right_y };
                                    let show_target = !watch && y != shown_y;
                                    let legend = if count > 0 && debug_paddles {
                                        format!("White: play{gray} | Red: target ({count})")
                                    } else {
                                        format!("White: play{gray} | Red: target")
                                    };
                                    let status = if status.is_empty() { legend } else { format!("{legend} | {status}") };
                                    let frame = ascii::court_frame_with_presentation(
                                        s,
                                        daa,
                                        &status,
                                        &title,
                                        "Up/Down or W/S: move; M: sound; Esc: menu (no refund)",
                                        &footer,
                                        ascii::CourtPresentation {
                                            columns,
                                            fractional_daa,
                                            accepted_paddles: own_world.as_ref().map(|w| (w.left_y, w.right_y)),
                                            debug_paddles,
                                            sparkles: true,
                                            countdown_pulse: true,
                                            preview: show_target.then_some((side, y)),
                                            impact: effects.sample(daa, fractional_daa, frame_time),
                                            goal_notice,
                                            extra_impacts: effects.extra_impacts(daa, fractional_daa, frame_time),
                                            extra_goal_notices,
                                            ..Default::default()
                                        },
                                    )
                                    .unwrap_or_else(|e| format!("{title}\n{e}"));
                                    final_board.show(Some(frame.clone()), false, Instant::now());
                                    frame
                                }
                            } else {
                                format!("{title}\nWaiting for a coherent game clock")
                            }
                        } else if let Stage::Collecting(s) = &drawing.stage {
                            let finished = &s.collector.state.finish;
                            audio.finished_goal(finished);
                            let board = effects.finished_frame(finished, &title, status, &footer, columns, frame_time);
                            let held = final_board.show(board.clone(), true, frame_time).map(str::to_owned);
                            let has_notice = effects.goal_notice(frame_time).is_some()
                                || effects.extra_goal_notices(frame_time).iter().any(Option::is_some);
                            has_notice.then_some(board).flatten().or(held).unwrap_or_else(|| {
                                audio.outcome(finished, (!watch).then_some(side));
                                completed_frame(
                                    finished,
                                    (!watch).then_some(side),
                                    &title,
                                    "Outcome accepted on chain; pot payout pending.",
                                    d.spending.as_ref(),
                                    unit,
                                )
                            })
                        } else if let Stage::Reclaimed(result) = &drawing.stage {
                            audio.finished_goal(result);
                            let board = effects.finished_frame(result, &title, status, &footer, columns, frame_time);
                            let held = final_board.show(board.clone(), true, frame_time).map(str::to_owned);
                            let has_notice = effects.goal_notice(frame_time).is_some()
                                || effects.extra_goal_notices(frame_time).iter().any(Option::is_some);
                            has_notice.then_some(board).flatten().or(held).unwrap_or_else(|| {
                                audio.outcome(result, (!watch).then_some(side));
                                completed_frame(
                                    result,
                                    (!watch).then_some(side),
                                    &title,
                                    "Payouts accepted on chain.",
                                    d.spending.as_ref(),
                                    unit,
                                )
                            })
                        } else {
                            let phase = match &drawing.stage {
                                Stage::AwaitingLobby => "Waiting for lobby acceptance",
                                Stage::Lobby(_) if matches!(d.stage, Stage::Live(_)) => "Start accepted; buffering the court",
                                Stage::Lobby(_) if side == 1 => "Starting game",
                                Stage::Lobby(_) => "Waiting for the host (RIGHT) to fund and start",
                                Stage::Cancelled => "Lobby cancelled",
                                _ => unreachable!(),
                            };
                            format!(
                                "{title}\n\n{phase}\n\n{}\n\n{footer}\n\n{status}\nEsc: main menu (match stays active; no automatic refund).\n",
                                d.lobby_hint.as_deref().unwrap_or("")
                            )
                        }
                    } else {
                        format!("{title}\n\nConnecting and synchronizing...\nEsc: main menu (match stays active; no automatic refund).\n")
                    };
                    let frame = if rematch_active {
                        if let Some(d) = &current {
                            if let Stage::Reclaimed(result) = &d.stage {
                                format!("{}\n{}", completed_frame(result, Some(side), &title,
                                    "Payouts accepted on chain.", d.spending.as_ref(), unit), rematch_controls.text())
                            } else { frame }
                        } else { frame }
                    } else { frame };
                    let width = crossterm::terminal::size().map(|(w, _)| w.saturating_sub(1) as usize).unwrap_or(79);
                    let frame = if rematch_active { wrap_rematch_frame(&frame, width) } else {
                        frame.lines().map(|line| line.chars().take(width).collect::<String>()).collect::<Vec<_>>().join("\n")
                    };
                    let reorg_notice = reorg_notices.visible(Instant::now());
                    let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(25);
                    let count = if height >= 32 {
                        3
                    } else if height >= 28 {
                        2
                    } else {
                        1
                    };
                    let activity = current
                        .as_ref()
                        .map(|d| {
                            d.recent_transactions
                                .iter()
                                .rev()
                                .take(count)
                                .map(TransactionRecord::terminal_lines)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .filter(|s| !s.is_empty());
                    // Ordinary reorgs share the quiet activity panel. Only a
                    // substantial game rollback uses the single-line alert.
                    let activity = match reorg_notice.filter(|(_, big)| !big) {
                        Some((notice, _)) => Some(match activity {
                            Some(activity) => format!("{notice}\n{activity}"),
                            None => notice.to_owned(),
                        }),
                        None => activity,
                    };
                    let alert = reorg_notice.filter(|(_, big)| *big).map(|(notice, _)| notice);
                    if terminal.draw_with_panels(&frame, activity.as_deref(), alert).is_err() {
                        quit.store(true, Ordering::Relaxed);
                        break;
                    }
                    if let Some((transaction, remaining)) = start_frame {
                        rendered_start = true;
                        crate::node::rpc::startup::diagnostic(&serde_json::json!({"type":"startup_timing", "phase":"accepted_start_first_render",
                            "transaction":transaction, "observed_unix_ms":crate::node::client::now_ms(),
                            "elapsed_ms":first_live.map(|at| at.elapsed().as_millis() as u64),
                            "opening_countdown_daa":crate::engine::OPENING_DELAY_DAA,
                            "opening_remaining_daa":remaining, "rendering_buffer_daa":clock.reserve}).to_string());
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
            }));
        } else if interactive {
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let direction = match gameplay_line(&line) {
                        Some(GameplayCommand::Exit) => {
                            quit.store(true, Ordering::Relaxed);
                            break;
                        }
                        Some(GameplayCommand::Up) => -1,
                        Some(GameplayCommand::Down) => 1,
                        _ => continue,
                    };
                    let movement = Movement { direction, pressed: Instant::now() };
                    let mut preview = preview.lock().expect("preview mutex");
                    preview.push(movement.clone());
                    if send.send(movement.clone()).is_err() {
                        preview.discard(movement.pressed);
                        break;
                    }
                }
            });
        }
        Ok(frontend)
    }
    /// Publish Host only after the accepted-Lobby watcher has captured its cursor.
    /// Game reorgs invalidate the offer; callers must publish a fresh one afterward.
    pub fn offer_rematch(&self, offer: Option<RematchOffer>) {
        let mut shared = self.rematch.lock().expect("rematch mutex");
        if offer.is_none() { shared.request = None; }
        if shared.offer == offer { return; }
        shared.offer = offer;
        shared.revision = shared.revision.wrapping_add(1);
    }
    pub fn take_rematch_request(&self) -> Option<RematchRequest> {
        self.rematch.lock().expect("rematch mutex").request.take()
    }
    pub fn update(&self, display: Display) {
        if let Some(latest) = &self.display {
            *latest.lock().expect("display mutex") = Some(display);
        }
    }
    pub fn movement_batch(&self, queue: &std::collections::VecDeque<Movement>) -> Vec<Movement> {
        self.preview.lock().expect("preview mutex").ready_movement_batch(queue, Instant::now())
    }
    /// Enqueue uses this same mutex. No key can enter between the final drain
    /// and the caller's cancellation/authorization or network handoff.
    pub fn with_inputs<R>(
        &self, queue: &mut std::collections::VecDeque<Movement>,
        commit: impl FnOnce(&mut InputPreview, &mut std::collections::VecDeque<Movement>) -> R,
    ) -> (R, Vec<Movement>) {
        let mut preview = self.preview.lock().expect("preview mutex");
        let received = self.input.try_iter().collect::<Vec<_>>();
        queue.extend(received.iter().cloned());
        queue.make_contiguous().sort_by_key(|movement| movement.pressed);
        (commit(&mut preview, queue), received)
    }
    pub fn cancel_unsent(
        &self,
        queue: &mut std::collections::VecDeque<Movement>,
        world: &World,
        side: i64,
        accepted: &[kaspa_consensus_core::Hash],
    ) {
        self.preview.lock().expect("preview mutex").cancel_unsent(queue, world, side, accepted);
    }
    pub fn submitted_movement(&self, movement: &Movement, txid: kaspa_consensus_core::Hash) {
        self.preview.lock().expect("preview mutex").submitted(movement.pressed, txid);
    }
    pub fn discard_movement(&self, movement: &Movement) {
        self.preview.lock().expect("preview mutex").discard(movement.pressed);
    }
    pub fn restore_movement(&self, movement: &Movement, txid: kaspa_consensus_core::Hash) {
        let mut preview = self.preview.lock().expect("preview mutex");
        if !preview.inputs.iter().any(|input| input.movement.pressed == movement.pressed) {
            preview.push(movement.clone());
            preview.inputs.make_contiguous().sort_by_key(|input| input.movement.pressed);
        }
        preview.submitted(movement.pressed, txid);
    }
    pub fn requeue_movement(&self, movement: &Movement) {
        self.preview.lock().expect("preview mutex").requeue(movement);
    }
    pub fn clear_preview(&self) {
        let mut preview = self.preview.lock().expect("preview mutex");
        preview.clear();
        // Hold the same lock used when queuing a preview and its input, so old
        // channel entries cannot reappear as invisible movement after a reset.
        for _ in self.input.try_iter() {}
    }
    pub fn accepted_previews(&self, tracker: &super::node::tracker::Tracker) -> Vec<kaspa_consensus_core::Hash> {
        self.preview.lock().expect("preview mutex").accepted(tracker)
    }
    pub fn observe_clock(&self, sample: ClockSample) {
        *self.clock.lock().expect("clock mutex") = Some(sample);
    }
    /// Runs concurrently with signing/funding/submission on the same RPC peer.
    /// It observes chain time only; accepted actors still come from replay.
    pub async fn follow_clock(&self, node: &super::node::rpc::Node, epoch: &std::cell::Cell<u64>) {
        loop {
            let before = epoch.get();
            if let Ok(info) = node.info().await {
                if epoch.get() == before {
                    self.observe_clock(ClockSample { epoch: before, daa: info.virtual_daa_score, observed: Instant::now() });
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    /// Most recent observed score, useful to diagnose clock-feed stalls.
    pub fn latest_clock(&self) -> Option<ClockSample> {
        *self.clock.lock().expect("clock mutex")
    }
}

pub fn clock_matches(display: &Display, sample: &ClockSample, now: Instant) -> bool {
    display.ready
        && display.clock_epoch == sample.epoch
        && now.saturating_duration_since(display.observed) <= SNAPSHOT_LIFETIME
        && now.saturating_duration_since(sample.observed) <= Duration::from_secs(2)
        && sample.daa >= display.daa
}
impl Drop for Frontend {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod reorg_notice_tests {
    use super::*;
    use crate::node::audit::Reaccepted;
    use kaspa_consensus_core::Hash;

    fn hash(n: u8) -> Hash {
        Hash::from_bytes([n; 32])
    }

    fn event(id: &str) -> ReorgEvent {
        ReorgEvent {
            version: 1,
            id: id.into(),
            observed_unix_ms: 0,
            game: hash(0),
            network: "testnet-10".into(),
            old_cursor: hash(1),
            new_cursor: hash(2),
            common_ancestor: Some(hash(3)),
            old_selected_daa: Some(100),
            common_ancestor_daa: Some(99),
            removed_daa_span: Some(1),
            removed_blocks: vec![hash(1)],
            added_blocks: vec![],
            removed_game_transactions: vec![],
            reaccepted: vec![],
            missing_game_transactions: vec![hash(4)],
            history_complete: true,
            discarded_local_inputs: 0,
            pending_local_transaction: None,
        }
    }

    #[test]
    fn quiet_vs_big_depends_on_distinct_game_actions_or_game_time() {
        let mut event = event("severity");
        event.removed_blocks = (0..100).map(hash).collect();
        event.removed_daa_span = Some(1_000);
        assert!(!event.is_big_game_rollback(), "global chain depth must not amplify the notice");
        event.reaccepted.push(Reaccepted { txid: hash(5), old_daa: Some(100), new_daa: 101 });
        assert!(!event.is_big_game_rollback(), "two affected game actions stay quiet");
        event.missing_game_transactions.push(hash(4));
        assert!(!event.is_big_game_rollback(), "duplicate transactions are not extra actions");
        event.missing_game_transactions.push(hash(6));
        assert!(event.is_big_game_rollback(), "three distinct lost/retimed actions are substantial");
        assert!(event.notice().starts_with("Game rollback:"));

        event.missing_game_transactions.clear();
        event.reaccepted[0].new_daa = 119;
        assert!(!event.is_big_game_rollback());
        event.reaccepted[0].new_daa = 120;
        assert!(event.is_big_game_rollback(), "20 DAA acceptance shift is substantial");
        event.reaccepted[0].new_daa = 80;
        assert!(event.is_big_game_rollback(), "earlier acceptance also shifts game time");
        event.reaccepted[0].new_daa = 100;
        assert!(!event.affects_game());
        let mut notices = ReorgNotices::default();
        notices.observe(Some(&event), Instant::now());
        assert!(notices.since.is_none(), "unchanged game acceptance stays silent");
    }

    #[test]
    fn quiet_and_big_notices_expire_at_two_seconds_without_snapshot_refresh() {
        let now = Instant::now();
        for big in [false, true] {
            let mut event = event("expiry");
            if big {
                event.missing_game_transactions.extend([hash(5), hash(6)]);
            }
            let mut notices = ReorgNotices::default();
            notices.observe(Some(&event), now);
            assert_eq!(notices.visible(now).unwrap().1, big);
            assert!(notices.visible(now).unwrap().0.starts_with(if big { "Game rollback:" } else { "Reorg:" }));
            let almost = now + Duration::from_millis(1999);
            notices.observe(Some(&event), almost);
            assert!(notices.visible(almost).is_some());
            let deadline = now + Duration::from_millis(2000);
            assert!(notices.visible(deadline).is_none());
            notices.observe(Some(&event), deadline);
            assert!(notices.visible(deadline).is_none(), "same cache snapshot must not restart expiry");
            notices.observe(None, deadline);
            notices.observe(Some(&event), deadline + Duration::from_secs(1));
            assert!(notices.visible(deadline + Duration::from_secs(1)).is_none(), "an old buffered event must stay expired");
        }
    }

    #[test]
    fn new_event_gets_its_own_window_but_dismissed_and_expired_ids_stay_seen() {
        let now = Instant::now();
        let first = event("first");
        let second = event("second");
        let mut notices = ReorgNotices::default();
        notices.observe(Some(&first), now);
        notices.dismiss();
        notices.observe(Some(&first), now + Duration::from_millis(1));
        assert!(notices.visible(now + Duration::from_millis(1)).is_none());
        let later = now + Duration::from_secs(3);
        notices.observe(Some(&second), later);
        assert!(notices.visible(later).is_some(), "a later new ID gets a fresh window");
        notices.observe(Some(&first), later + Duration::from_secs(1));
        assert!(notices.visible(later + Duration::from_millis(1999)).is_some());
        assert!(notices.visible(later + Duration::from_secs(2)).is_none(), "old IDs cannot renew the current notice");
        notices.observe(Some(&second), later + Duration::from_secs(2));
        assert!(notices.visible(later + Duration::from_secs(2)).is_none());
        notices.observe(Some(&first), later + Duration::from_secs(3));
        assert!(notices.visible(later + Duration::from_secs(3)).is_none(), "dismissed IDs stay suppressed after another event");
    }
}
