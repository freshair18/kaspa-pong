//! Process-wide atomic input claims, backed by independently durable journals.
//! No lock is held while fetching, signing, saving, or submitting a transaction.
use crate::engine::{Result, pubkey};
use kaspa_consensus_core::tx::TransactionOutpoint;
use secp256k1::Keypair;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Debug)]
pub struct InputReserved;

impl std::fmt::Display for InputReserved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("input reserved by another preparation or pending transaction")
    }
}
impl std::error::Error for InputReserved {}

#[derive(Default)]
struct Book {
    next: u64,
    journals: HashMap<PathBuf, Vec<TransactionOutpoint>>,
    drafts: HashMap<u64, Vec<TransactionOutpoint>>,
}
impl Book {
    fn occupied(&self, input: &TransactionOutpoint) -> bool {
        self.journals.values().chain(self.drafts.values()).any(|v| v.contains(input))
    }
    fn claim(&mut self, inputs: Vec<TransactionOutpoint>) -> Result<u64> {
        if inputs.iter().any(|i| self.occupied(i)) {
            return Err(InputReserved.into());
        }
        self.next = self.next.checked_add(1).ok_or("reservation counter overflow")?;
        self.drafts.insert(self.next, inputs);
        Ok(self.next)
    }
}
fn book() -> &'static Mutex<Book> {
    static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
    BOOK.get_or_init(Default::default)
}
fn path(path: &Path) -> Result<PathBuf> {
    Ok(path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?
        .join(path.file_name().ok_or("journal path has no filename")?))
}
/// Held across preparation. Dropping an unexposed draft releases only its claims.
#[derive(Debug)]
pub struct Claim {
    id: u64,
}
impl Claim {
    pub fn inputs(inputs: Vec<TransactionOutpoint>) -> Result<Self> {
        Ok(Self { id: book().lock().map_err(|_| "reservation lock poisoned")?.claim(inputs)? })
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut b) = book().lock() {
            b.drafts.remove(&self.id);
        }
    }
}
pub fn reserved() -> Result<Vec<TransactionOutpoint>> {
    let b = book().lock().map_err(|_| "reservation lock poisoned")?;
    Ok(b.journals.values().chain(b.drafts.values()).flatten().copied().collect())
}
pub fn restore(journal: &Path, inputs: Vec<TransactionOutpoint>) -> Result<()> {
    let key = path(journal)?;
    book().lock().map_err(|_| "reservation lock poisoned")?.journals.insert(key, inputs);
    Ok(())
}
pub fn signer_locks(network: &str, player: &Keypair, payer: &Keypair) -> Result<Vec<std::fs::File>> {
    let mut keys = vec![pubkey(player), pubkey(payer)];
    keys.sort();
    keys.dedup();
    keys.iter()
        .map(|key| {
            super::lock(&PathBuf::from("games").join(format!("signer-{network}-{}.json", secp256k1::XOnlyPublicKey::from_slice(key)?)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaspa_consensus_core::Hash;
    fn input(n: u8) -> TransactionOutpoint {
        TransactionOutpoint::new(Hash::from_bytes([n; 32]), 0)
    }
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
    fn competing_threads_cannot_select_the_same_fee_input() {
        let book = std::sync::Arc::new(Mutex::new(Book::default()));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let book = book.clone();
                std::thread::spawn(move || book.lock().unwrap().claim(vec![input(9)]).is_ok())
            })
            .collect();
        assert_eq!(threads.into_iter().map(|t| t.join().unwrap()).filter(|won| *won).count(), 1);
    }
}
