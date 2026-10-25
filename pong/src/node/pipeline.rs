//! Bounded chain fetch/validation and a single ordered disk writer. Only the
//! gameplay coordinator owns reservations and can authorize a submission.
use super::{
    Config, audit,
    client::{Cache, now_ms},
    rpc::{Delta, Node},
    tracker::Invitation,
};
use crate::engine::Result;
use argent_runtime::Artifact;
use kaspa_consensus_core::Hash;
use serde::Serialize;
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

struct RawBatch {
    epoch: u64,
    cursor: Hash,
    arrived_ms: u64,
    result: std::result::Result<Delta, String>,
}

/// The fetch cursor is speculative, never spendable state. Validation consumes
/// batches in order; deep replay resets the reader instead of reusing old data.
pub struct ChainFeed {
    control: Arc<Mutex<(u64, Hash)>>,
    receiver: mpsc::Receiver<RawBatch>,
    worker: tokio::task::JoinHandle<()>,
    expected: Hash,
    epoch: u64,
    pub arrivals: HashMap<Hash, u64>,
}
impl ChainFeed {
    async fn finish(&mut self) {
        self.worker.abort();
        let _ = (&mut self.worker).await;
    }
    pub fn new(node: Node, cursor: Hash, poll_ms: u64) -> Self {
        let control = Arc::new(Mutex::new((0u64, cursor)));
        let shared = control.clone();
        let (sender, receiver) = mpsc::sync_channel(2);
        let worker = tokio::spawn(async move {
            let mut notifications = None;
            let mut notification_attempted = false;
            let mut epoch = 0;
            let mut cursor = cursor;
            loop {
                let desired = *shared.lock().unwrap();
                if desired.0 != epoch {
                    (epoch, cursor) = desired;
                }
                let result = node.delta(cursor).await.map_err(|e| e.to_string());
                if shared.lock().unwrap().0 != epoch {
                    continue;
                }
                let next = result.as_ref().ok().map(|d| d.added.last().map(|b| b.hash).or(d.common).unwrap_or(cursor));
                let mut packet = RawBatch { epoch, cursor, arrived_ms: now_ms(), result };
                loop {
                    if shared.lock().unwrap().0 != epoch {
                        break;
                    }
                    match sender.try_send(packet) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Full(p)) => packet = p,
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                if let Some(next) = next {
                    cursor = next;
                } else {
                    return;
                }
                // Do the first accepted-chain read immediately, as before.
                // The subscription may take one RPC round trip, so start it
                // only afterwards and bound it to the former poll interval.
                if !notification_attempted {
                    notification_attempted = true;
                    notifications = tokio::time::timeout(Duration::from_millis(poll_ms), node.virtual_chain_feed())
                        .await
                        .ok()
                        .and_then(|result| result.ok());
                    continue;
                }
                if let Some(feed) = &notifications {
                    tokio::select! {
                        result = feed.changed() => {
                            if result.is_err() { return; }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(poll_ms)) => {}
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(poll_ms)).await;
                }
            }
        });
        Self { control, receiver, worker, expected: cursor, epoch: 0, arrivals: HashMap::new() }
    }
    pub async fn next(&mut self, cursor: Hash) -> Result<Delta> {
        if cursor != self.expected {
            self.epoch = self.epoch.wrapping_add(1);
            *self.control.lock().unwrap() = (self.epoch, cursor);
            self.expected = cursor;
            self.arrivals.clear();
        }
        loop {
            match self.receiver.try_recv() {
                Ok(packet) if packet.epoch == self.epoch && packet.cursor == cursor => {
                    let delta = packet.result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    for block in &delta.added {
                        self.arrivals.insert(block.hash, packet.arrived_ms);
                    }
                    self.expected = delta.added.last().map(|b| b.hash).or(delta.common).unwrap_or(cursor);
                    return Ok(delta);
                }
                Ok(_) => continue,
                Err(mpsc::TryRecvError::Disconnected) => return Err("chain reader stopped".into()),
                Err(mpsc::TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    }
}
impl Drop for ChainFeed {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

pub struct Validated {
    pub cache: Cache,
    pub ready: bool,
    pub reorg: bool,
    pub validated_ms: u64,
    pub arrivals: HashMap<Hash, u64>,
}
pub struct Follower {
    updates: mpsc::Receiver<std::result::Result<Validated, String>>,
    worker: tokio::task::JoinHandle<()>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
}
impl Follower {
    pub async fn finish(&mut self) {
        self.stopping.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = (&mut self.worker).await;
    }
    pub fn try_update(&self) -> std::result::Result<Option<Validated>, String> {
        match self.updates.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err("chain validator stopped; reconnecting".into()),
        }
    }
    pub fn new(mut cache: Cache, a: Artifact, invite: Invitation, node: Node, config: Config) -> Self {
        // Outboxes belong to the coordinator/writer. Publish only new events.
        cache.reorg_log_pending.clear();
        cache.transaction_log_pending.clear();
        cache.pending.clear();
        let (sender, updates) = mpsc::sync_channel(2);
        let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = stopping.clone();
        let worker = tokio::spawn(async move {
            let mut feed = ChainFeed::new(node.clone(), cache.tracker.cursor, config.poll_ms);
            'following: while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let result = cache.sync_follow(&a, &invite, &node, &config, &mut feed).await.map_err(|e| e.to_string());
                let mut packet = match result {
                    Ok((ready, reorg)) => Ok(Validated {
                        cache: cache.clone(),
                        ready,
                        reorg,
                        validated_ms: now_ms(),
                        arrivals: if ready { std::mem::take(&mut feed.arrivals) } else { HashMap::new() },
                    }),
                    Err(error) => Err(error),
                };
                let failed = packet.is_err();
                loop {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) { break 'following; }
                    match sender.try_send(packet) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Full(p)) => packet = p,
                        Err(mpsc::TrySendError::Disconnected(_)) => break 'following,
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                if failed {
                    break;
                }
                cache.reorg_log_pending.clear();
                cache.transaction_log_pending.clear();
            }
            feed.finish().await;
        });
        Self { updates, worker, stopping }
    }
}
impl Drop for Follower {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Serialize)]
pub struct Timing {
    pub phase: &'static str,
    pub observed_unix_ms: u64,
    pub transaction: Option<Hash>,
    pub elapsed_ms: Option<u64>,
    pub accepted_daa: Option<u64>,
    // RPC acceptance DAA is exact, but it is not a node wall-clock timestamp.
    pub node_acceptance_unix_ms: Option<u64>,
}
impl Timing {
    pub fn new(phase: &'static str, tx: Option<Hash>, elapsed_ms: Option<u64>) -> Self {
        Self { phase, observed_unix_ms: now_ms(), transaction: tx, elapsed_ms, accepted_daa: None, node_acceptance_unix_ms: None }
    }
}

enum WriteJob {
    Snapshot { sequence: u64, cache: Box<Cache>, durable: Option<mpsc::Sender<std::result::Result<(), String>>> },
    Timing(Timing),
}
pub struct Persisted {
    pub reorg_ids: Vec<String>,
    pub transaction_ids: Vec<String>,
    pub result: std::result::Result<(), String>,
}
/// One FIFO writer owns every gameplay cache write, including required draft
/// records. An old diagnostics snapshot cannot race a newer authorization.
pub struct Persistence {
    sender: Option<mpsc::Sender<WriteJob>>,
    background: Arc<Mutex<Option<WriteJob>>>,
    pub completed: mpsc::Receiver<Persisted>,
    worker: Option<std::thread::JoinHandle<()>>,
    sequence: u64,
}
impl Persistence {
    pub fn new(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        let (completed_tx, completed) = mpsc::channel();
        let background = Arc::new(Mutex::new(None));
        let background_jobs = background.clone();
        let worker = std::thread::spawn(move || {
            let mut reorg_log = audit::ReorgLog::open(&audit::log_path(&path)).map_err(|e| e.to_string());
            let mut transaction_log = audit::ReorgLog::open(&audit::transaction_log_path(&path)).map_err(|e| e.to_string());
            let mut timing_path = path.clone();
            timing_path.set_extension("timing.jsonl");
            let mut timing_log = std::fs::OpenOptions::new().create(true).append(true).open(timing_path).map_err(|e| e.to_string());
            let mut written = 0;
            loop {
                let job = match receiver.recv_timeout(Duration::from_millis(5)) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Recheck the FIFO while holding the background slot:
                        // an older required job may have arrived just as recv
                        // timed out, before a newer snapshot was coalesced.
                        let mut background = background_jobs.lock().unwrap();
                        match receiver.try_recv() {
                            Ok(job) => job,
                            Err(_) => match background.take() {
                                Some(job) => job,
                                None => continue,
                            },
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let Some(job) = background_jobs.lock().unwrap().take() else {
                            break;
                        };
                        job
                    }
                };
                match job {
                    WriteJob::Timing(record) => {
                        let result = timing_log.as_mut().map_err(|e| e.clone()).and_then(|log| {
                            serde_json::to_writer(&mut *log, &record).map_err(|e| e.to_string())?;
                            log.write_all(b"\n").map_err(|e| e.to_string())
                        });
                        if result.is_err() {
                            let _ = completed_tx.send(Persisted { reorg_ids: vec![], transaction_ids: vec![], result });
                        }
                    }
                    WriteJob::Snapshot { sequence, cache, durable } => {
                        let started = Instant::now();
                        // Coalesced background snapshots may be superseded by
                        // a required write. Never let those rewrite old state.
                        if sequence <= written && durable.is_none() {
                            continue;
                        }
                        let result = if sequence <= written {
                            Err("out-of-order cache write rejected".to_string())
                        } else {
                            super::save(&path, &cache).and_then(|_| super::reservations::persisted(&path, &cache)).map_err(|e| e.to_string())
                        };
                        if result.is_ok() {
                            written = sequence;
                        }
                        // Submission waits for THIS durable snapshot, not for
                        // diagnostic appends or any unrelated validation work.
                        if let Some(ack) = durable {
                            let _ = ack.send(result.clone());
                        }
                        let reorg_ids = cache.reorg_log_pending.iter().map(|e| e.id.clone()).collect();
                        let transaction_ids = cache.transaction_log_pending.iter().map(|e| e.id.clone()).collect();
                        let logged = result.and_then(|_| {
                            let reorg_log = reorg_log.as_mut().map_err(|e| e.clone())?;
                            let transaction_log = transaction_log.as_mut().map_err(|e| e.clone())?;
                            for event in &cache.reorg_log_pending {
                                reorg_log.append(event).map_err(|e| e.to_string())?;
                            }
                            for event in &cache.transaction_log_pending {
                                transaction_log.append_record(&event.id, event).map_err(|e| e.to_string())?;
                            }
                            let log = timing_log.as_mut().map_err(|e| e.clone())?;
                            let record = Timing::new("cache_persisted", None, Some(started.elapsed().as_millis() as u64));
                            serde_json::to_writer(&mut *log, &record).map_err(|e| e.to_string())?;
                            log.write_all(b"\n").map_err(|e| e.to_string())?;
                            Ok(())
                        });
                        let _ = completed_tx.send(Persisted { reorg_ids, transaction_ids, result: logged });
                    }
                }
            }
            let result = timing_log
                .as_mut()
                .map_err(|e| e.clone())
                .and_then(|log| log.flush().and_then(|_| log.sync_data()).map_err(|e| e.to_string()));
            if result.is_err() {
                let _ = completed_tx.send(Persisted { reorg_ids: vec![], transaction_ids: vec![], result });
            }
        });
        Self { sender: Some(sender), background, completed, worker: Some(worker), sequence: 0 }
    }
    pub fn timing(&self, record: Timing) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(WriteJob::Timing(record));
        }
    }
    pub fn snapshot(&mut self, cache: &Cache, required: bool) -> Result<Option<mpsc::Receiver<std::result::Result<(), String>>>> {
        let sender = self.sender.as_ref().ok_or("cache writer closed")?;
        self.sequence = self.sequence.checked_add(1).ok_or("cache write sequence overflow")?;
        let (durable, wait) = if required {
            let (tx, rx) = mpsc::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let job = WriteJob::Snapshot { sequence: self.sequence, cache: Box::new(cache.clone()), durable };
        if required {
            sender.send(job).map_err(|_| "cache writer stopped")?;
        } else {
            // One latest snapshot only. Unacknowledged outbox events remain in
            // the coordinator cache, hence survive replacement/coalescing.
            *self.background.lock().unwrap() = Some(job);
        }
        Ok(wait)
    }
    pub fn finish(&mut self) -> Result<()> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "cache writer panicked")?;
        }
        for event in self.completed.try_iter() {
            event.result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        }
        Ok(())
    }
}
impl Drop for Persistence {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
