//! Presentation-only arcade effects. No files, music, RPC or gameplay sleeps.
use crate::engine::{Finish, World, SERVE_DELAY_DAA};
use std::{collections::HashSet, sync::atomic::{AtomicBool, Ordering}};

static MUTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect { Paddle, Wall, Goal, Serve, Win, Lose }
impl Effect {
    const ALL: [Self; 6] = [Self::Paddle, Self::Wall, Self::Goal, Self::Serve, Self::Win, Self::Lose];
    fn priority(self) -> u8 {
        match self { Self::Win | Self::Lose => 3, Self::Goal => 2, Self::Serve => 1, _ => 0 }
    }
}

/// Keys survive timeline resets so replayed snapshots cannot replay sounds.
#[derive(Default)]
struct Cues {
    last: Option<World>,
    seen: HashSet<(u8, i64, i64, i64, i64)>,
}
impl Cues {
    fn goal(&mut self, left: i64, right: i64, at: i64) -> Option<Effect> {
        self.seen.insert((3, at, 0, left, right)).then_some(Effect::Goal)
    }
    fn world(&mut self, world: &World) -> Option<Effect> {
        let previous = self.last.replace(world.clone())?;
        if world.clock < previous.clock || world.left_score < previous.left_score || world.right_score < previous.right_score {
            return None;
        }
        if world.left_score > previous.left_score || world.right_score > previous.right_score {
            let at = if world.ended() { world.clock } else { world.serve_until - SERVE_DELAY_DAA };
            return self.goal(world.left_score, world.right_score, at);
        }
        let (tag, effect, at) = if previous.clock < world.serve_until && world.clock >= world.serve_until {
            (4, Effect::Serve, world.serve_until)
        } else if previous.serve_until == world.serve_until && previous.clock >= previous.serve_until {
            if world.ball_vx != previous.ball_vx { (1, Effect::Paddle, world.clock) }
            else if world.ball_vy != 0 && previous.ball_vy != 0 && world.ball_vy.signum() != previous.ball_vy.signum() {
                (2, Effect::Wall, world.clock)
            } else { return None; }
        } else { return None; };
        self.seen.insert((tag, at, world.serve_until, world.left_score, world.right_score)).then_some(effect)
    }
    fn finished_goal(&mut self, result: &Finish) -> Option<Effect> {
        let previous = self.last.as_mut()?;
        let increased = result.left_score >= previous.left_score && result.right_score >= previous.right_score
            && (result.left_score > previous.left_score || result.right_score > previous.right_score);
        previous.left_score = result.left_score;
        previous.right_score = result.right_score;
        if increased { self.goal(result.left_score, result.right_score, result.end_daa) } else { None }
    }
    fn outcome(&mut self, result: &Finish, side: Option<i64>) -> Option<Effect> {
        let side = side?;
        if result.winner < 0 || !self.seen.insert((5, result.end_daa, result.winner, result.left_score, result.right_score)) {
            return None;
        }
        Some(if side == result.winner { Effect::Win } else { Effect::Lose })
    }
}

pub struct GameAudio {
    cues: Cues,
    output: Output,
}
impl GameAudio {
    pub fn new() -> Self { Self { cues: Cues::default(), output: Output::new() } }
    pub fn world(&mut self, world: &World) { let cue = self.cues.world(world); self.emit(cue); }
    pub fn finished_goal(&mut self, result: &Finish) { let cue = self.cues.finished_goal(result); self.emit(cue); }
    pub fn outcome(&mut self, result: &Finish, side: Option<i64>) { let cue = self.cues.outcome(result, side); self.emit(cue); }
    pub fn reset_timeline(&mut self) {
        self.cues.last = None;
        self.output.stop();
    }
    pub fn toggle(&self) -> bool {
        let muted = !MUTED.fetch_xor(true, Ordering::Relaxed);
        self.output.stop();
        muted
    }
    fn emit(&self, effect: Option<Effect>) {
        // Advance deduplication even while muted; unmuting never replays a backlog.
        if !MUTED.load(Ordering::Relaxed) { if let Some(effect) = effect { self.output.play(effect); } }
    }
}

