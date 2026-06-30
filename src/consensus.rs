use crate::ContractError;
use soroban_sdk::{contracttype, symbol_short, Address, Env, Map, Symbol, Vec};

/// Basis-point denominator used when converting a BPS fraction to a multiplier.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// Minimum safety threshold for block height gaps between consecutive submissions.
/// Prevents ledger bloat from rapid telemetry updates within the same block window.
pub const MIN_BLOCK_GAP_THRESHOLD: u32 = 3;

/// Storage key for tracking the last successful ledger index per node.
pub(crate) const BLOCK_TRACKER_KEY: Symbol = symbol_short!("BLKTRK");

/// A single provider's submission paired with its consensus weight (stake amount).
#[contracttype]
#[derive(Clone)]
pub struct WeightedEntry {
    /// Raw submitted value (e.g. price in smallest denomination).
    pub value: u64,
    /// Weight assigned to this entry, typically the provider's staked amount.
    pub weight: u64,
}

/// Multiply a raw value by a weight, returning `Overflow` on saturation.
///
/// This is the inner kernel called for each entry in `compute_weighted_sum`.
pub fn apply_weight(value: u64, weight: u64) -> Result<u64, ContractError> {
    value.checked_mul(weight).ok_or(ContractError::Overflow)
}

/// Accumulate the sum of `entry.value * entry.weight` across every entry in the
/// dataset.  Each individual product and every running-total addition is checked
/// so no intermediate result can wrap silently.
pub fn compact_duplicate_price_rows(
    env: &Env,
    entries: &Vec<WeightedEntry>,
) -> Result<Vec<WeightedEntry>, ContractError> {
    let mut compacted: Vec<WeightedEntry> = Vec::new(env);
    let mut index_by_value: Map<u64, u64> = Map::new(env);

    for i in 0..entries.len() {
        let entry = entries.get(i).unwrap();

        if let Some(existing_index) = index_by_value.get(entry.value) {
            let idx = existing_index as u32;
            let existing = compacted.get(idx).unwrap();
            let merged_weight = existing
                .weight
                .checked_add(entry.weight)
                .ok_or(ContractError::Overflow)?;

            compacted.set(
                idx,
                WeightedEntry {
                    value: existing.value,
                    weight: merged_weight,
                },
            );
        } else {
            let index = compacted.len() as u64;
            compacted.push_back(entry.clone());
            index_by_value.set(entry.value, index);
        }
    }

    Ok(compacted)
}

pub fn compute_weighted_sum(
    env: &Env,
    entries: &Vec<WeightedEntry>,
) -> Result<(u64, u64), ContractError> {
    let compacted = compact_duplicate_price_rows(env, entries)?;
    let mut weighted_sum: u64 = 0;
    let mut total_weight: u64 = 0;

    for i in 0..compacted.len() {
        let entry = compacted.get(i).unwrap();

        let weighted_value = apply_weight(entry.value, entry.weight)?;

        weighted_sum = weighted_sum
            .checked_add(weighted_value)
            .ok_or(ContractError::Overflow)?;

        total_weight = total_weight
            .checked_add(entry.weight)
            .ok_or(ContractError::Overflow)?;
    }

    Ok((weighted_sum, total_weight))
}

/// Compute the stake-weighted average across all entries.
///
/// Returns `(weighted_average, total_weight)`.  Division is always safe once
/// the checked accumulation above has succeeded, but we guard the zero-weight
/// edge case to avoid a panic.
pub fn compute_weighted_average(
    env: &Env,
    entries: &Vec<WeightedEntry>,
) -> Result<u64, ContractError> {
    let (weighted_sum, total_weight) = compute_weighted_sum(env, entries)?;

    if total_weight == 0 {
        return Ok(0);
    }

    Ok(weighted_sum / total_weight)
}

/// Compute the minimum weight required for quorum.
///
/// `quorum_bps` is expressed in basis points (e.g. 6700 = 67 %).
/// The multiplication `total_weight * quorum_bps` is checked before the
/// denominator division so large stake totals cannot overflow silently.
pub fn compute_quorum_threshold(total_weight: u64, quorum_bps: u64) -> Result<u64, ContractError> {
    let numerator = total_weight
        .checked_mul(quorum_bps)
        .ok_or(ContractError::Overflow)?;

    Ok(numerator / BPS_DENOMINATOR)
}

