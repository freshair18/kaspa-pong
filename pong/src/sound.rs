//! Presentation-only arcade effects. No files, music, RPC or gameplay sleeps.
use crate::engine::{Finish, World, MAX_BALLS};
use std::{collections::HashSet, sync::atomic::{AtomicBool, Ordering}};
/// Voices remain alive across output blocks, so a new cue never truncates an
/// already playing ball. The limiter bounds the sum without selecting a winner.
#[derive(Default)]
struct Mixer {
    voices: Vec<(Effect, usize)>,
}
impl Mixer {
    fn add(&mut self, effects: impl IntoIterator<Item = Effect>) {
        self.voices.extend(effects.into_iter().map(|effect| (effect, 0)));
    }
    fn render(&mut self, output: &mut [i16]) {
        output.fill(0);
        for (offset, sample) in output.iter_mut().enumerate() {
            let sum: i32 = self.voices.iter().map(|&(effect, cursor)|
                i32::from(clip(effect).get(cursor + offset).copied().unwrap_or(0))).sum();
            *sample = sum.clamp(-9830, 9830) as i16;
        }
        for (_, cursor) in &mut self.voices { *cursor += output.len(); }
        self.voices.retain(|&(effect, cursor)| cursor < clip(effect).len());
    }
}
fn clip(effect: Effect) -> &'static [i16] {
    static CLIPS: std::sync::OnceLock<[Vec<i16>; Effect::ALL.len()]> = std::sync::OnceLock::new();
    &CLIPS.get_or_init(|| Effect::ALL.map(|effect|
        wave(effect)[44..].chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect()))[effect as usize]
}

