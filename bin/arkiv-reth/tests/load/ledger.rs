//! One record per transaction, from plan to inclusion, and the violations
//! found along the way.

use std::collections::HashMap;

use alloy_primitives::{Address, B256};
use serde_json::{Value, json};

use crate::plan::{Body, PlannedTx, SINK};

#[derive(Clone, Debug)]
pub struct Inclusion {
    pub block: u64,
    pub index: usize,
    pub status: bool,
    pub gas_used: u64,
    pub effective_gas_price: u128,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub sender: usize,
    pub sender_address: Address,
    pub plan: PlannedTx,
    /// The nonce it was sent with; `None` until sent.
    pub nonce: Option<u64>,
    pub hash: Option<B256>,
    /// The chain height and the run's clock (ms) when the node accepted it.
    pub sent: Option<(u64, u64)>,
    /// Why the node refused it at submission.
    pub rejected: Option<String>,
    /// The run's clock when the watcher first saw it in the pool.
    pub seen_in_pool_ms: Option<u64>,
    pub included: Option<Inclusion>,
    /// Pool checks in a row that found it neither in the pool nor in a block.
    pub misses: u32,
    /// Recorded as sent, but never sent: the run's self-check.
    pub phantom: bool,
}

impl Record {
    pub fn is_outstanding(&self) -> bool {
        self.hash.is_some() && self.included.is_none()
    }

    /// Where the transaction ended up.
    fn outcome(&self) -> &'static str {
        match (&self.included, &self.rejected, &self.hash) {
            (Some(i), _, _) if i.status => "success",
            (Some(_), _, _) => "reverted",
            (None, Some(_), _) => "rejected_at_ingress",
            (None, None, Some(_)) => "outstanding",
            (None, None, None) => "not_sent",
        }
    }

    /// What the transaction asked for, op by op.
    fn ops_json(&self) -> Value {
        match &self.plan.body {
            Body::Execute(_) => self.plan.effects.iter().map(|e| e.to_json()).collect(),
            Body::Transfer { value } => json!([{
                "op": "value transfer",
                "to": format!("{SINK:#x}"),
                "value": value.to_string(),
            }]),
            Body::EntityNonce => json!([{
                "op": "entityNonce view",
                "owner": format!("{:#x}", self.sender_address),
            }]),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "sender": self.sender,
            "from": format!("{:#x}", self.sender_address),
            "nonce": self.nonce,
            "hash": self.hash.map(|h| format!("{h:#x}")),
            "label": self.plan.label,
            "ops": self.ops_json(),
            "expect": self.plan.expect.name(),
            "outcome": self.outcome(),
            "gasLimit": self.plan.gas,
            "sentAtBlock": self.sent.map(|(block, _)| block),
            "sentAtMs": self.sent.map(|(_, ms)| ms),
            "rejected": self.rejected,
            "seenInPoolAtMs": self.seen_in_pool_ms,
            "includedIn": self.included.as_ref().map(|i| json!({
                "block": i.block,
                "index": i.index,
                "status": if i.status { "success" } else { "reverted" },
                "gasUsed": i.gas_used,
                "effectiveGasPrice": i.effective_gas_price.to_string(),
                "feeWei": (u128::from(i.gas_used) * i.effective_gas_price).to_string(),
            })),
            "phantom": self.phantom,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Violation {
    /// `lost`, `duplicate`, `nonce_gap`, `unexpected_outcome`,
    /// `entity_mismatch`, `unclean_end`.
    pub kind: &'static str,
    pub block: Option<u64>,
    pub txs: Vec<B256>,
    pub keys: Vec<B256>,
    pub detail: String,
}

impl Violation {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "block": self.block,
            "txs": self.txs.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>(),
            "keys": self.keys.iter().map(|k| format!("{k:#x}")).collect::<Vec<_>>(),
            "detail": self.detail,
        })
    }
}

#[derive(Default)]
pub struct Ledger {
    pub records: Vec<Record>,
    by_hash: HashMap<B256, usize>,
}

impl Ledger {
    pub fn push(&mut self, record: Record) -> usize {
        self.records.push(record);
        self.records.len() - 1
    }

    /// Mark record `id` as accepted by the node under `hash`.
    pub fn sent(&mut self, id: usize, nonce: u64, hash: B256, block: u64, ms: u64) {
        let record = &mut self.records[id];
        record.nonce = Some(nonce);
        record.hash = Some(hash);
        record.sent = Some((block, ms));
        self.by_hash.insert(hash, id);
    }

    pub fn by_hash(&self, hash: &B256) -> Option<usize> {
        self.by_hash.get(hash).copied()
    }
}