const RATE: u32 = 22_050;
/// Short square-wave sweeps with attack/release envelopes to avoid clicks.
fn wave(effect: Effect) -> Vec<u8> {
    let notes: &[(f32, f32, u32)] = match effect {
        Effect::Paddle => &[(880.0, 660.0, 45)],
        Effect::Wall => &[(360.0, 280.0, 28)],
        Effect::Goal => &[(220.0, 100.0, 85), (110.0, 55.0, 110)],
        Effect::Serve => &[(660.0, 1320.0, 90)],
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
            let sample = (if phase < 0.5 { 1.0 } else { -1.0 }) * envelope * 0.10 * i16::MAX as f32;
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

#[cfg(windows)]
struct Output {
    sender: Option<std::sync::mpsc::SyncSender<Option<Effect>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
#[cfg(windows)]
impl Output {
    fn new() -> Self {
        let (sender, receive) = std::sync::mpsc::sync_channel::<Option<Effect>>(8);
        let worker = std::thread::Builder::new().name("pong-audio".into()).spawn(move || {
            while let Ok(mut next) = receive.recv() {
                while let Ok(command) = receive.try_recv() {
                    match command {
                        None => { native::stop(); next = None; }
                        Some(effect) if next.is_none_or(|old| effect.priority() >= old.priority()) => next = Some(effect),
                        _ => {}
                    }
                }
                match next {
                    Some(effect) if !MUTED.load(Ordering::Relaxed) => { native::play(effect); }
                    _ => native::stop(),
                }
            }
            native::stop();
        }).ok();
        Self { sender: worker.as_ref().map(|_| sender), worker }
    }
    fn play(&self, effect: Effect) { if let Some(sender) = &self.sender { let _ = sender.try_send(Some(effect)); } }
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
    fn play(&self, _: Effect) {}
    fn stop(&self) {}
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{ffi::c_void, ptr, sync::OnceLock};
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn PlaySoundW(sound: *const u16, module: *mut c_void, flags: u32) -> i32;
    }
    pub(super) fn play(effect: Effect) -> bool {
        // ASYNC|MEMORY requires buffers to outlive playback. Static clips are
        // retained until process exit, including during worker shutdown.
        static CLIPS: OnceLock<[Vec<u8>; 6]> = OnceLock::new();
        let clips = CLIPS.get_or_init(|| Effect::ALL.map(wave));
        unsafe { PlaySoundW(clips[effect as usize].as_ptr().cast(), ptr::null_mut(), 0x0001 | 0x0002 | 0x0004) != 0 }
    }
    pub(super) fn stop() { unsafe { PlaySoundW(ptr::null(), ptr::null_mut(), 0); } }
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
        assert_eq!(cues.world(&w), None, "first snapshot is a silent baseline");
        w.clock += 7; w.ball_vx = -w.ball_vx;
        assert_eq!(cues.world(&w), Some(Effect::Paddle));
        assert_eq!(cues.world(&w), None);
        let paddle = w.clone();
        w.clock += 7; w.ball_vy = -w.ball_vy;
        assert_eq!(cues.world(&w), Some(Effect::Wall));
        cues.last = None;
        assert_eq!(cues.world(&world()), None);
        assert_eq!(cues.world(&paddle), None, "reorg replay must stay silent");
        let mut corrected = world(); corrected.clock = 50;
        assert_eq!(cues.world(&corrected), None, "clock rewind establishes a baseline");
    }
    #[test]
    fn sound_goal_and_finished_goal_share_identity_and_timeout_has_no_fake_goal() {
        let mut cues = Cues::default(); let mut w = world(); cues.world(&w);
        w.clock = 120; w.left_score = 1; w.serve_until = 140;
        assert_eq!(cues.world(&w), Some(Effect::Goal));
        let mut finish = w.result(); finish.end_daa = 120;
        assert_eq!(cues.finished_goal(&finish), None);
        finish.left_score = 3; finish.winner = 0; finish.end_daa = 200;
        assert_eq!(cues.finished_goal(&finish), Some(Effect::Goal), "settlement can skip the final live frame");
        assert_eq!(cues.finished_goal(&finish), None);
        assert_eq!(cues.outcome(&finish, Some(0)), Some(Effect::Win));
        assert_eq!(cues.outcome(&finish, Some(0)), None, "collecting -> paid must not repeat");
        let mut other = Cues::default(); other.world(&world());
        let mut timeout = world().result(); timeout.winner = 0;
        assert_eq!(other.finished_goal(&timeout), None);
        assert_eq!(other.outcome(&timeout, Some(1)), Some(Effect::Lose));
        assert_eq!(Cues::default().outcome(&timeout, None), None, "observer has no personal victory cue");
    }
    #[test]
    fn sound_serve_and_muted_history_do_not_replay_old_events() {
        let mut cues = Cues::default(); let mut w = world(); w.clock = 80;
        cues.world(&w); w.clock = 90;
        assert_eq!(cues.world(&w), Some(Effect::Serve));
        assert_eq!(cues.world(&w), None);
        // Muting affects delivery only; cue tracking keeps consuming frames.
        w.clock = 97; w.ball_vx = -w.ball_vx;
        let _not_delivered_while_muted = cues.world(&w);
        assert_eq!(cues.world(&w), None);
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
            assert!(samples.iter().all(|s| s.unsigned_abs() <= 3277));
            assert_eq!(samples[0], 0); assert_eq!(*samples.last().unwrap(), 0);
        }
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "audible local Windows device smoke check; run explicitly"]
    fn sound_windows_device_smoke() {
        assert!(native::play(Effect::Serve), "Windows audio device rejected the generated PCM cue");
        std::thread::sleep(std::time::Duration::from_millis(140));
        native::stop();
    }
}
