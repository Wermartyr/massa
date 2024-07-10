use std::{collections::BTreeMap, ops::Bound};

use massa_ledger_exports::{SetOrKeepDeserializer, SetOrKeepSerializer};
use massa_models::{
    amount::Amount,
    deferred_call_id::DeferredCallId,
    slot::{Slot, SlotDeserializer, SlotSerializer},
};
use massa_serialization::{
    Deserializer, SerializeError, Serializer, U128VarIntDeserializer, U128VarIntSerializer,
    U64VarIntDeserializer, U64VarIntSerializer,
};
use nom::{
    error::{context, ContextError, ParseError},
    multi::length_count,
    IResult,
};
use serde::{Deserialize, Serialize};

use crate::{
    slot_changes::{
        DeferredRegistrySlotChanges, DeferredRegistrySlotChangesDeserializer,
        DeferredRegistrySlotChangesSerializer,
    },
    DeferredCall, DeferredRegistryGasChange,
};
use std::ops::Bound::Included;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct DeferredRegistryChanges {
    pub slots: BTreeMap<Slot, DeferredRegistrySlotChanges>,
    pub total_gas: DeferredRegistryGasChange<u128>,
}

impl DeferredRegistryChanges {
    pub fn merge(&mut self, other: DeferredRegistryChanges) {
        for (slot, changes) in other.slots {
            match self.slots.entry(slot) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().merge(changes);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(changes);
                }
            }
        }
        match other.total_gas {
            DeferredRegistryGasChange::Set(v) => self.total_gas = DeferredRegistryGasChange::Set(v),
            DeferredRegistryGasChange::Keep => {}
        }
    }

    pub fn delete_call(&mut self, target_slot: Slot, id: &DeferredCallId) {
        self.slots.entry(target_slot).or_default().delete_call(id)
    }

    pub fn set_call(&mut self, id: DeferredCallId, call: DeferredCall) {
        self.slots
            .entry(call.target_slot.clone())
            .or_default()
            .set_call(id, call);
    }

    pub fn get_call(&self, target_slot: &Slot, id: &DeferredCallId) -> Option<&DeferredCall> {
        self.slots
            .get(target_slot)
            .and_then(|slot_changes| slot_changes.get_call(id))
    }

    pub fn get_slot_gas(&self, target_slot: &Slot) -> Option<u64> {
        self.slots
            .get(target_slot)
            .and_then(|slot_changes| slot_changes.get_gas())
    }

    pub fn set_slot_gas(&mut self, target_slot: Slot, gas: u64) {
        self.slots.entry(target_slot).or_default().set_gas(gas);
    }

    pub fn set_slot_base_fee(&mut self, target_slot: Slot, base_fee: Amount) {
        self.slots
            .entry(target_slot)
            .or_default()
            .set_base_fee(base_fee);
    }

    pub fn get_slot_base_fee(&self, target_slot: &Slot) -> Option<Amount> {
        self.slots
            .get(target_slot)
            .and_then(|slot_changes| slot_changes.get_base_fee())
    }

    pub fn set_total_gas(&mut self, gas: u128) {
        self.total_gas = DeferredRegistryGasChange::Set(gas);
    }

    pub fn get_total_gas(&self) -> Option<u128> {
        match self.total_gas {
            DeferredRegistryGasChange::Set(v) => Some(v),
            DeferredRegistryGasChange::Keep => None,
        }
    }
}

pub struct DeferredRegistryChangesSerializer {
    slots_length: U64VarIntSerializer,
    slot_changes_serializer: DeferredRegistrySlotChangesSerializer,
    slot_serializer: SlotSerializer,
    total_gas_serializer: SetOrKeepSerializer<u128, U128VarIntSerializer>,
}

impl DeferredRegistryChangesSerializer {
    pub fn new() -> Self {
        Self {
            slots_length: U64VarIntSerializer::new(),
            slot_changes_serializer: DeferredRegistrySlotChangesSerializer::new(),
            slot_serializer: SlotSerializer::new(),
            total_gas_serializer: SetOrKeepSerializer::new(U128VarIntSerializer::new()),
        }
    }
}

impl Serializer<DeferredRegistryChanges> for DeferredRegistryChangesSerializer {
    fn serialize(
        &self,
        value: &DeferredRegistryChanges,
        buffer: &mut Vec<u8>,
    ) -> Result<(), SerializeError> {
        self.slots_length.serialize(
            &(value.slots.len().try_into().map_err(|_| {
                SerializeError::GeneralError("Fail to transform usize to u64".to_string())
            })?),
            buffer,
        )?;

        for (slot, changes) in &value.slots {
            self.slot_serializer.serialize(slot, buffer)?;
            self.slot_changes_serializer.serialize(changes, buffer)?;
        }

        self.total_gas_serializer
            .serialize(&value.total_gas, buffer)?;

        Ok(())
    }
}

// todo deserialize

pub struct DeferredRegistryChangesDeserializer {
    slots_length: U64VarIntDeserializer,
    slot_changes_deserializer: DeferredRegistrySlotChangesDeserializer,
    slot_deserializer: SlotDeserializer,
    total_gas_deserializer: SetOrKeepDeserializer<u128, U128VarIntDeserializer>,
}

impl DeferredRegistryChangesDeserializer {
    pub fn new(thread_count: u8, max_gas: u64, max_deferred_calls_pool_changes: u64) -> Self {
        Self {
            slots_length: U64VarIntDeserializer::new(
                Included(u64::MIN),
                Included(max_deferred_calls_pool_changes),
            ),
            slot_changes_deserializer: DeferredRegistrySlotChangesDeserializer::new(
                thread_count,
                max_gas,
                max_deferred_calls_pool_changes,
            ),
            slot_deserializer: SlotDeserializer::new(
                (Bound::Included(0), Bound::Included(u64::MAX)),
                (Bound::Included(0), Bound::Excluded(thread_count)),
            ),
            total_gas_deserializer: SetOrKeepDeserializer::new(U128VarIntDeserializer::new(
                Included(u128::MIN),
                Included(u128::MAX),
            )),
        }
    }
}

impl Deserializer<DeferredRegistryChanges> for DeferredRegistryChangesDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], DeferredRegistryChanges, E> {
        unimplemented!("DeferredRegistryChangesDeserializer::deserialize")
        // context(
        //     "Failed DeferredRegistryChanges deserialization",
        //     length_count(
        //         context("Failed length deserialization", |input| {
        //             self.slots_length.deserialize(input)
        //         }),
        //         |input| {

        //         },
        //     ),
        // )
    }
}