#[cfg(windows)]
struct Output {
    sender: Option<std::sync::mpsc::SyncSender<Option<Vec<Effect>>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
#[cfg(windows)]
impl Output {
    fn new() -> Self {
        let (sender, receive) = std::sync::mpsc::sync_channel::<Option<Vec<Effect>>>(64);
        let worker = std::thread::Builder::new().name("pong-audio".into()).spawn(move || {
            let Some(mut device) = native::Device::new() else { return; };
            let mut mixer = Mixer::default();
            loop {
                match receive.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(Some(effects)) if !MUTED.load(Ordering::Relaxed) => mixer.add(effects),
                    Ok(_) => { mixer.voices.clear(); device.stop(); }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                while let Ok(command) = receive.try_recv() {
                    match command {
                        Some(effects) if !MUTED.load(Ordering::Relaxed) => mixer.add(effects),
                        _ => { mixer.voices.clear(); device.stop(); }
                    }
                }
                device.write(&mut mixer);
            }
        }).ok();
        Self { sender: worker.as_ref().map(|_| sender), worker }
    }
    fn play(&self, effects: Vec<Effect>) { if let Some(sender) = &self.sender { let _ = sender.try_send(Some(effects)); } }
    fn stop(&self) { if let Some(sender) = &self.sender { let _ = sender.try_send(None); } }
}
#[cfg(windows)]
impl Drop for Output {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}
#[cfg(not(windows))]
struct Output;
#[cfg(not(windows))]
impl Output {
    fn new() -> Self { Self }
    fn play(&self, _: Vec<Effect>) {}
    fn stop(&self) {}
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{ffi::{c_char, c_void}, ptr};
    type Handle = *mut c_void;
    #[repr(C)]
    struct Format {
        tag: u16, channels: u16, rate: u32, bytes_per_second: u32,
        align: u16, bits: u16, extra: u16,
    }
    #[repr(C)]
    struct Header {
        data: *mut c_char, length: u32, recorded: u32, user: usize,
        flags: u32, loops: u32, next: *mut Header, reserved: usize,
    }
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn waveOutOpen(handle: *mut Handle, device: u32, format: *const Format, callback: usize, instance: usize, flags: u32) -> u32;
        fn waveOutPrepareHeader(handle: Handle, header: *mut Header, size: u32) -> u32;
        fn waveOutWrite(handle: Handle, header: *mut Header, size: u32) -> u32;
        fn waveOutReset(handle: Handle) -> u32;
        fn waveOutUnprepareHeader(handle: Handle, header: *mut Header, size: u32) -> u32;
        fn waveOutClose(handle: Handle) -> u32;
    }
    const BLOCK_SAMPLES: usize = RATE as usize / 100;
    const HEADER_SIZE: u32 = std::mem::size_of::<Header>() as u32;
    struct Block {
        samples: [i16; BLOCK_SAMPLES],
        header: Header,
        queued: bool,
    }
    pub(super) struct Device {
        handle: Handle,
        // Boxes keep both PCM and WAVEHDR addresses stable until reset/close.
        blocks: Vec<Box<Block>>,
    }
    impl Device {
        pub(super) fn new() -> Option<Self> {
            let format = Format { tag: 1, channels: 1, rate: RATE, bytes_per_second: RATE * 2, align: 2, bits: 16, extra: 0 };
            let mut handle = ptr::null_mut();
            if unsafe { waveOutOpen(&mut handle, u32::MAX, &format, 0, 0, 0) } != 0 { return None; }
            let mut device = Self { handle, blocks: Vec::new() };
            for _ in 0..3 {
                let mut block = Box::new(Block {
                    samples: [0; BLOCK_SAMPLES],
                    header: Header { data: ptr::null_mut(), length: (BLOCK_SAMPLES * 2) as u32, recorded: 0, user: 0,
                        flags: 0, loops: 0, next: ptr::null_mut(), reserved: 0 },
                    queued: false,
                });
                block.header.data = block.samples.as_mut_ptr().cast();
                if unsafe { waveOutPrepareHeader(handle, &mut block.header, HEADER_SIZE) } != 0 { return None; }
                device.blocks.push(block);
            }
            Some(device)
        }
        pub(super) fn stop(&mut self) {
            unsafe { waveOutReset(self.handle); }
            for block in &mut self.blocks { block.queued = false; }
        }
        pub(super) fn write(&mut self, mixer: &mut Mixer) {
            for block in &mut self.blocks {
                // WHDR_DONE is written by the Windows audio device.
                let done = unsafe { ptr::read_volatile(&block.header.flags) } & 1 != 0;
                if (!block.queued || done) && !mixer.voices.is_empty() {
                    mixer.render(&mut block.samples);
                    block.queued = unsafe { waveOutWrite(self.handle, &mut block.header, HEADER_SIZE) } == 0;
                }
            }
        }
    }
    impl Drop for Device {
        fn drop(&mut self) {
            self.stop();
            for block in &mut self.blocks {
                unsafe { waveOutUnprepareHeader(self.handle, &mut block.header, HEADER_SIZE); }
            }
            unsafe { waveOutClose(self.handle); }
        }
    }
}
static MUTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect { Paddle, Wall, Goal, Serve, SparkleA, SparkleB, SparkleC, Win, Lose, SparkleA2, SparkleA3, SparkleB2, SparkleB3, SparkleC2, SparkleC3 }
impl Effect {
    const ALL: [Self; 15] = [Self::Paddle, Self::Wall, Self::Goal, Self::Serve, Self::SparkleA, Self::SparkleB, Self::SparkleC, Self::Win, Self::Lose,
        Self::SparkleA2, Self::SparkleA3, Self::SparkleB2, Self::SparkleB3, Self::SparkleC2, Self::SparkleC3];
}
fn sparkle(index: usize, cycle: u64) -> Effect {
    const FAMILIES: [[Effect; 3]; 3] = [
        [Effect::SparkleA, Effect::SparkleA2, Effect::SparkleA3],
        [Effect::SparkleB, Effect::SparkleB2, Effect::SparkleB3],
        [Effect::SparkleC, Effect::SparkleC2, Effect::SparkleC3],
    ];
    // Slots 3–7 reuse the three families, advancing a variant every three slots.
    FAMILIES[index % 3][(cycle % 3 + (index / 3) as u64) as usize % 3]
}

