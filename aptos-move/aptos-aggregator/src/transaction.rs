// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::delta_change_set::{deserialize, DeltaChangeSet};
use anyhow::bail;
use aptos_state_view::StateView;
use aptos_types::{
    state_store::state_key::StateKey,
    transaction::{ChangeSet, CheckChangeSet, TransactionOutput},
    write_set::{TransactionWrite, WriteOp, WriteSet},
};
use std::{collections::btree_map, sync::Arc};

/// Helpful trait for e.g. extracting u128 value out of TransactionWrite that we know is
/// for aggregator (i.e. if we have seen a DeltaOp for the same access path).
pub struct AggregatorValue(u128);

impl AggregatorValue {
    /// Returns None if the write doesn't contain a value (i.e deletion), and panics if
    /// the value raw bytes can't be deserialized into an u128.
    pub fn from_write(write: &dyn TransactionWrite) -> Option<Self> {
        let v = write.extract_raw_bytes();
        v.map(|bytes| Self(deserialize(&bytes)))
    }

    pub fn into(self) -> u128 {
        self.0
    }
}

/// Extension of `ChangeSet` that also holds deltas.
pub struct ChangeSetExt {
    pub delta_change_set: DeltaChangeSet,
    pub change_set: ChangeSet,
    checker: Arc<dyn CheckChangeSet>,
}

impl ChangeSetExt {
    pub fn new(
        delta_change_set: DeltaChangeSet,
        change_set: ChangeSet,
        checker: Arc<dyn CheckChangeSet>,
    ) -> Self {
        ChangeSetExt {
            delta_change_set,
            change_set,
            checker,
        }
    }

    pub fn empty(checker: Arc<dyn CheckChangeSet>) -> Self {
        ChangeSetExt {
            delta_change_set: DeltaChangeSet::empty(),
            change_set: ChangeSet::empty(),
            checker,
        }
    }

    pub fn change_set(&self) -> &ChangeSet {
        &self.change_set
    }

    pub fn delta_change_set(&self) -> &DeltaChangeSet {
        &self.delta_change_set
    }

    pub fn write_set(&self) -> &WriteSet {
        self.change_set.write_set()
    }

    pub fn into_inner(self) -> (DeltaChangeSet, ChangeSet) {
        (self.delta_change_set, self.change_set)
    }

    pub fn squash_delta_change_set(self, other: DeltaChangeSet) -> anyhow::Result<Self> {
        use btree_map::Entry::*;
        use WriteOp::*;

        let checker = self.checker.clone();
        let (mut delta_set, change_set) = self.into_inner();
        let (write_set, events) = change_set.into_inner();
        let mut write_set = write_set.into_mut();

        let delta_ops = delta_set.as_inner_mut();
        let write_ops = write_set.as_inner_mut();

        for (key, mut op) in other.into_iter() {
            if let Some(r) = write_ops.get_mut(&key) {
                match r {
                    Creation(data)
                    | Modification(data)
                    | CreationWithMetadata { data, .. }
                    | ModificationWithMetadata { data, .. } => {
                        let val: u128 = bcs::from_bytes(data)?;
                        *data = bcs::to_bytes(&op.apply_to(val)?)?;
                    },
                    Deletion | DeletionWithMetadata { .. } => {
                        bail!("Failed to apply Aggregator delta -- value already deleted");
                    },
                }
            } else {
                match delta_ops.entry(key) {
                    Occupied(entry) => {
                        // In this case, we need to merge the new incoming `op` to the existing
                        // delta, ensuring the strict ordering.
                        op.merge_onto(*entry.get())?;
                        *entry.into_mut() = op;
                    },
                    Vacant(entry) => {
                        entry.insert(op);
                    },
                }
            }
        }

        Ok(Self {
            delta_change_set: delta_set,
            change_set: ChangeSet::new(write_set.freeze()?, events, checker.as_ref())?,
            checker,
        })
    }

