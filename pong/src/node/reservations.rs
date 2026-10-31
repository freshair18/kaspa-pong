//! Process-wide atomic input claims, backed by independently durable journals.
//! No lock is held while fetching, signing, saving, or submitting a transaction.
use crate::engine::{Funding, Result};
use kaspa_consensus_core::{Hash, tx::TransactionOutpoint};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::{Mutex, OnceLock}};

#[derive(Default)]
struct Book {
    next: u64,
    journals: HashMap<PathBuf, Vec<TransactionOutpoint>>,
    drafts: HashMap<u64, Vec<TransactionOutpoint>>,
    promoted: HashMap<(PathBuf, Hash), Vec<TransactionOutpoint>>,
}
impl Book {
    fn persisted(&mut self, key: PathBuf, inputs: Vec<TransactionOutpoint>, recorded: &[Hash]) {
        self.journals.insert(key.clone(), inputs);
        self.promoted.retain(|(owner, id), _| *owner != key || !recorded.contains(id));
    }
    fn occupied(&self, input: &TransactionOutpoint) -> bool {
        self.journals.values().chain(self.drafts.values()).chain(self.promoted.values()).any(|v| v.contains(input))
    }
    fn claim(&mut self, inputs: Vec<TransactionOutpoint>) -> Result<u64> {
        if inputs.iter().any(|i| self.occupied(i)) { return Err("input reserved by another preparation or pending transaction".into()); }
        self.next = self.next.checked_add(1).ok_or("reservation counter overflow")?;
        self.drafts.insert(self.next, inputs);
        Ok(self.next)
    }
}
fn book() -> &'static Mutex<Book> { static BOOK: OnceLock<Mutex<Book>> = OnceLock::new(); BOOK.get_or_init(Default::default) }
fn path(path: &Path) -> Result<PathBuf> {
    Ok(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).canonicalize()?
        .join(path.file_name().ok_or("journal path has no filename")?))
}
/// Held across preparation. Dropping an unexposed draft releases only its claims.
#[derive(Debug)]
pub struct Claim { id: u64 }
impl Claim {
    pub fn inputs(inputs: Vec<TransactionOutpoint>) -> Result<Self> {
        Ok(Self { id: book().lock().map_err(|_| "reservation lock poisoned")?.claim(inputs)? })
    }
    /// Retain just the actual transaction inputs after exhaustive coin selection.
    pub fn restrict(&mut self, inputs: Vec<TransactionOutpoint>) -> Result<()> {
        let mut b = book().lock().map_err(|_| "reservation lock poisoned")?;
        let old = b.drafts.get(&self.id).ok_or("missing preparation claim")?;
        if inputs.iter().any(|i| !old.contains(i) && b.occupied(i)) { return Err("transaction input is already reserved".into()); }
        b.drafts.insert(self.id, inputs);
        Ok(())
    }
    /// Keep authorization reserved even if its required save fails. Older disk
    /// snapshots cannot release newer claims; only a snapshot containing this
    /// authorization (or its durable withdrawal) can replace it.
    pub fn promote(self, journal: &Path, id: Hash) -> Result<()> {
        let key = path(journal)?;
        let mut b = book().lock().map_err(|_| "reservation lock poisoned")?;
        let inputs = b.drafts.remove(&self.id).ok_or("missing preparation claim")?;
        b.promoted.insert((key, id), inputs);
        Ok(())
    }
}
impl Drop for Claim {
    fn drop(&mut self) { if let Ok(mut b) = book().lock() { b.drafts.remove(&self.id); } }
}
pub fn reserved() -> Result<Vec<TransactionOutpoint>> {
    let b = book().lock().map_err(|_| "reservation lock poisoned")?;
    Ok(b.journals.values().chain(b.drafts.values()).chain(b.promoted.values()).flatten().copied().collect())
}
/// Selection and claim happen under the SAME lock, against the latest claims.
pub fn available(coins: Vec<Funding>, all: bool) -> Result<(Vec<Funding>, Claim)> {
    let mut b = book().lock().map_err(|_| "reservation lock poisoned")?;
    let mut chosen = Vec::new();
    for coin in coins {
        if !b.occupied(&coin.outpoint) && !chosen.iter().any(|f: &Funding| f.outpoint == coin.outpoint) {
            chosen.push(coin);
            if !all { break; }
        }
    }
    if chosen.is_empty() { return Err("insufficient AVAILABLE funds: mature coins are missing or reserved by another game/refund; fund this wallet with another usable coin".into()); }
    let id = b.claim(chosen.iter().map(|f| f.outpoint).collect())?;
    Ok((chosen, Claim { id }))
}
pub fn restore(journal: &Path, inputs: Vec<TransactionOutpoint>) -> Result<()> {
    let key = path(journal)?;
    book().lock().map_err(|_| "reservation lock poisoned")?.journals.insert(key, inputs);
    Ok(())
}
/// Transfer a confirmed journal's reserved output to the next setup atomically.
pub fn claim_journal_output(journal: &Path, output: TransactionOutpoint) -> Result<Claim> {
    let owner = path(journal)?;
    let mut b = book().lock().map_err(|_| "reservation lock poisoned")?;
    let inputs = b.journals.remove(&owner).ok_or("wallet preparation reservation missing")?;
    if !inputs.contains(&output) { b.journals.insert(owner, inputs); return Err("wallet preparation output is not reserved".into()); }
    match b.claim(vec![output]) {
        Ok(id) => Ok(Claim { id }),
        Err(error) => { b.journals.insert(owner, inputs); Err(error) }
    }
}
pub fn persisted(journal: &Path, cache: &super::client::Cache) -> Result<()> {
    let key = path(journal)?;
    let mut b = book().lock().map_err(|_| "reservation lock poisoned")?;
    let inputs = cache.pending.iter().filter(|p| !cache.withdrawals.contains(&p.transaction.id()))
        .flat_map(|p| p.transaction.inputs.iter().map(|i| i.previous_outpoint)).collect();
    let recorded = cache.pending.iter().map(|p| p.transaction.id()).chain(cache.withdrawals.iter().copied()).collect::<Vec<_>>();
    b.persisted(key, inputs, &recorded);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(n: u8) -> TransactionOutpoint { TransactionOutpoint::new(Hash::from_bytes([n;32]), 0) }
    #[test]
    fn atomic_claims_exclude_other_workers_but_not_other_coins_or_opponents() {
        let mut b = Book::default();
        let first = b.claim(vec![input(1)]).unwrap();
        assert!(b.claim(vec![input(1)]).is_err());
        assert!(b.claim(vec![input(2)]).is_ok(), "no wallet/opponent-wide recovery gate");
        b.drafts.remove(&first);
        assert!(b.claim(vec![input(1)]).is_ok());
    }
    #[test]
    fn older_saved_snapshot_cannot_release_newer_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("state.json");
        // Exercise the journal/promoted separation without global fixture state.
        let mut b = Book::default();
        let owner = path(&journal).unwrap();
        b.promoted.insert((owner.clone(), Hash::from_bytes([2;32])), vec![input(2)]);
        b.persisted(owner.clone(), vec![input(1)], &[]);
        assert!(b.occupied(&input(1)) && b.occupied(&input(2)));
        b.persisted(owner.clone(), vec![input(1), input(2)], &[Hash::from_bytes([2;32])]);
        assert!(b.promoted.is_empty() && b.occupied(&input(2)));
        b.persisted(owner, vec![], &[]);
        assert!(!b.occupied(&input(2)));
    }
    #[test]
    fn competing_threads_cannot_select_the_same_fee_input() {
        let book = std::sync::Arc::new(Mutex::new(Book::default()));
        let threads: Vec<_> = (0..8).map(|_| {
            let book = book.clone();
            std::thread::spawn(move || book.lock().unwrap().claim(vec![input(9)]).is_ok())
        }).collect();
        assert_eq!(threads.into_iter().map(|t| t.join().unwrap()).filter(|won| *won).count(), 1);
    }
}