/// Keys include the ball slot and survive timeline resets.
#[derive(Default)]
struct Cues {
    /// Initial history stays silent after a rewind, just like visual goal notices.
    baseline_daa: Option<i64>,
    last: Option<World>,
    last_daa: Option<u64>,
    seen: HashSet<(usize, u8, i64, i64, i64)>,
}
impl Cues {
    fn goal(&mut self, index: usize, left: i64, right: i64, at: i64) -> Option<Effect> {
        if self.baseline_daa.is_some_and(|baseline| at <= baseline) { return None; }
        let scores = if index == usize::MAX { (left, right) } else { (0, 0) };
        self.seen.insert((index, 3, at, scores.0, scores.1)).then_some(Effect::Goal)
    }
    #[cfg(test)]
    fn world(&mut self, world: &World, displayed_daa: u64) -> Vec<Effect> {
        self.world_at(world, displayed_daa, [None; MAX_BALLS])
    }
    fn world_at(&mut self, world: &World, displayed_daa: u64, goals: [Option<i64>; MAX_BALLS]) -> Vec<Effect> {
        self.baseline_daa.get_or_insert(world.clock);
        let previous = self.last.replace(world.clone());
        let previous_daa = self.last_daa.replace(displayed_daa);
        let (Some(previous), Some(previous_daa)) = (previous, previous_daa) else { return vec![]; };
        if world.clock < previous.clock || world.left_score < previous.left_score || world.right_score < previous.right_score {
            return vec![];
        }
        let mut effects = Vec::new();
        for index in 0..world.ball_count() {
            let ball = world.ball(index);
            let before = previous.ball(index);
            if ball.serve_until != before.serve_until
                && (world.left_score > previous.left_score || world.right_score > previous.right_score) {
                let at = goals[index].unwrap_or(world.clock);
                if let Some(effect) = self.goal(index, world.left_score, world.right_score, at) { effects.push(effect); }
                continue;
            }
            let mut cue = |tag, effect, at| {
                if self.seen.insert((index, tag, at, ball.serve_until, 0)) { effects.push(effect); }
            };
            if previous.clock < ball.serve_until && world.clock >= ball.serve_until && !world.ended() {
                cue(4, Effect::Serve, ball.serve_until);
            } else if before.serve_until == ball.serve_until && previous.clock >= before.serve_until && !world.ended() {
                if ball.vx != before.vx { cue(1, Effect::Paddle, world.clock); }
                else if ball.vy != 0 && before.vy != 0 && ball.vy.signum() != before.vy.signum() {
                    cue(2, Effect::Wall, world.clock);
                }
                // Sparkles and contacts can occur in the same frame.
                if previous_daa / 18 != displayed_daa / 18 {
                    cue(6, sparkle(index, displayed_daa / 18), (displayed_daa / 18 * 18) as i64);
                }
            }
        }
        effects
    }
    fn reset_timeline(&mut self) {
        self.last = None;
        self.last_daa = None;
    }
    fn finished_goal(&mut self, result: &Finish) -> Option<Effect> {
        let previous = self.last.as_mut()?;
        let increased = result.left_score >= previous.left_score && result.right_score >= previous.right_score
            && (result.left_score > previous.left_score || result.right_score > previous.right_score);
        previous.left_score = result.left_score;
        previous.right_score = result.right_score;
        if increased { self.goal(usize::MAX, result.left_score, result.right_score, result.end_daa) } else { None }
    }
    fn outcome(&mut self, result: &Finish, side: Option<i64>) -> Option<Effect> {
        let side = side?;
        if result.winner < 0 || !self.seen.insert((usize::MAX, 5, result.end_daa, result.winner, 0)) { return None; }
        Some(if side == result.winner { Effect::Win } else { Effect::Lose })
    }
}

