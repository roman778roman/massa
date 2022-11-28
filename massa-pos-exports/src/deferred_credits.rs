use massa_hash::{Hash, HASH_SIZE_BYTES};
use massa_models::{
    address::{Address, AddressDeserializer, AddressSerializer},
    amount::{Amount, AmountDeserializer, AmountSerializer},
    prehash::PreHashMap,
    slot::{Slot, SlotDeserializer, SlotSerializer},
};
use massa_serialization::{
    Deserializer, SerializeError, Serializer, U64VarIntDeserializer, U64VarIntSerializer,
};
use nom::{
    error::{context, ContextError, ParseError},
    multi::length_count,
    sequence::tuple,
    IResult, Parser,
};
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included};

const DEFERRED_CREDITS_HASH_INITIAL_BYTES: &[u8; 32] = &[0; HASH_SIZE_BYTES];

#[derive(Debug, Clone)]
/// Structure containing all the PoS deferred credits information
pub struct DeferredCredits {
    /// Deferred credits
    pub credits: BTreeMap<Slot, PreHashMap<Address, Amount>>,
    /// Hash of the current deferred credits state
    pub hash: Hash,
}

impl Default for DeferredCredits {
    fn default() -> Self {
        Self {
            credits: Default::default(),
            hash: Hash::from_bytes(DEFERRED_CREDITS_HASH_INITIAL_BYTES),
        }
    }
}

struct DeferredCreditsHashComputer {
    slot_ser: SlotSerializer,
    address_ser: AddressSerializer,
    amount_ser: AmountSerializer,
}

impl DeferredCreditsHashComputer {
    fn new() -> Self {
        Self {
            slot_ser: SlotSerializer::new(),
            address_ser: AddressSerializer::new(),
            amount_ser: AmountSerializer::new(),
        }
    }

    fn compute_slot_credits_hash(
        &self,
        slot: &Slot,
        credits: &PreHashMap<Address, Amount>,
    ) -> Hash {
        // serialization can never fail in the following computations, unwrap is justified
        let mut buffer = Vec::new();
        self.slot_ser.serialize(slot, &mut buffer).unwrap();
        let mut hash = Hash::compute_from(&buffer);
        for (address, amount) in credits {
            hash ^= self.compute_single_credit_hash(address, amount);
        }
        hash
    }

    fn compute_single_credit_hash(&self, address: &Address, amount: &Amount) -> Hash {
        let mut buffer = Vec::new();
        self.address_ser.serialize(address, &mut buffer).unwrap();
        self.amount_ser.serialize(amount, &mut buffer).unwrap();
        Hash::compute_from(&buffer)
    }
}

impl DeferredCredits {
    /// Extends the current `DeferredCredits` with another but accumulates the addresses and amounts
    pub fn nested_replace(&mut self, other: Self) {
        for (slot, other_credits) in other.credits {
            self.credits
                .entry(slot)
                .and_modify(|current_credits| {
                    for (address, other_amount) in other_credits.iter() {
                        current_credits
                            .entry(*address)
                            .and_modify(|current_amount| *current_amount = *other_amount)
                            .or_insert(*other_amount);
                    }
                })
                .or_insert(other_credits);
        }
    }

    /// Extends the current `DeferredCredits` with another, accumulates the addresses and amounts and computes the object hash, use only on finality
    pub fn final_nested_replace(&mut self, other: Self) {
        let hash_computer = DeferredCreditsHashComputer::new();
        for (slot, other_credits) in other.credits {
            self.credits
                .entry(slot)
                // if has given slot entry update it
                .and_modify(|current_credits| {
                    for (address, other_amount) in other_credits.iter() {
                        current_credits
                            .entry(*address)
                            .and_modify(|current_amount| {
                                // compute the current credit hash and XOR it
                                self.hash ^= hash_computer
                                    .compute_single_credit_hash(address, current_amount);
                                // compute the replacement credit hash and XOR it
                                self.hash ^=
                                    hash_computer.compute_single_credit_hash(address, other_amount);
                                // set the new credit amount
                                *current_amount = *other_amount
                            })
                            .or_insert({
                                // compute the credit hash and XOR it
                                self.hash ^=
                                    hash_computer.compute_single_credit_hash(address, other_amount);
                                // set the credit amount
                                *other_amount
                            });
                    }
                })
                // if misses the given slot entry insert
                .or_insert({
                    // compute every slot credits hash and XOR them
                    hash_computer.compute_slot_credits_hash(&slot, &other_credits);
                    // set the slot credits
                    other_credits
                });
        }
    }

    /// Remove zero credits, use only on finality
    pub fn remove_zeros(&mut self) {
        let hash_computer = DeferredCreditsHashComputer::new();
        let mut delete_slots = Vec::new();
        for (slot, credits) in &mut self.credits {
            credits.retain(|_addr, amount| !amount.is_zero());
            if credits.is_empty() {
                delete_slots.push(*slot);
                self.hash ^= hash_computer.compute_slot_credits_hash(slot, credits);
            }
        }
        for slot in delete_slots {
            self.credits.remove(&slot);
        }
    }

    /// Gets the deferred credits for a given address that will be credited at a given slot
    pub fn get_address_deferred_credit_for_slot(
        &self,
        addr: &Address,
        slot: &Slot,
    ) -> Option<Amount> {
        if let Some(v) = self
            .credits
            .get(slot)
            .and_then(|slot_credits| slot_credits.get(addr))
        {
            return Some(*v);
        }
        None
    }

