//! Accepted-chain interpretation of reusable games. Transaction payloads locate
//! invitations; VM validation and accepted source UTXOs authorize transitions.
use super::tracker::{AcceptedTx, Stage, call, is_entry, script_int};
use crate::{engine::*, reusable::*};
use argent_runtime::{Artifact, TxBuilder};
use kaspa_consensus_core::{Hash, tx::Transaction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QueueStage {
    Invitation {lobby:Cell<Offer>,guest:Set},
    Lobby { lobby: Cell<Offer>, sets: [Set; 2] },
    Live(Match),
    Collecting { result: Cell<ResultState>, sets: [Set; 2] },
    Finished { result: Finish, sets: [Set; 2] },
    Cancelled(Set),
}
fn find<T: Clone>(b: &TxBuilder<'_>, tx: &Transaction, daa: u64, actor: &str, state: T, encoded: State, id: Hash) -> Result<Cell<T>> {
    let script = b.covenant_utxo(actor, encoded, 1, daa, false, Some(id))?.script_public_key;
    let matches = tx
        .outputs
        .iter()
        .enumerate()
        .filter(|(_, o)| o.script_public_key == script && o.covenant.is_some_and(|c| c.covenant_id == id))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!("missing or ambiguous {actor} successor").into());
    }
    Ok(accepted_cell(tx, matches[0].0, daa, state))
}
fn find_set(b: &TxBuilder<'_>, tx: &Transaction, daa: u64, id: Hash, states: [Lane; 3]) -> Result<Set> {
    let lanes = states
        .iter()
        .map(|s| find(b, tx, daa, s.actor(), s.clone(), s.state(), id))
        .collect::<Result<Vec<_>>>()?
        .try_into()
        .map_err(|_| "wrong queue count")?;
    let set = Set { lanes };
    set.validate(b)?;
    Ok(set)
}
impl QueueStage {
    pub fn sets(&self) -> Vec<&Set> {
        match self {
            Self::Invitation{guest,..}=>vec![guest],
            Self::Lobby { sets, .. } | Self::Collecting { sets, .. } | Self::Finished { sets, .. } => sets.iter().collect(),
            Self::Live(m) => m.sets.iter().collect(),
            Self::Cancelled(s) => vec![s],
        }
    }
    pub fn cells(&self) -> Vec<(kaspa_consensus_core::tx::TransactionOutpoint, kaspa_consensus_core::tx::UtxoEntry)> {
        if matches!(self, Self::Finished { .. } | Self::Cancelled(_)) {
            return vec![];
        }
        let mut cells = self.sets().iter().flat_map(|s| s.lanes.iter().map(|c| (c.outpoint, c.utxo.clone()))).collect::<Vec<_>>();
        match self {
            Self::Lobby { lobby, .. } | Self::Invitation{lobby,..} => cells.push((lobby.outpoint, lobby.utxo.clone())),
            Self::Live(m) => cells.push((m.court.outpoint, m.court.utxo.clone())),
            Self::Collecting { result, .. } => cells.push((result.outpoint, result.utxo.clone())),
            _ => {}
        }
        cells
    }
    pub fn display(&self) -> Stage {
        match self {
            Self::Lobby { lobby, .. } | Self::Invitation{lobby,..} => {
                Stage::Lobby(Cell { outpoint: lobby.outpoint, utxo: lobby.utxo.clone(), state: lobby.state.lobby.clone() })
            }
            Self::Live(m) => Stage::Live(m.snapshot()),
            Self::Collecting { result, sets } => Stage::Collecting(Collecting {
                collector: Cell { outpoint: result.outpoint, utxo: result.utxo.clone(), state: result.state.collection.clone() },
                players: std::array::from_fn(|i| {
                    let c = &sets[i % 2].lanes[i / 2];
                    Cell { outpoint: c.outpoint, utxo: c.utxo.clone(), state: c.state.queue.clone() }
                }),
            }),
            Self::Finished { result, .. } => Stage::Reclaimed(result.clone()),
            Self::Cancelled(_) => Stage::Cancelled,
        }
    }
    pub fn apply(&mut self, a: &Artifact, b: &TxBuilder<'_>, accepted: &AcceptedTx, daa: u64) -> Result<bool> {
        let tx = &accepted.tx;
        let spent = self
            .cells()
            .into_iter()
            .filter_map(|(out, u)| tx.inputs.iter().position(|i| i.previous_outpoint == out).map(|i| (i, u)))
            .collect::<Vec<_>>();
        if spent.is_empty() {
            return Ok(false);
        }
        if accepted.inputs.len() != tx.inputs.len() {
            return Err("accepted queue spend lacks source UTXOs".into());
        }
        for (i, u) in &spent {
            if !super::queue_book::same_source(u, &accepted.inputs[*i]) {
                return Err("accepted source differs from saved queue".into());
            }
        }
        for i in 0..tx.inputs.len() {
            argent_runtime::execute_input_with_covenants(tx, accepted.inputs.clone(), i)?;
        }
        if storage_mass(tx, accepted.inputs.clone())? != tx.storage_mass() {
            return Err("accepted spend has invalid storage commitment".into());
        }
        // An identical outpoint can be reaccepted at a different DAA after a
        // reorg. The canonical Full response and successful VM execution prove
        // the new birth score; commands must then use that score.
        let refresh = |out: kaspa_consensus_core::tx::TransactionOutpoint, u: &mut kaspa_consensus_core::tx::UtxoEntry| {
            if let Some(i) = tx.inputs.iter().position(|i| i.previous_outpoint == out) {
                *u = accepted.inputs[i].clone();
            }
        };
        match self {
            Self::Invitation{lobby,guest}=>{refresh(lobby.outpoint,&mut lobby.utxo);for c in &mut guest.lanes{refresh(c.outpoint,&mut c.utxo);}},
            Self::Lobby { lobby, sets } => {
                refresh(lobby.outpoint, &mut lobby.utxo);
                for set in sets {
                    for c in &mut set.lanes {
                        refresh(c.outpoint, &mut c.utxo);
                    }
                }
            }
            Self::Live(m) => {
                refresh(m.court.outpoint, &mut m.court.utxo);
                for set in &mut m.sets {
                    for c in &mut set.lanes {
                        refresh(c.outpoint, &mut c.utxo);
                    }
                }
            }
            Self::Collecting { result, sets } => {
                refresh(result.outpoint, &mut result.utxo);
                for set in sets {
                    for c in &mut set.lanes {
                        refresh(c.outpoint, &mut c.utxo);
                    }
                }
            }
            _ => {}
        }
        let index = |out| tx.inputs.iter().position(|i| i.previous_outpoint == out);
        match self.clone() {
            Self::Invitation{lobby,mut guest}=>{
                let Some(i)=index(lobby.outpoint) else{update_commands(a,b,tx,daa,std::slice::from_mut(&mut guest))?;*self=Self::Invitation{lobby,guest};return Ok(true);};
                let args=call(b,a,"Lobby",lobby.state.state(),&tx.inputs[i].signature_script)?;
                if is_entry(a,"Lobby","start",&args)?{
                    let host=super::queue_codes::started_host(b,accepted,lobby.utxo.covenant_id.ok_or("missing game")?,lobby.state.lobby.stake_per_player as u64)?;
                    let mut expanded=Self::Lobby{lobby,sets:[guest,host]};expanded.apply(a,b,accepted,daa)?;*self=expanded;
                }else if is_entry(a,"Lobby","cancel",&args)?{
                    let game=lobby.utxo.covenant_id.ok_or("missing game")?;
                    let states=std::array::from_fn(|i|guest.lanes[i].state.reset(game,guest.generation()+1,0,0,false));
                    *self=Self::Cancelled(find_set(b,tx,daa,guest.id()?,states)?);
                }else{return Err("unknown invitation transition".into());}
            },
            Self::Lobby { lobby, sets } => {
                let Some(i) = index(lobby.outpoint) else {
                    let mut sets = sets;
                    update_commands(a, b, tx, daa, &mut sets)?;
                    *self = Self::Lobby { lobby, sets };
                    return Ok(true);
                };
                let args = call(b, a, "Lobby", lobby.state.state(), &tx.inputs[i].signature_script)?;
                let game = lobby.utxo.covenant_id.ok_or("missing game")?;
                if is_entry(a, "Lobby", "start", &args)? {
                    let host=super::queue_codes::started_host(b,accepted,lobby.utxo.covenant_id.ok_or("missing game")?,lobby.state.lobby.stake_per_player as u64)?;
                    if host.lanes[0].state.queue.owner!=lobby.state.lobby.right || host.id()?==sets[0].id()? {return Err("start uses another host or the guest's queue identity".into());}
                    let mut w = lobby.state.opening();w.pools[1]=host.id()?;w.generations[1]=host.generation();
                    let sets=[sets[0].clone(),host];
                    let wager = w.world.stake_per_player as u64;
                    let states: [[_; 3]; 2] = std::array::from_fn(|side| {
                        std::array::from_fn(|lane| {
                            sets[side].lanes[lane].state.reset(
                                game,
                                sets[side].generation(),
                                side,
                                part(wager - wager / 2, lane),
                                true,
                            )
                        })
                    });
                    let court = find(b, tx, daa, "Court", w.clone(), w.state(), game)?;
                    let sets = [sets[0].clone(), find_set(b, tx, daa, sets[1].id()?, states[1].clone())?];
                    *self = Self::Live(Match { court, sets });
                } else if is_entry(a, "Lobby", "cancel", &args)? {
                    let states =
                        std::array::from_fn(|lane| sets[0].lanes[lane].state.reset(game, sets[0].generation() + 1, 0, 0, false));
                    *self = Self::Cancelled(find_set(b, tx, daa, sets[0].id()?, states)?);
                } else {
                    return Err("unknown lobby transition".into());
                }
            }
            Self::Live(mut m) => {
                if let Some(i) = index(m.court.outpoint) {
                    let args = call(b, a, "Court", m.court.state.state(), &tx.inputs[i].signature_script)?;
                    if is_entry(a, "Court", "resolve", &args)? {
                        let side = usize::try_from(script_int(args.first().ok_or("missing side")?)?)?;
                        let until = script_int(args.get(1).ok_or("missing deadline")?)?;
                        let (game, states) = m.checkpoint_state(side, until)?;
                        m.sets[side] = find_set(b, tx, daa, m.sets[side].id()?, states)?;
                        let id = m.court.utxo.covenant_id.ok_or("missing game")?;
                        if game.world.ended() {
                            let r = game.result_state(game.world.result());
                            let result = find(b, tx, daa, "Collector", r.clone(), r.state(), id)?;
                            *self = Self::Collecting { result, sets: m.sets };
                        } else {
                            m.court = find(b, tx, daa, "Court", game.clone(), game.state(), id)?;
                            *self = Self::Live(m);
                        }
                    } else if is_entry(a, "Court", "timeout", &args)? {
                        let r = m.court.state.result_state(response_timeout(&m.snapshot())?);
                        let result =
                            find(b, tx, daa, "Collector", r.clone(), r.state(), m.court.utxo.covenant_id.ok_or("missing game")?)?;
                        *self = Self::Collecting { result, sets: m.sets };
                    } else {
                        return Err("unknown court transition".into());
                    }
                } else {
                    update_commands(a, b, tx, daa, &mut m.sets)?;
                    *self = Self::Live(m);
                }
            }
            Self::Collecting { result, mut sets } => {
                if let Some(i) = index(result.outpoint) {
                    let args = call(b, a, "Collector", result.state.state(), &tx.inputs[i].signature_script)?;
                    if !is_entry(a, "Collector", "collect", &args)? {
                        return Err("unknown collection".into());
                    }
                    let game = result.utxo.covenant_id.ok_or("missing game")?;
                    for side in 0..2 {
                        let states = std::array::from_fn(|lane| {
                            sets[side].lanes[lane].state.reset(game, sets[side].generation() + 1, side, 0, false)
                        });
                        sets[side] = find_set(b, tx, daa, sets[side].id()?, states)?;
                    }
                    *self = Self::Finished { result: result.state.collection.finish, sets };
                } else {
                    update_commands(a, b, tx, daa, &mut sets)?;
                    *self = Self::Collecting { result, sets };
                }
            }
            Self::Finished { .. } | Self::Cancelled(_) => return Err("unexpected spend of a completed game's queues".into()),
        }
        Ok(true)
    }
}
fn update_commands(a: &Artifact, b: &TxBuilder<'_>, tx: &Transaction, daa: u64, sets: &mut [Set]) -> Result<()> {
    for set in sets {
        for cell in &mut set.lanes {
            let Some(i) = tx.inputs.iter().position(|i| i.previous_outpoint == cell.outpoint) else {
                continue;
            };
            let actor = cell.state.actor();
            let args = call(b, a, actor, cell.state.state(), &tx.inputs[i].signature_script)?;
            if !is_entry(a, actor, "command", &args)? {
                return Err("queue spent without movement or game transition".into());
            }
            let mut next = cell.state.clone();
            next.queue = next.queue.push(script_int(args.first().ok_or("missing direction")?)?, cell.utxo.block_daa_score)?;
            *cell = find(b, tx, daa, actor, next.clone(), next.state(), cell.utxo.covenant_id.ok_or("missing queue id")?)?;
        }
    }
    Ok(())
}