pub struct GameAudio {
    cues: Cues,
    snapshot: Option<crate::engine::Snapshot>,
    output: Output,
}
impl GameAudio {
    pub fn new() -> Self { Self { cues: Cues::default(), snapshot: None, output: Output::new() } }
    pub fn world(&mut self, snapshot: &crate::engine::Snapshot, world: &World, displayed_daa: u64) {
        let goals = std::array::from_fn(|index| {
            if index >= world.ball_count() || self.cues.last.as_ref().is_none_or(|last|
                last.ball(index).serve_until == world.ball(index).serve_until) { return None; }
            crate::ascii::before_ball_goal(snapshot, self.snapshot.as_ref(), &world.with_ball(index), index).map(|(_, at)| at)
        });
        let effects = self.cues.world_at(world, displayed_daa, goals);
        self.snapshot = Some(snapshot.clone());
        self.emit(effects);
    }
    pub fn finished_goal(&mut self, result: &Finish) {
        if let Some(snapshot) = self.snapshot.clone() {
            if let Ok(projected) = crate::engine::project(&snapshot, result.end_daa.max(0) as u64) {
                if (projected.world.left_score, projected.world.right_score) == (result.left_score, result.right_score) {
                    self.world(&snapshot, &projected.world, result.end_daa.max(0) as u64);
                }
            }
        }
        let cue = self.cues.finished_goal(result);
        self.emit(cue.into_iter().collect());
    }
    pub fn outcome(&mut self, result: &Finish, side: Option<i64>) {
        let cue = self.cues.outcome(result, side); self.emit(cue.into_iter().collect());
    }
    pub fn reset_timeline(&mut self) {
        self.cues.reset_timeline();
        self.snapshot = None;
        self.output.stop();
    }
    pub fn toggle(&self) -> bool {
        let muted = !MUTED.fetch_xor(true, Ordering::Relaxed);
        self.output.stop();
        muted
    }
    fn emit(&self, effects: Vec<Effect>) {
        // Advance deduplication while muted; unmuting never replays a backlog.
        if !effects.is_empty() && !MUTED.load(Ordering::Relaxed) { self.output.play(effects); }
    }
}

const RATE: u32 = 22_050;
/// Short arcade sweeps with attack/release envelopes to avoid clicks.
fn wave(effect: Effect) -> Vec<u8> {
    let notes: &[(f32, f32, u32)] = match effect {
        Effect::Paddle => &[(880.0, 660.0, 45)],
        Effect::Wall => &[(520.0, 360.0, 52), (390.0, 280.0, 34)],
        Effect::Goal => &[(220.0, 100.0, 85), (110.0, 55.0, 110)],
        Effect::Serve => &[(660.0, 1320.0, 90)],
        Effect::SparkleA => &[(1760.0, 2640.0, 24)],
        Effect::SparkleB => &[(3136.0, 1568.0, 28)],
        Effect::SparkleC => &[(1568.0, 2352.0, 18), (2352.0, 1760.0, 14)],
        Effect::SparkleA2 => &[(1975.0, 2960.0, 26)],
        Effect::SparkleA3 => &[(1568.0, 2352.0, 22)],
        Effect::SparkleB2 => &[(2794.0, 1397.0, 26)],
        Effect::SparkleB3 => &[(3520.0, 1760.0, 24)],
        Effect::SparkleC2 => &[(1318.0, 1975.0, 20), (1975.0, 1480.0, 16)],
        Effect::SparkleC3 => &[(1760.0, 2640.0, 16), (2640.0, 2093.0, 18)],
        Effect::Win => &[(523.25, 523.25, 70), (659.25, 659.25, 70), (784.0, 784.0, 70), (1046.5, 1046.5, 130)],
        Effect::Lose => &[(392.0, 392.0, 80), (330.0, 330.0, 80), (262.0, 262.0, 80), (131.0, 98.0, 140)],
    };
    let mut pcm = Vec::new();
    for &(start, end, millis) in notes {
        let count = (RATE * millis / 1000) as usize;
        let mut phase = 0.0_f32;
        for i in 0..count {
            let t = i as f32 / count as f32;
            phase = (phase + (start + (end - start) * t) / RATE as f32).fract();
            let envelope = (i as f32 / (RATE as f32 * 0.002))
                .min((count - i - 1) as f32 / (RATE as f32 * 0.008)).clamp(0.0, 1.0);
            let carrier = if effect == Effect::Wall {
                (std::f32::consts::TAU * phase).sin()
            } else if phase < 0.5 {
                1.0
            } else {
                -1.0
            };
            let gain = if effect == Effect::Wall { 0.18 } else { 0.10 };
            let sample = carrier * envelope * gain * i16::MAX as f32;
            pcm.extend_from_slice(&(sample as i16).to_le_bytes());
        }
        pcm.extend(std::iter::repeat_n(0, (RATE / 100 * 2) as usize));
    }
    let bytes = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes()); wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data"); wav.extend_from_slice(&bytes.to_le_bytes()); wav.extend_from_slice(&pcm);
    wav
}


