//! Causal version vectors.
//!
//! Every revision carries a version vector: a map from device ID to a
//! per-device counter. Two revisions are ordered by dominance:
//!
//! - `A` dominates `B` when every counter in `A` is `>=` the corresponding
//!   counter in `B` and at least one is strictly greater.
//! - If neither dominates, the revisions are concurrent — a conflict.
//!
//! This is how SyncEngine detects concurrent edits without trusting
//! modification clocks (SRD §10).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// A map of per-device counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionVector {
    counters: BTreeMap<DeviceId, u64>,
}

/// Result of comparing two version vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CausalOrder {
    /// Same causal position.
    Equal,
    /// Left is causally newer (dominates right).
    Dominates,
    /// Left is causally older (dominated by right).
    DominatedBy,
    /// Concurrent — neither dominates; a conflict.
    Concurrent,
}

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counter for `device`, or 0 when absent.
    pub fn get(&self, device: &DeviceId) -> u64 {
        self.counters.get(device).copied().unwrap_or(0)
    }

    /// Returns a new vector with `device`'s counter incremented.
    pub fn incremented(&self, device: DeviceId) -> Self {
        let mut next = self.clone();
        *next.counters.entry(device).or_insert(0) += 1;
        next
    }

    /// Returns a new vector with `device`'s counter set to at least `value`.
    pub fn merged_with(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        for (device, counter) in &other.counters {
            let entry = merged.counters.entry(*device).or_insert(0);
            *entry = (*entry).max(*counter);
        }
        merged
    }

    /// The component-wise maximum of two vectors, i.e. `merged_with`.
    pub fn merge(&mut self, other: &Self) {
        *self = self.merged_with(other);
    }

    /// Compares causal position against `other`.
    pub fn compare(&self, other: &Self) -> CausalOrder {
        let mut any_greater = false;
        let mut any_less = false;

        let devices: std::collections::BTreeSet<_> = self
            .counters
            .keys()
            .chain(other.counters.keys())
            .copied()
            .collect();

        for device in devices {
            let a = self.get(&device);
            let b = other.get(&device);
            if a > b {
                any_greater = true;
            } else if a < b {
                any_less = true;
            }
        }

        match (any_greater, any_less) {
            (false, false) => CausalOrder::Equal,
            (true, false) => CausalOrder::Dominates,
            (false, true) => CausalOrder::DominatedBy,
            (true, true) => CausalOrder::Concurrent,
        }
    }

    /// True when this vector is strictly newer than `other`.
    pub fn dominates(&self, other: &Self) -> bool {
        self.compare(other) == CausalOrder::Dominates
    }

    /// True when the vectors are concurrent.
    pub fn is_concurrent_with(&self, other: &Self) -> bool {
        self.compare(other) == CausalOrder::Concurrent
    }

    /// Number of tracked devices.
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(n: u128) -> DeviceId {
        DeviceId::new_from_u128(n)
    }

    #[cfg(test)]
    impl DeviceId {
        fn new_from_u128(n: u128) -> Self {
            Self(uuid::Uuid::from_u128(n))
        }
    }

    #[test]
    fn empty_vectors_are_equal() {
        assert_eq!(
            VersionVector::new().compare(&VersionVector::new()),
            CausalOrder::Equal
        );
    }

    #[test]
    fn increment_makes_a_vector_dominate() {
        let base = VersionVector::new();
        let newer = base.incremented(dev(1));
        assert_eq!(newer.compare(&base), CausalOrder::Dominates);
        assert_eq!(base.compare(&newer), CausalOrder::DominatedBy);
        assert!(newer.dominates(&base));
    }

    #[test]
    fn same_device_increments_are_linear() {
        let a = VersionVector::new().incremented(dev(1));
        let b = a.incremented(dev(1));
        assert_eq!(b.compare(&a), CausalOrder::Dominates);
        assert_eq!(a.get(&dev(1)), 1);
        assert_eq!(b.get(&dev(1)), 2);
    }

    #[test]
    fn independent_device_increments_are_concurrent() {
        let base = VersionVector::new().incremented(dev(1));
        let a = base.incremented(dev(1));
        let b = base.incremented(dev(2));
        assert_eq!(a.compare(&b), CausalOrder::Concurrent);
        assert!(a.is_concurrent_with(&b));
    }

    #[test]
    fn merge_takes_the_max_per_device() {
        let base = VersionVector::new().incremented(dev(1));
        let a = base.incremented(dev(1));
        let b = base.incremented(dev(2));
        let merged = a.merged_with(&b);
        assert_eq!(merged.get(&dev(1)), 2);
        assert_eq!(merged.get(&dev(2)), 1);
        assert!(merged.dominates(&a));
        assert!(merged.dominates(&b));
    }

    #[test]
    fn vectors_round_trip_through_json() {
        let v = VersionVector::new()
            .incremented(dev(1))
            .incremented(dev(2))
            .incremented(dev(1));
        let json = serde_json::to_string(&v).expect("serialize");
        let back: VersionVector = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn vector_of_many_devices_compares_correctly() {
        let mut a = VersionVector::new();
        let mut b = VersionVector::new();
        for i in 0..10 {
            a = a.incremented(dev(i));
            b = b.incremented(dev(i));
        }
        assert_eq!(a.compare(&b), CausalOrder::Equal);
        let a2 = a.incremented(dev(5));
        assert_eq!(a2.compare(&b), CausalOrder::Dominates);
    }
}