/// Scale a raw consensus score by a fixed precision multiplier.
///
/// Used when promoting an integer score to a higher-precision representation
/// before further computation.  Both the score itself and the scale factor are
/// checked to prevent rollover.
pub fn normalize_weight_score(raw_score: u64, precision: u64) -> Result<u64, ContractError> {
    raw_score
        .checked_mul(precision)
        .ok_or(ContractError::Overflow)
}

/// Compute how much of the accumulated weighted score a single entry
/// contributes, expressed in basis points of the total.
///
/// Returns a value in [0, 10 000].  The intermediate `entry_weight * BPS_DENOMINATOR`
/// product is checked before the final division.
pub fn entry_weight_share_bps(entry_weight: u64, total_weight: u64) -> Result<u64, ContractError> {
    if total_weight == 0 {
        return Ok(0);
    }

    let numerator = entry_weight
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(ContractError::Overflow)?;

    Ok(numerator / total_weight)
}

/// Result type for price retrieval.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PriceResult {
    Live(i64),          // Live price from oracle
    Fallback(i64, u32), // Historical backup price and safety warning code
}

/// Safety warning code returned when the live oracle feed is offline.
pub const WARNING_ORACLE_OFFLINE: u32 = 1001;

/// Retrieves the price for a given asset symbol with a graceful fallback.
///
/// Returns `PriceResult::Live` if the oracle call succeeds, otherwise `PriceResult::Fallback`
/// and emits a warning event.
pub fn get_price_with_fallback(env: &Env, asset: Symbol, fallback_rate: i64) -> PriceResult {
    let oracle_result = mock_oracle_price(env, asset.clone());
    match oracle_result {
        Ok(price) => PriceResult::Live(price),
        Err(_) => {
            // Emit a warning event for observability.
            env.events().publish(
                (symbol_short!("FallbackW"), asset),
                (fallback_rate, WARNING_ORACLE_OFFLINE),
            );
            PriceResult::Fallback(fallback_rate, WARNING_ORACLE_OFFLINE)
        }
    }
}

/// Mock function representing the external oracle price lookup.
/// Uses temporary storage to allow tests to configure success/failure paths.
pub fn mock_oracle_price(env: &Env, _asset: Symbol) -> Result<i64, ContractError> {
    let key = symbol_short!("mock_prc");
    if env.storage().temporary().has(&key) {
        let val: i64 = env.storage().temporary().get(&key).unwrap();
        if val >= 0 {
            Ok(val)
        } else {
            Err(ContractError::NotRegistered)
        }
    } else {
        Err(ContractError::NotRegistered)
    }
}

/// Validate and register the sequence of the latest asset update.
/// Rejects incoming price updates instantly if the incoming tracking sequence
/// is less than or equal to the active stored checkpoint value.
pub fn verify_and_update_sequence(
    env: &Env,
    asset: Symbol,
    incoming_sequence: u32,
) -> Result<(), ContractError> {
    let key = symbol_short!("SEQ_TRK");
    let mut tracker: Map<Symbol, u32> = env
        .storage()
        .instance()
        .get(&key)
        .unwrap_or_else(|| Map::new(env));

    if let Some(active_sequence) = tracker.get(asset.clone()) {
        if incoming_sequence <= active_sequence {
            return Err(ContractError::StaleSequence);
        }
    }

    tracker.set(asset, incoming_sequence);
    env.storage().instance().set(&key, &tracker);
    Ok(())
}