#[cfg(test)]
mod tests {
    use super::*;
    fn world() -> World {
        let mut w = World::opening(vec![1; 32], vec![2; 32]);
        w.started = true; w.clock = 100; w.serve_until = 90; w.ball_vy = 1;
        w
    }
    #[test]
    fn sound_contacts_follow_displayed_frames_and_do_not_repeat_after_rewind() {
        let mut cues = Cues::default();
        let mut w = world();
        assert_eq!(cues.world(&w, 100), vec![], "first snapshot is a silent baseline");
        w.clock += 7; w.ball_vx = -w.ball_vx;
        assert_eq!(cues.world(&w, 107), vec![Effect::Paddle]);
        assert_eq!(cues.world(&w, 107), vec![]);
        let paddle = w.clone();
        w.clock += 7; w.ball_vy = -w.ball_vy;
        assert_eq!(cues.world(&w, 114), vec![Effect::Wall, sparkle(0, 6)]);
        cues.last = None;
        cues.last_daa = None;
        assert_eq!(cues.world(&world(), 100), vec![]);
        assert_eq!(cues.world(&paddle, 107), vec![], "reorg replay must stay silent");
        let mut corrected = world(); corrected.clock = 50;
        assert_eq!(cues.world(&corrected, 50), vec![], "clock rewind establishes a baseline");
    }
    #[test]
    fn sound_goal_and_finished_goal_share_identity_and_timeout_has_no_fake_goal() {
        let mut cues = Cues::default(); let mut w = world(); cues.world(&w, 100);
        w.clock = 120; w.left_score = 1; w.serve_until = 140;
        assert_eq!(cues.world(&w, 120), vec![Effect::Goal]);
        let mut finish = w.result(); finish.end_daa = 120;
        assert_eq!(cues.finished_goal(&finish), None);
        finish.left_score = 3; finish.winner = 0; finish.end_daa = 200;
        assert_eq!(cues.finished_goal(&finish), Some(Effect::Goal), "settlement can skip the final live frame");
        assert_eq!(cues.finished_goal(&finish), None);
        assert_eq!(cues.outcome(&finish, Some(0)), Some(Effect::Win));
        assert_eq!(cues.outcome(&finish, Some(0)), None, "collecting -> paid must not repeat");
        let mut other = Cues::default(); other.world(&world(), 100);
        let mut timeout = world().result(); timeout.winner = 0;
        assert_eq!(other.finished_goal(&timeout), None);
        assert_eq!(other.outcome(&timeout, Some(1)), Some(Effect::Lose));
        assert_eq!(Cues::default().outcome(&timeout, None), None, "observer has no personal victory cue");
    }
    #[test]
    fn initial_goal_history_stays_silent_after_reset_and_new_goals_still_play() {
        for count in [1, 3, 6, 8] {
            let mut loaded = world(); loaded.clock = 110; loaded.ball_count = count; loaded.win_score = 9;
            loaded.left_score = count; loaded.serve_until = 200;
            loaded.extra_balls = [loaded.ball(0); MAX_BALLS - 1];
            let mut cues = Cues::default();
            assert!(cues.world(&loaded, 110).is_empty());
            cues.reset_timeline();
            let mut before = loaded.clone(); before.clock = 106; before.left_score = 0; before.serve_until = 90;
            before.extra_balls = [before.ball(0); MAX_BALLS - 1];
            assert!(cues.world(&before, 106).is_empty());
            let mut replay = loaded.clone(); replay.clock = 109;
            let goals = std::array::from_fn(|i| (i < count as usize).then_some(107 + (i % 3) as i64));
            assert!(cues.world_at(&replay, 109, goals).is_empty(), "initial historical goals must not chime on replay");
            cues.reset_timeline();
            cues.world(&before, 106);
            let mut finish = replay.result(); finish.end_daa = 109;
            assert_eq!(cues.finished_goal(&finish), None, "settlement cannot replay initial goal history");
            cues.world_at(&replay, 109, goals);
            let mut fresh = replay.clone(); fresh.clock = 120; fresh.left_score += 1; fresh.serve_until = 300;
            assert_eq!(cues.world_at(&fresh, 120, std::array::from_fn(|i| (i == 0).then_some(120))), vec![Effect::Goal]);
            assert!(cues.world_at(&fresh, 120, std::array::from_fn(|i| (i == 0).then_some(120))).is_empty());
        }
    }
    #[test]
    fn sound_serve_and_muted_history_do_not_replay_old_events() {
        let mut cues = Cues::default(); let mut w = world(); w.clock = 80;
        cues.world(&w, 80); w.clock = 90;
        assert_eq!(cues.world(&w, 90), vec![Effect::Serve]);
        assert_eq!(cues.world(&w, 90), vec![]);
        // Muting affects delivery only; cue tracking keeps consuming frames.
        w.clock = 97; w.ball_vx = -w.ball_vx;
        let _not_delivered_while_muted = cues.world(&w, 97);
        assert_eq!(cues.world(&w, 97), vec![]);
    }
    #[test]
    fn sparkle_sound_follows_the_displayed_daa_cycle() {
        let mut cues = Cues::default();
        let mut w = world();
        cues.world(&w, 17);
        w.clock += 1;
        assert_eq!(cues.world(&w, 18), vec![sparkle(0, 1)]);
    }
    #[test]
    fn three_ball_families_vary_and_simultaneous_contacts_do_not_hide_sparkles() {
        let mut w = world();
        w.ball_count = 3;
        w.win_score = 6;
        w.extra_balls = [w.ball(0); MAX_BALLS - 1];
        let mut cues = Cues::default();
        assert!(cues.world(&w, 17).is_empty());
        w.clock += 1;
        w.extra_balls[0].vx = -w.extra_balls[0].vx;
        assert_eq!(cues.world(&w, 18), vec![sparkle(0, 1), Effect::Paddle, sparkle(1, 1), sparkle(2, 1)]);
        assert!(cues.world(&w, 18).is_empty());
        for cycle in 2..=4 {
            w.clock += 1;
            assert_eq!(cues.world(&w, cycle * 18), (0..3).map(|i| sparkle(i, cycle)).collect::<Vec<_>>());
        }
        let mut clips = HashSet::new();
        for index in 0..3 {
            for cycle in 0..3 { assert!(clips.insert(wave(sparkle(index, cycle))), "all nine variants differ"); }
        }
        // Each ball has its own dedup key, retained when playback is reset.
        cues.last = None;
        cues.last_daa = None;
        cues.world(&w, 17);
        w.clock += 1;
        assert!(cues.world(&w, 18).is_empty());
    }
    #[test]
    fn simultaneous_goals_are_independent_and_settlement_does_not_repeat_them() {
        let mut w = world(); w.ball_count = 3; w.win_score = 6;
        w.extra_balls = [w.ball(0); MAX_BALLS - 1];
        let mut cues = Cues::default(); cues.world(&w, 100);
        w.clock = 107; w.left_score = 1; w.right_score = 1;
        w.serve_until = 200; w.extra_balls[1].serve_until = 210;
        assert_eq!(cues.world_at(&w, 107, std::array::from_fn(|i| matches!(i, 0 | 2).then_some(107))), vec![Effect::Goal, Effect::Goal]);
        assert_eq!(cues.finished_goal(&w.result()), None);
        cues.last = None; cues.last_daa = None;
        let mut baseline = w.clone(); baseline.left_score = 0; baseline.right_score = 0;
        baseline.serve_until = 90; baseline.extra_balls[1].serve_until = 90;
        cues.world(&baseline, 100);
        assert!(cues.world_at(&w, 107, std::array::from_fn(|i| matches!(i, 0 | 2).then_some(107))).is_empty());
    }
    #[test]
    fn eight_slots_reuse_clips_but_keep_independent_contact_serve_and_goal_identities() {
        for index in 3..MAX_BALLS {
            for cycle in 0..3 {
                assert_eq!(sparkle(index, cycle), sparkle(index - 3, cycle + 1));
                assert_ne!(sparkle(index, cycle), sparkle(index - 3, cycle));
            }
        }
        let mut w = world(); w.ball_count = MAX_BALLS as i64; w.win_score = 15;
        w.extra_balls = [w.ball(0); MAX_BALLS - 1];
        let mut cues = Cues::default(); cues.world(&w, 17);
        w.clock += 1; w.ball_vx = -w.ball_vx;
        for ball in &mut w.extra_balls { ball.vx = -ball.vx; }
        let effects = cues.world(&w, 18);
        let expected: Vec<_> = (0..MAX_BALLS).flat_map(|i| [Effect::Paddle, sparkle(i, 1)]).collect();
        assert_eq!(effects, expected);
        let mut mixer = Mixer::default(); mixer.add(effects);
        mixer.render(&mut [0; 220]);
        assert!(cues.world(&w, 18).is_empty());
        let before = w.clone();
        cues.reset_timeline(); cues.world(&before, 17);
        assert!(cues.world(&w, 18).is_empty(), "reused audio retains per-slot replay keys");

        w.clock = 110; w.left_score = MAX_BALLS as i64; w.serve_until = 120;
        for ball in &mut w.extra_balls { ball.serve_until = 120; }
        assert_eq!(cues.world_at(&w, 110, [Some(110); MAX_BALLS]), vec![Effect::Goal; MAX_BALLS]);
        assert_eq!(cues.finished_goal(&w.result()), None);
        w.clock = 120;
        assert_eq!(cues.world(&w, 120), vec![Effect::Serve; MAX_BALLS]);
        assert!(cues.world(&w, 120).is_empty());
        cues.reset_timeline(); cues.world(&before, 18);
        assert!(cues.world_at(&w, 120, [Some(110); MAX_BALLS]).is_empty());
    }
    #[test]
    fn pcm_mixer_sums_simultaneous_and_overlapping_voices_across_blocks() {
        let mut mixer = Mixer::default();
        mixer.add([Effect::SparkleA, Effect::SparkleB, Effect::SparkleC]);
        let mut first = [0; 220]; mixer.render(&mut first);
        for (i, &sample) in first.iter().enumerate() {
            let sum: i32 = [Effect::SparkleA, Effect::SparkleB, Effect::SparkleC].into_iter()
                .map(|effect| i32::from(clip(effect)[i])).sum();
            assert_eq!(sample, sum.clamp(-9830, 9830) as i16);
        }
        mixer.add([Effect::Paddle]);
        let mut second = [0; 220]; mixer.render(&mut second);
        for (i, &sample) in second.iter().enumerate() {
            let sum: i32 = [Effect::SparkleA, Effect::SparkleB, Effect::SparkleC].into_iter()
                .map(|effect| i32::from(clip(effect).get(i + 220).copied().unwrap_or(0))).sum::<i32>()
                + i32::from(clip(Effect::Paddle)[i]);
            assert_eq!(sample, sum.clamp(-9830, 9830) as i16);
        }
        assert!(first.iter().chain(&second).any(|&sample| sample != 0));
        for _ in 0..50 { mixer.render(&mut second); }
        assert!(mixer.voices.is_empty());
        assert!(second.iter().all(|&sample| sample == 0));
    }
    #[test]
    fn sound_clips_are_bounded_quiet_pcm_with_valid_lengths_and_envelopes() {
        for effect in Effect::ALL {
            let bytes = wave(effect);
            assert_eq!(&bytes[..4], b"RIFF"); assert_eq!(&bytes[8..16], b"WAVEfmt ");
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8, bytes.len());
            assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize + 44, bytes.len());
            assert!(bytes.len() > 1000 && bytes.len() < RATE as usize * 2 / 2 + 44);
            let samples: Vec<_> = bytes[44..].chunks_exact(2).map(|s| i16::from_le_bytes(s.try_into().unwrap())).collect();
            assert!(samples.iter().any(|s| *s != 0));
            let peak = if effect == Effect::Wall { 5899 } else { 3277 };
            assert!(samples.iter().all(|s| s.unsigned_abs() <= peak), "{effect:?} exceeds its configured gain");
            assert_eq!(samples[0], 0); assert_eq!(*samples.last().unwrap(), 0);
        }
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "audible local Windows device smoke check; run explicitly"]
    fn sound_windows_device_smoke() {
        let mut device = native::Device::new().expect("Windows audio device rejected PCM output");
        let mut mixer = Mixer::default();
        mixer.add([Effect::SparkleA, Effect::SparkleB, Effect::SparkleC]);
        for _ in 0..15 {
            device.write(&mut mixer);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        device.stop();
    }
}