    pub fn squash_change_set(self, other: ChangeSet) -> anyhow::Result<Self> {
        use btree_map::Entry::*;

        let checker = self.checker.clone();
        let (mut delta, change_set) = self.into_inner();
        let (write_set, mut events) = change_set.into_inner();
        let mut write_set = write_set.into_mut();
        let write_ops = write_set.as_inner_mut();

        let (other_write_set, other_events) = other.into_inner();

        for (key, op) in other_write_set.into_iter() {
            match write_ops.entry(key) {
                Occupied(mut entry) => {
                    if !WriteOp::squash(entry.get_mut(), op)? {
                        entry.remove();
                    }
                },
                Vacant(entry) => {
                    delta.remove(entry.key());
                    entry.insert(op);
                },
            }
        }

        events.extend(other_events);

        Ok(Self {
            delta_change_set: delta,
            change_set: ChangeSet::new(write_set.freeze()?, events, checker.as_ref())?,
            checker,
        })
    }

    pub fn squash(self, other: Self) -> anyhow::Result<Self> {
        let (delta_change_set, change_set) = other.into_inner();
        self.squash_change_set(change_set)?
            .squash_delta_change_set(delta_change_set)
    }
}

/// Extension of `TransactionOutput` that also holds `DeltaChangeSet`
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionOutputExt {
    delta_change_set: DeltaChangeSet,
    output: TransactionOutput,
}

impl TransactionOutputExt {
    pub fn new(delta_change_set: DeltaChangeSet, output: TransactionOutput) -> Self {
        TransactionOutputExt {
            delta_change_set,
            output,
        }
    }

    pub fn delta_change_set(&self) -> &DeltaChangeSet {
        &self.delta_change_set
    }

    pub fn txn_output(&self) -> &TransactionOutput {
        &self.output
    }

    // TODO: rename to unpack() and consider other into()'s in the crate.
    pub fn into(self) -> (DeltaChangeSet, TransactionOutput) {
        (self.delta_change_set, self.output)
    }

    /// Similar to `into()` but tries to apply delta changes as well.
    /// TODO: ideally, we may want to expose this function to VM instead. Since
    /// we do not care about rerunning the epilogue - it sufficies to have it
    /// here for now.
    pub fn into_transaction_output(self, state_view: &impl StateView) -> TransactionOutput {
        let (delta_change_set, txn_output) = self.into();

        // First, check if output of transaction should be discarded or delta
        // change set is empty. In both cases, we do not need to apply any
        // deltas and can return immediately.
        if txn_output.status().is_discarded() || delta_change_set.is_empty() {
            return txn_output;
        }

        // TODO: at this point we know that delta application failed
        // (and it should have occurred in user transaction in general).
        // We need to rerun the epilogue and charge gas. Currently, the use
        // case of an aggregator is for gas fees (which are computed in
        // the epilogue), and therefore this should never happen.
        // Also, it is worth mentioning that current VM error handling is
        // rather ugly and has a lot of legacy code. This makes proper error
        // handling quite challenging.
        delta_change_set
            .take_materialized(state_view)
            .map(|materialized_deltas| Self::merge_delta_writes(txn_output, materialized_deltas))
            .expect("Failed to apply aggregator delta outputs")
    }

    pub fn output_with_delta_writes(
        self,
        delta_writes: Vec<(StateKey, WriteOp)>,
    ) -> TransactionOutput {
        let (delta_change_set, txn_output) = self.into();

        // First, check if output of transaction should be discarded or delta
        // change set is empty. In both cases, we do not need to apply any
        // deltas and can return immediately.
        if txn_output.status().is_discarded() || delta_change_set.is_empty() {
            return txn_output;
        }

        // We should have a delta write for every delta in the output.
        assert_eq!(delta_change_set.len(), delta_writes.len());

        Self::merge_delta_writes(txn_output, delta_writes)
    }

    fn merge_delta_writes(
        output: TransactionOutput,
        delta_writes: Vec<(StateKey, WriteOp)>,
    ) -> TransactionOutput {
        let (write_set, events, gas_used, status) = output.unpack();
        let mut write_set_mut = write_set.into_mut();

        // Add the delta writes to the write set of the transaction.
        delta_writes
            .into_iter()
            .for_each(|item| write_set_mut.insert(item));

        TransactionOutput::new(write_set_mut.freeze().unwrap(), events, gas_used, status)
    }
}

impl From<TransactionOutput> for TransactionOutputExt {
    fn from(output: TransactionOutput) -> Self {
        TransactionOutputExt {
            delta_change_set: DeltaChangeSet::empty(),
            output,
        }
    }
}
