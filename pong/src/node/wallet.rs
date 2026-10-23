//! Local single-key wallets. No RPC, key registration or mining is involved.
use super::super::engine::{Result, Funding, p2pk, pubkey};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::network::{NetworkId, NetworkType};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};

// Intentionally no Debug implementation: this record contains private keys.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWallet {
    version: u32,
    player_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fee_key: Option<String>,
}

pub struct Wallet {
    pub player: Keypair,
    pub payer: Keypair,
}
/// Payouts arrive at the game key, including wallets with a separate fee key.
/// Spend either owned address directly; never transfer or migrate balances.
pub fn funding_key<'a>(funding: &Funding, player: &'a Keypair, payer: &'a Keypair) -> Result<&'a Keypair> {
    if funding.utxo.script_public_key == p2pk(&pubkey(payer)) { Ok(payer) }
    else if funding.utxo.script_public_key == p2pk(&pubkey(player)) { Ok(player) }
    else { Err("funding coin does not belong to this wallet".into()) }
}
pub async fn funding_candidates(node: &super::rpc::Node, player: &Keypair, payer: &Keypair,
    minimum: u64, maturity: u64, reserved: &[kaspa_consensus_core::tx::TransactionOutpoint]) -> Result<Vec<Funding>> {
    let mut reservations = super::hosted::reserved()?;
    reservations.extend_from_slice(reserved);
    let reserved = reservations.as_slice();
    let mut coins = node.funding_candidates(&pubkey(payer), minimum, maturity, reserved).await?;
    if pubkey(player) != pubkey(payer) {
        coins.extend(node.funding_candidates(&pubkey(player), minimum, maturity, reserved).await?);
    }
    Ok(coins)
}

/// Gameplay needs one usable coin, not launch's exhaustive mass comparison.
/// An optional player-address lookup must not delay or invalidate payer success.
pub async fn fee_funding(node: &super::rpc::Node, player: &Keypair, payer: &Keypair,
    minimum: u64, maturity: u64, reserved: &[kaspa_consensus_core::tx::TransactionOutpoint]) -> Result<Funding> {
    let mut reservations = super::hosted::reserved()?;
    reservations.extend_from_slice(reserved);
    let reserved = reservations.as_slice();
    let primary = node.funding_candidates(&pubkey(payer), minimum, maturity, reserved).await?;
    first_fee_coin(primary, pubkey(player) != pubkey(payer), || async {
        node.funding_candidates(&pubkey(player), minimum, maturity, reserved).await
    }).await
}

async fn first_fee_coin<F: std::future::Future<Output = Result<Vec<Funding>>>>(
    primary: Vec<Funding>, has_alternate: bool, alternate: impl FnOnce() -> F,
) -> Result<Funding> {
    if let Some(coin) = primary.into_iter().next() { return Ok(coin); }
    if has_alternate {
        if let Some(coin) = alternate().await?.into_iter().next() { return Ok(coin); }
    }
    Err("no mature owned fee coin; fund the player or fee address".into())
}
impl Wallet {
    pub fn generate(separate_fee_key: bool) -> Result<Self> {
        fn random_key() -> Result<Keypair> {
            loop {
                let mut bytes = [0u8; 32];
                getrandom::getrandom(&mut bytes).map_err(|_| "operating system randomness is unavailable")?;
                if let Ok(secret) = SecretKey::from_slice(&bytes) {
                    return Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret));
                }
            }
        }
        let player = random_key()?;
        let payer = if separate_fee_key { random_key()? } else { player };
        Ok(Self { player, payer })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let stored: StoredWallet = serde_json::from_slice(&std::fs::read(path)?).map_err(|_| "invalid wallet file")?;
        if stored.version != 1 {
            return Err("unsupported wallet file version".into());
        }
        fn parse(text: &str) -> Result<Keypair> {
            let secret: SecretKey = text.parse().map_err(|_| "invalid private key in wallet file")?;
            Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret))
        }
        let player = parse(&stored.player_key)?;
        let payer = stored.fee_key.as_deref().map(parse).transpose()?.unwrap_or(player);
        Ok(Self { player, payer })
    }

    pub fn save_new(&self, path: &Path) -> Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let stored = StoredWallet {
            version: 1,
            player_key: self.player.secret_key().display_secret().to_string(),
            fee_key: (self.player != self.payer).then(|| self.payer.secret_key().display_secret().to_string()),
        };
        // NamedTempFile is created with owner-only permissions on Unix. Persist
        // without replacement so an existing funded key can never be overwritten.
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&serde_json::to_vec_pretty(&stored)?)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                "wallet already exists; reuse it with --wallet FILE address, or choose a different --out FILE".into()
            } else {
                Box::<dyn std::error::Error>::from(e.error)
            }
        })?;
        Ok(())
    }
}

pub fn public_identity(network: &str, player: &Keypair, payer: &Keypair) -> Result<serde_json::Value> {
    let network: NetworkId = network.parse()?;
    let prefix = match network.network_type {
        NetworkType::Mainnet => Prefix::Mainnet,
        NetworkType::Testnet => Prefix::Testnet,
        NetworkType::Devnet => Prefix::Devnet,
        NetworkType::Simnet => Prefix::Simnet,
    };
    let player_public = player.x_only_public_key().0;
    let payer_public = payer.x_only_public_key().0;
    Ok(serde_json::json!({
        "network": network.to_string(),
        "player": player_public.to_string(),
        "player_address": Address::new(prefix, Version::PubKey, &player_public.serialize()).to_string(),
        "fee_address": Address::new(prefix, Version::PubKey, &payer_public.serialize()).to_string(),
    }))
}

#[cfg(test)]
mod fee_funding_tests {
    use super::*;
    use kaspa_consensus_core::{Hash, tx::{TransactionOutpoint, UtxoEntry}};
    fn coin() -> Funding {
        Funding { outpoint: TransactionOutpoint::new(Hash::from_bytes([1; 32]), 0),
            utxo: UtxoEntry::new(100_000_000, p2pk(&[2; 32]), 1, false, None) }
    }
    #[tokio::test]
    async fn usable_payer_never_queries_optional_player_even_if_it_would_fail() {
        let chosen = first_fee_coin(vec![coin()], true, || {
            panic!("optional player lookup must not run");
            #[allow(unreachable_code)]
            std::future::ready(Err("optional RPC failure".into()))
        }).await.unwrap();
        assert_eq!(chosen.outpoint, coin().outpoint);
    }
    #[tokio::test]
    async fn player_fallback_runs_only_when_needed_and_errors_remain_visible() {
        let chosen = first_fee_coin(vec![], true, || std::future::ready(Ok(vec![coin()]))).await.unwrap();
        assert_eq!(chosen.outpoint, coin().outpoint);
        let error = first_fee_coin(vec![], true, || std::future::ready(Err("player RPC failed".into()))).await.unwrap_err();
        assert!(error.to_string().contains("player RPC failed"));
        assert!(first_fee_coin(vec![], true, || std::future::ready(Ok(vec![]))).await.is_err());
    }
    #[tokio::test]
    async fn equal_keys_do_not_repeat_the_empty_address_lookup() {
        assert!(first_fee_coin(vec![], false, || {
            panic!("same address must not be queried twice");
            #[allow(unreachable_code)]
            std::future::ready(Ok(vec![]))
        }).await.is_err());
    }
}