/// Validate and enforce minimum block height gap between consecutive submissions.
/// Rejects incoming transaction payloads if the current network ledger index has not
/// progressed by at least MIN_BLOCK_GAP_THRESHOLD blocks since the node's last successful entry.
///
/// This prevents ledger bloat and reduces gas fees from rapid telemetry updates within
/// a singular block index window.
pub fn verify_and_update_block_gap(
    env: &Env,
    node: Address,
) -> Result<(), ContractError> {
    let current_ledger_index = env.ledger().sequence();
    let mut block_tracker: Map<Address, u32> = env
        .storage()
        .instance()
        .get(&BLOCK_TRACKER_KEY)
        .unwrap_or_else(|| Map::new(env));

    if let Some(last_ledger_index) = block_tracker.get(node.clone()) {
        let gap = current_ledger_index.saturating_sub(last_ledger_index);
        if gap < MIN_BLOCK_GAP_THRESHOLD {
            return Err(ContractError::StaleSequence);
        }
    }

    block_tracker.set(node, current_ledger_index);
    env.storage().instance().set(&BLOCK_TRACKER_KEY, &block_tracker);
    Ok(())
}

/// Maximum number of active reporting validators supported by the stack-based array.
pub const MAX_ACTIVE_VALIDATORS: usize = 16;

/// Stack-allocated validator registry using a flat array configuration.
/// This eliminates heap allocations during validator confirmation checks,
/// significantly reducing gas costs under heavy transaction loads.
/// 
/// Note: This is a pure Rust struct (not #[contracttype]) designed for in-function
/// stack allocation. Store validated IDs separately in contract storage if persistence is needed.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatorRegistry {
    /// Flat array of validator IDs in sorted order for binary search.
    /// Unused slots are filled with u32::MAX.
    pub validator_ids: [u32; MAX_ACTIVE_VALIDATORS],
    /// Number of active validators in the registry (0..=MAX_ACTIVE_VALIDATORS).
    pub count: usize,
}

impl ValidatorRegistry {
    /// Create an empty validator registry.
    pub const fn new() -> Self {
        Self {
            validator_ids: [u32::MAX; MAX_ACTIVE_VALIDATORS],
            count: 0,
        }
    }

    /// Check if a validator ID is registered using fast binary search.
    /// 
    /// Time complexity: O(log n) where n is the number of active validators.
    pub fn is_registered(&self, validator_id: u32) -> bool {
        if self.count == 0 {
            return false;
        }

        let active_slice = &self.validator_ids[..self.count];
        active_slice.binary_search(&validator_id).is_ok()
    }

    /// Register a new validator ID in the sorted array.
    /// 
    /// Returns `Err(ContractError::Overflow)` if the registry is full.
    /// Returns `Err(ContractError::AlreadyRegistered)` if the ID is already present.
    pub fn register(&mut self, validator_id: u32) -> Result<(), ContractError> {
        if self.count >= MAX_ACTIVE_VALIDATORS {
            return Err(ContractError::Overflow);
        }

        let active_slice = &self.validator_ids[..self.count];
        
        match active_slice.binary_search(&validator_id) {
            Ok(_) => Err(ContractError::AlreadyRegistered),
            Err(insert_pos) => {
                // Shift elements to the right to maintain sorted order
                for i in (insert_pos..self.count).rev() {
                    self.validator_ids[i + 1] = self.validator_ids[i];
                }
                self.validator_ids[insert_pos] = validator_id;
                self.count += 1;
                Ok(())
            }
        }
    }

    /// Remove a validator ID from the registry.
    /// 
    /// Returns `Err(ContractError::NotRegistered)` if the ID is not found.
    pub fn unregister(&mut self, validator_id: u32) -> Result<(), ContractError> {
        if self.count == 0 {
            return Err(ContractError::NotRegistered);
        }

        let active_slice = &self.validator_ids[..self.count];
        
        match active_slice.binary_search(&validator_id) {
            Err(_) => Err(ContractError::NotRegistered),
            Ok(remove_pos) => {
                // Shift elements to the left to maintain sorted order
                for i in remove_pos..(self.count - 1) {
                    self.validator_ids[i] = self.validator_ids[i + 1];
                }
                self.validator_ids[self.count - 1] = u32::MAX;
                self.count -= 1;
                Ok(())
            }
        }
    }

    /// Get the number of active validators.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Check if the registry is empty.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Check if the registry is full.
    pub const fn is_full(&self) -> bool {
        self.count >= MAX_ACTIVE_VALIDATORS
    }
}