    /// Insert/overwrite a deferred credit
    pub fn insert(&mut self, addr: Address, slot: Slot, amount: Amount) {
        let entry = self.credits.entry(slot).or_default();
        entry.insert(addr, amount);
    }
}

/// Serializer for `DeferredCredits`
pub struct DeferredCreditsSerializer {
    slot_ser: SlotSerializer,
    u64_ser: U64VarIntSerializer,
    credits_ser: CreditsSerializer,
}

impl Default for DeferredCreditsSerializer {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredCreditsSerializer {
    /// Creates a new `DeferredCredits` serializer
    pub fn new() -> Self {
        Self {
            slot_ser: SlotSerializer::new(),
            u64_ser: U64VarIntSerializer::new(),
            credits_ser: CreditsSerializer::new(),
        }
    }
}

impl Serializer<DeferredCredits> for DeferredCreditsSerializer {
    fn serialize(
        &self,
        value: &DeferredCredits,
        buffer: &mut Vec<u8>,
    ) -> Result<(), SerializeError> {
        // deferred credits length
        self.u64_ser
            .serialize(&(value.credits.len() as u64), buffer)?;
        // deferred credits
        for (slot, credits) in &value.credits {
            // slot
            self.slot_ser.serialize(slot, buffer)?;
            // credits
            self.credits_ser.serialize(credits, buffer)?;
        }
        Ok(())
    }
}

/// Deserializer for `DeferredCredits`
pub struct DeferredCreditsDeserializer {
    u64_deserializer: U64VarIntDeserializer,
    slot_deserializer: SlotDeserializer,
    credit_deserializer: CreditsDeserializer,
}

impl DeferredCreditsDeserializer {
    /// Creates a new `DeferredCredits` deserializer
    pub fn new(thread_count: u8, max_credits_length: u64) -> DeferredCreditsDeserializer {
        DeferredCreditsDeserializer {
            u64_deserializer: U64VarIntDeserializer::new(
                Included(u64::MIN),
                Included(max_credits_length),
            ),
            slot_deserializer: SlotDeserializer::new(
                (Included(0), Included(u64::MAX)),
                (Included(0), Excluded(thread_count)),
            ),
            credit_deserializer: CreditsDeserializer::new(max_credits_length),
        }
    }
}

impl Deserializer<DeferredCredits> for DeferredCreditsDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], DeferredCredits, E> {
        context(
            "Failed DeferredCredits deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    context("Failed slot deserialization", |input| {
                        self.slot_deserializer.deserialize(input)
                    }),
                    context("Failed credit deserialization", |input| {
                        self.credit_deserializer.deserialize(input)
                    }),
                )),
            ),
        )
        .map(|elements| DeferredCredits {
            credits: elements.into_iter().collect(),
            hash: Hash::from_bytes(DEFERRED_CREDITS_HASH_INITIAL_BYTES),
        })
        .parse(buffer)
    }
}
/// Serializer for `Credits`
pub struct CreditsSerializer {
    u64_ser: U64VarIntSerializer,
    amount_ser: AmountSerializer,
}

impl Default for CreditsSerializer {
    fn default() -> Self {
        Self::new()
    }
}

impl CreditsSerializer {
    /// Creates a new `Credits` serializer
    pub fn new() -> Self {
        Self {
            u64_ser: U64VarIntSerializer::new(),
            amount_ser: AmountSerializer::new(),
        }
    }
}

impl Serializer<PreHashMap<Address, Amount>> for CreditsSerializer {
    fn serialize(
        &self,
        value: &PreHashMap<Address, Amount>,
        buffer: &mut Vec<u8>,
    ) -> Result<(), SerializeError> {
        // slot credits length
        self.u64_ser.serialize(&(value.len() as u64), buffer)?;
        // slot credits
        for (addr, amount) in value {
            // address
            buffer.extend(addr.to_bytes());
            // credited amount
            self.amount_ser.serialize(amount, buffer)?;
        }
        Ok(())
    }
}

/// Deserializer for a single credit
struct CreditsDeserializer {
    u64_deserializer: U64VarIntDeserializer,
    address_deserializer: AddressDeserializer,
    amount_deserializer: AmountDeserializer,
}

impl CreditsDeserializer {
    /// Creates a new single credit deserializer
    fn new(max_credits_length: u64) -> CreditsDeserializer {
        CreditsDeserializer {
            u64_deserializer: U64VarIntDeserializer::new(
                Included(u64::MIN),
                Included(max_credits_length),
            ),
            address_deserializer: AddressDeserializer::new(),
            amount_deserializer: AmountDeserializer::new(
                Included(Amount::MIN),
                Included(Amount::MAX),
            ),
        }
    }
}

impl Deserializer<PreHashMap<Address, Amount>> for CreditsDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], PreHashMap<Address, Amount>, E> {
        context(
            "Failed Credit deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    context("Failed address deserialization", |input| {
                        self.address_deserializer.deserialize(input)
                    }),
                    context("Failed amount deserialization", |input| {
                        self.amount_deserializer.deserialize(input)
                    }),
                )),
            ),
        )
        .map(|elements| elements.into_iter().collect())
        .parse(buffer)
    }
}