impl Default for ValidatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Verify that a validator is actively registered before processing their submission.
/// 
/// This function uses stack-based binary search to efficiently confirm validator
/// identity without heap allocations, reducing gas costs under heavy loads.
pub fn verify_active_validator(
    registry: &ValidatorRegistry,
    validator_id: u32,
) -> Result<(), ContractError> {
    if registry.is_registered(validator_id) {
        Ok(())
    } else {
        Err(ContractError::NotRegistered)
    }
}

/// Batch verify multiple validator IDs against the active registry.
/// 
/// Returns `Ok(())` if all validators are registered, otherwise returns
/// `Err(ContractError::NotRegistered)` on the first unregistered validator.
pub fn verify_validators_batch(
    registry: &ValidatorRegistry,
    validator_ids: &[u32],
) -> Result<(), ContractError> {
    for &validator_id in validator_ids {
        verify_active_validator(registry, validator_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::Env;

    fn make_entries(env: &Env, pairs: &[(u64, u64)]) -> Vec<WeightedEntry> {
        let mut v = Vec::new(env);
        for &(value, weight) in pairs {
            v.push_back(WeightedEntry { value, weight });
        }
        v
    }

    // --- apply_weight ---

    #[test]
    fn test_apply_weight_normal() {
        assert_eq!(apply_weight(100, 50).unwrap(), 5_000);
    }

    #[test]
    fn test_apply_weight_zero_value() {
        assert_eq!(apply_weight(0, u64::MAX).unwrap(), 0);
    }

    #[test]
    fn test_apply_weight_zero_weight() {
        assert_eq!(apply_weight(u64::MAX, 0).unwrap(), 0);
    }

    #[test]
    fn test_apply_weight_overflow() {
        let result = apply_weight(u64::MAX, 2);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    // --- compute_weighted_sum ---

    #[test]
    fn test_weighted_sum_single_entry() {
        let env = Env::default();
        let entries = make_entries(&env, &[(200, 3)]);
        let (ws, tw) = compute_weighted_sum(&env, &entries).unwrap();
        assert_eq!(ws, 600);
        assert_eq!(tw, 3);
    }

    #[test]
    fn test_weighted_sum_multiple_entries() {
        let env = Env::default();
        // (100 * 10) + (200 * 5) = 1000 + 1000 = 2000, total_weight = 15
        let entries = make_entries(&env, &[(100, 10), (200, 5)]);
        let (ws, tw) = compute_weighted_sum(&env, &entries).unwrap();
        assert_eq!(ws, 2_000);
        assert_eq!(tw, 15);
    }

    #[test]
    fn test_weighted_sum_duplicate_price_rows_compact() {
        let env = Env::default();
        // Same price value appears twice; weights should merge before weighted sum.
        let entries = make_entries(&env, &[(100, 10), (100, 5), (200, 5)]);
        let (ws, tw) = compute_weighted_sum(&env, &entries).unwrap();
        assert_eq!(ws, 2_500);
        assert_eq!(tw, 20);
    }

    #[test]
    fn test_weighted_sum_empty_dataset() {
        let env = Env::default();
        let entries = make_entries(&env, &[]);
        let (ws, tw) = compute_weighted_sum(&env, &entries).unwrap();
        assert_eq!(ws, 0);
        assert_eq!(tw, 0);
    }

    #[test]
    fn test_weighted_sum_overflow_on_product() {
        let env = Env::default();
        let entries = make_entries(&env, &[(u64::MAX, 2)]);
        let result = compute_weighted_sum(&env, &entries);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    #[test]
    fn test_weighted_sum_overflow_on_accumulation() {
        let env = Env::default();
        // Two entries that are individually fine but their sum overflows u64.
        let half = u64::MAX / 2;
        let entries = make_entries(&env, &[(half, 2), (half, 2)]);
        // half*2 = u64::MAX-1, second half*2 would overflow the running sum
        // u64::MAX - 1 + (u64::MAX - 1) overflows
        let result = compute_weighted_sum(&env, &entries);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    // --- compute_weighted_average ---

    #[test]
    fn test_weighted_average_normal() {
        let env = Env::default();
        // (1000 * 3 + 2000 * 1) / (3 + 1) = 5000 / 4 = 1250
        let entries = make_entries(&env, &[(1_000, 3), (2_000, 1)]);
        assert_eq!(compute_weighted_average(&env, &entries).unwrap(), 1_250);
    }

    #[test]
    fn test_weighted_average_zero_total_weight() {
        let env = Env::default();
        let entries = make_entries(&env, &[(500, 0), (300, 0)]);
        assert_eq!(compute_weighted_average(&env, &entries).unwrap(), 0);
    }

    // --- compute_quorum_threshold ---

    #[test]
    fn test_quorum_threshold_two_thirds() {
        // 6700 BPS of 1_000_000 = 670_000
        assert_eq!(compute_quorum_threshold(1_000_000, 6_700).unwrap(), 670_000);
    }

    #[test]
    fn test_quorum_threshold_fifty_percent() {
        assert_eq!(compute_quorum_threshold(200, 5_000).unwrap(), 100);
    }

    #[test]
    fn test_quorum_threshold_overflow() {
        // u64::MAX * 2 overflows even before dividing
        let result = compute_quorum_threshold(u64::MAX, 2);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    #[test]
    fn test_quorum_threshold_zero_weight() {
        assert_eq!(compute_quorum_threshold(0, 6_700).unwrap(), 0);
    }

    // --- normalize_weight_score ---

    #[test]
    fn test_normalize_score_normal() {
        assert_eq!(normalize_weight_score(42, 1_000).unwrap(), 42_000);
    }

    #[test]
    fn test_normalize_score_overflow() {
        let result = normalize_weight_score(u64::MAX, 2);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    #[test]
    fn test_normalize_score_zero() {
        assert_eq!(normalize_weight_score(0, u64::MAX).unwrap(), 0);
    }

    // --- entry_weight_share_bps ---

    #[test]
    fn test_share_bps_full_weight() {
        // Entry holds all the weight → 10 000 BPS
        assert_eq!(entry_weight_share_bps(500, 500).unwrap(), 10_000);
    }

    #[test]
    fn test_share_bps_half_weight() {
        assert_eq!(entry_weight_share_bps(250, 500).unwrap(), 5_000);
    }

    #[test]
    fn test_share_bps_zero_total() {
        assert_eq!(entry_weight_share_bps(100, 0).unwrap(), 0);
    }

    #[test]
    fn test_share_bps_overflow_on_numerator() {
        let result = entry_weight_share_bps(u64::MAX, 1);
        assert_eq!(result, Err(ContractError::Overflow));
    }

    // --- get_price_with_fallback tests ---

    #[test]
    fn test_get_price_with_fallback_success() {
        let env = Env::default();
        let contract_id = env.register_contract(None, crate::TimeLockedUpgradeContract);

        env.as_contract(&contract_id, || {
            let asset = symbol_short!("BTC");
            // Configure the mock price to return 50000
            env.storage()
                .temporary()
                .set(&symbol_short!("mock_prc"), &50000i64);

            let result = get_price_with_fallback(&env, asset, 45000);
            assert_eq!(result, PriceResult::Live(50000));
        });
    }

    #[test]
    fn test_get_price_with_fallback_failure() {
        let env = Env::default();
        let contract_id = env.register_contract(None, crate::TimeLockedUpgradeContract);

        env.as_contract(&contract_id, || {
            let asset = symbol_short!("BTC");
            // No mock price configured (or set to negative to trigger failure)
            env.storage()
                .temporary()
                .set(&symbol_short!("mock_prc"), &-1i64);

            let result = get_price_with_fallback(&env, asset, 45000);
            assert_eq!(result, PriceResult::Fallback(45000, WARNING_ORACLE_OFFLINE));
        });
    }

    #[test]
    fn test_get_price_with_fallback_failure_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        let contract_id = env.register_contract(None, crate::TimeLockedUpgradeContract);

        env.as_contract(&contract_id, || {
            let asset = symbol_short!("BTC");

            let result = get_price_with_fallback(&env, asset.clone(), 45000);
            assert_eq!(result, PriceResult::Fallback(45000, WARNING_ORACLE_OFFLINE));

            let events = env.events().all();
            assert!(events.len() > 0);
        });
    }

    // --- ValidatorRegistry tests ---

    #[test]
    fn test_validator_registry_new() {
        let registry = ValidatorRegistry::new();
        assert_eq!(registry.count, 0);
        assert!(registry.is_empty());
        assert!(!registry.is_full());
    }

    #[test]
    fn test_validator_registry_register_single() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(100).is_ok());
        assert_eq!(registry.count, 1);
        assert!(registry.is_registered(100));
        assert!(!registry.is_registered(99));
    }

    #[test]
    fn test_validator_registry_register_maintains_sorted_order() {
        let mut registry = ValidatorRegistry::new();
        // Register in random order
        assert!(registry.register(50).is_ok());
        assert!(registry.register(10).is_ok());
        assert!(registry.register(30).is_ok());
        assert!(registry.register(20).is_ok());
        assert!(registry.register(40).is_ok());

        assert_eq!(registry.count, 5);
        // Verify sorted order
        assert_eq!(registry.validator_ids[0], 10);
        assert_eq!(registry.validator_ids[1], 20);
        assert_eq!(registry.validator_ids[2], 30);
        assert_eq!(registry.validator_ids[3], 40);
        assert_eq!(registry.validator_ids[4], 50);
    }

    #[test]
    fn test_validator_registry_register_duplicate_fails() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(100).is_ok());
        assert_eq!(registry.register(100), Err(ContractError::AlreadyRegistered));
        assert_eq!(registry.count, 1);
    }

    #[test]
    fn test_validator_registry_register_full_capacity() {
        let mut registry = ValidatorRegistry::new();
        // Fill the registry to capacity
        for i in 0..MAX_ACTIVE_VALIDATORS {
            assert!(registry.register(i as u32).is_ok());
        }
        assert_eq!(registry.len(), MAX_ACTIVE_VALIDATORS);
        assert!(registry.is_full());
        
        // Attempting to register one more should fail
        assert_eq!(registry.register(999), Err(ContractError::Overflow));
    }

    #[test]
    fn test_validator_registry_unregister_single() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(100).is_ok());
        assert!(registry.is_registered(100));
        
        assert!(registry.unregister(100).is_ok());
        assert_eq!(registry.count, 0);
        assert!(!registry.is_registered(100));
    }

    #[test]
    fn test_validator_registry_unregister_not_found() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(100).is_ok());
        
        assert_eq!(registry.unregister(200), Err(ContractError::NotRegistered));
        assert_eq!(registry.count, 1);
    }

    #[test]
    fn test_validator_registry_unregister_from_empty() {
        let mut registry = ValidatorRegistry::new();
        assert_eq!(registry.unregister(100), Err(ContractError::NotRegistered));
    }

    #[test]
    fn test_validator_registry_unregister_maintains_sorted_order() {
        let mut registry = ValidatorRegistry::new();
        for i in [10, 20, 30, 40, 50] {
            assert!(registry.register(i).is_ok());
        }

        // Remove from middle
        assert!(registry.unregister(30).is_ok());
        assert_eq!(registry.count, 4);
        assert_eq!(registry.validator_ids[0], 10);
        assert_eq!(registry.validator_ids[1], 20);
        assert_eq!(registry.validator_ids[2], 40);
        assert_eq!(registry.validator_ids[3], 50);
        assert_eq!(registry.validator_ids[4], u32::MAX); // Cleared slot
    }

    #[test]
    fn test_validator_registry_is_registered_binary_search() {
        let mut registry = ValidatorRegistry::new();
        let ids = [5, 15, 25, 35, 45, 55, 65, 75, 85, 95];
        
        for &id in &ids {
            assert!(registry.register(id).is_ok());
        }

        // Test all registered IDs
        for &id in &ids {
            assert!(registry.is_registered(id));
        }

        // Test unregistered IDs
        assert!(!registry.is_registered(0));
        assert!(!registry.is_registered(10));
        assert!(!registry.is_registered(20));
        assert!(!registry.is_registered(100));
    }

    #[test]
    fn test_verify_active_validator_success() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(42).is_ok());
        
        assert!(verify_active_validator(&registry, 42).is_ok());
    }

    #[test]
    fn test_verify_active_validator_failure() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(42).is_ok());
        
        assert_eq!(
            verify_active_validator(&registry, 99),
            Err(ContractError::NotRegistered)
        );
    }

    #[test]
    fn test_verify_validators_batch_all_valid() {
        let mut registry = ValidatorRegistry::new();
        for id in [10, 20, 30, 40, 50] {
            assert!(registry.register(id).is_ok());
        }

        let batch = [10, 30, 50];
        assert!(verify_validators_batch(&registry, &batch).is_ok());
    }

    #[test]
    fn test_verify_validators_batch_one_invalid() {
        let mut registry = ValidatorRegistry::new();
        for id in [10, 20, 30] {
            assert!(registry.register(id).is_ok());
        }

        let batch = [10, 20, 40]; // 40 is not registered
        assert_eq!(
            verify_validators_batch(&registry, &batch),
            Err(ContractError::NotRegistered)
        );
    }

    #[test]
    fn test_verify_validators_batch_empty_registry() {
        let registry = ValidatorRegistry::new();
        let batch = [10, 20];
        
        assert_eq!(
            verify_validators_batch(&registry, &batch),
            Err(ContractError::NotRegistered)
        );
    }

    #[test]
    fn test_verify_validators_batch_empty_batch() {
        let mut registry = ValidatorRegistry::new();
        assert!(registry.register(10).is_ok());
        
        let batch: [u32; 0] = [];
        assert!(verify_validators_batch(&registry, &batch).is_ok());
    }

    #[test]
    fn test_validator_registry_stress_test() {
        let mut registry = ValidatorRegistry::new();
        
        // Register validators in reverse order
        for i in (0..MAX_ACTIVE_VALIDATORS).rev() {
            assert!(registry.register(i as u32).is_ok());
        }

        // Verify all are registered and in sorted order
        for i in 0..MAX_ACTIVE_VALIDATORS {
            assert!(registry.is_registered(i as u32));
            assert_eq!(registry.validator_ids[i], i as u32);
        }

        // Unregister every other validator
        for i in (0..MAX_ACTIVE_VALIDATORS).step_by(2) {
            assert!(registry.unregister(i as u32).is_ok());
        }

        assert_eq!(registry.len(), MAX_ACTIVE_VALIDATORS / 2);

        // Verify remaining validators
        for i in 0..MAX_ACTIVE_VALIDATORS {
            if i % 2 == 0 {
                assert!(!registry.is_registered(i as u32));
            } else {
                assert!(registry.is_registered(i as u32));
            }
        }
    }

    #[test]
    fn test_validator_registry_edge_case_min_max_values() {
        let mut registry = ValidatorRegistry::new();
        
        // Test with extreme u32 values
        assert!(registry.register(0).is_ok());
        assert!(registry.register(u32::MAX - 1).is_ok()); // u32::MAX is reserved for empty slots
        assert!(registry.register(1).is_ok());
        assert!(registry.register(u32::MAX - 2).is_ok());

        assert_eq!(registry.len(), 4);
        assert!(registry.is_registered(0));
        assert!(registry.is_registered(1));
        assert!(registry.is_registered(u32::MAX - 2));
        assert!(registry.is_registered(u32::MAX - 1));
        
        // Verify sorted order
        assert_eq!(registry.validator_ids[0], 0);
        assert_eq!(registry.validator_ids[1], 1);
        assert_eq!(registry.validator_ids[2], u32::MAX - 2);
        assert_eq!(registry.validator_ids[3], u32::MAX - 1);
    }

    #[test]
    fn test_validator_registry_len_and_empty_checks() {
        let mut registry = ValidatorRegistry::new();
        
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        assert!(!registry.is_full());

        registry.register(10).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
        assert!(!registry.is_full());

        for i in 1..MAX_ACTIVE_VALIDATORS {
            registry.register(i as u32 * 10).unwrap();
        }
        
        assert_eq!(registry.len(), MAX_ACTIVE_VALIDATORS);
        assert!(!registry.is_empty());
        assert!(registry.is_full());
    }
}
