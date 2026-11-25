use soroban_sdk::{Address, Env, Vec};

use crate::{index, storage::DataKey};

/// Hard cap for pagination. Prevents a caller passing `limit = u32::MAX`
/// from forcing the contract to build a `Vec` up to the length of the
/// sender's entire history in a single view call.
pub const MAX_PAGE_SIZE: u32 = 100;

/// Returns a paginated slice of `v` starting at `offset` with at most `limit`
/// elements.
///
/// `limit` is clamped to [`MAX_PAGE_SIZE`] (100) so a `u32::MAX` view call
/// cannot DoS the RPC/simulator by materialising an unbounded vector. Uses
/// `saturating_add` to avoid panicking when `offset + limit` overflows
/// `usize` — both values are caller-controlled and the release profile enables
/// overflow checks, so a raw `+` would abort this read-only view call instead
/// of gracefully clamping. The result is clamped to the vector's actual length,
/// so callers never receive more elements than exist.
///
/// This function is shared by [`DripFactory::streams_by_sender`] and
/// [`DripFactory::streams_by_recipient`].
pub fn paginate(env: &Env, v: Vec<u64>, offset: u32, limit: u32) -> Vec<u64> {
    let mut result = Vec::new(env);
    let start = offset as usize;
    // Clamp caller-controlled limit to a hard maximum to bound work/memory.
    let effective_limit = limit.min(MAX_PAGE_SIZE) as usize;
    // offset + effective_limit is caller-controlled and can overflow u32; the release
    // profile enables overflow-checks, so a raw `+` here would panic this
    // read-only view call rather than just clamping to the Vec's length.
    let end = (offset as usize).saturating_add(effective_limit);
    for i in start..end.min(v.len() as usize) {
        result.push_back(v.get(i as u32).unwrap());
    }
    result
}

/// Returns a paginated slice of `Address` vector starting at `offset` with at most `limit` elements.
pub fn paginate_addresses(env: &Env, v: Vec<Address>, offset: u32, limit: u32) -> Vec<Address> {
    let mut result = Vec::new(env);
    let start = offset as usize;
    let effective_limit = limit.min(MAX_PAGE_SIZE) as usize;
    let end = (offset as usize).saturating_add(effective_limit);
    for i in start..end.min(v.len() as usize) {
        result.push_back(v.get(i as u32).unwrap());
    }
    result
}

/// Cursor-paginated stream addresses created by `creator` (Issue #901).
///
/// `start_offset` is a zero-based position in the creator's own stream history
/// (creation order), and at most `limit` addresses are considered, clamped to
/// [`MAX_PAGE_SIZE`]. Each page resolves only the IDs it covers, so the work and
/// ledger footprint per call stay bounded however many streams the creator has.
///
/// Advance the cursor by `limit` (or the clamped value), not by the length of the
/// returned vector: an ID whose `StreamAddr` entry has expired is skipped, so a
/// page can hold fewer addresses than the window it covered. An offset at or past
/// the end yields an empty vector.
pub fn get_streams_by_creator_paginated(
    env: &Env,
    creator: Address,
    start_offset: u32,
    limit: u32,
) -> Vec<Address> {
    let page = index::streams_by_sender(env, creator, start_offset, limit);
    let mut addrs = Vec::new(env);
    for id in page.ids.iter() {
        if let Some(addr) = env
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::StreamAddr(id))
        {
            addrs.push_back(addr);
        }
    }
    addrs
}

/// Pages over the whole stream registry in ID (creation) order (issue #959).
///
/// Returns up to `limit` stream addresses starting at registry position
/// `offset`, with `limit` clamped to [`MAX_PAGE_SIZE`]. Walks the IDs in
/// `offset..min(offset + limit, StreamCount)` and resolves each
/// `StreamAddr(id)`; each hit has its persistent TTL renewed. An ID whose entry
/// has expired is skipped, so a page can hold fewer addresses than the window
/// it covered. Advance the cursor by the clamped `limit`, not by the length of
/// the returned vector. An offset at or past the end yields an empty vector.
///
/// Arithmetic is done in `u64` with saturating adds, so `offset = u32::MAX`
/// cannot overflow.
pub fn get_streams_paged(env: &Env, offset: u32, limit: u32) -> Vec<Address> {
    let mut out = Vec::new(env);
    let total: u64 = env
        .storage()
        .instance()
        .get(&DataKey::StreamCount)
        .unwrap_or(0);
    let effective_limit = limit.min(MAX_PAGE_SIZE) as u64;
    let start = offset as u64;
    if start >= total || effective_limit == 0 {
        return out;
    }
    let end = start.saturating_add(effective_limit).min(total);
    for id in start..end {
        let key = DataKey::StreamAddr(id);
        if let Some(addr) = env.storage().persistent().get::<_, Address>(&key) {
            crate::ttl::bump_persistent(env, &key);
            out.push_back(addr);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DripFactory;
    use soroban_sdk::testutils::Address as _;

    /// Registers a factory and records `count` streams for `creator`, each with
    /// a distinct stream address. Returns the stream addresses in creation order.
    fn seed_creator_streams(
        env: &Env,
        contract_id: &Address,
        creator: &Address,
        count: u32,
    ) -> Vec<Address> {
        let mut expected = Vec::new(env);
        env.as_contract(contract_id, || {
            for id in 0..count as u64 {
                let stream = Address::generate(env);
                env.storage()
                    .persistent()
                    .set(&DataKey::StreamAddr(id), &stream);
                index::append_sender_index(env, creator, id);
                expected.push_back(stream);
            }
        });
        expected
    }

    #[test]
    fn pages_walk_the_creator_history_in_order() {
        let env = Env::default();
        let contract_id = env.register_contract(None, DripFactory);
        let creator = Address::generate(&env);
        let all = seed_creator_streams(&env, &contract_id, &creator, 5);

        env.as_contract(&contract_id, || {
            assert_eq!(
                get_streams_by_creator_paginated(&env, creator.clone(), 0, 2),
                soroban_sdk::vec![&env, all.get(0).unwrap(), all.get(1).unwrap()]
            );
            assert_eq!(
                get_streams_by_creator_paginated(&env, creator.clone(), 2, 2),
                soroban_sdk::vec![&env, all.get(2).unwrap(), all.get(3).unwrap()]
            );
            assert_eq!(
                get_streams_by_creator_paginated(&env, creator.clone(), 4, 2),
                soroban_sdk::vec![&env, all.get(4).unwrap()]
            );
        });
    }

    #[test]
    fn offset_past_the_end_returns_empty() {
        let env = Env::default();
        let contract_id = env.register_contract(None, DripFactory);
        let creator = Address::generate(&env);
        seed_creator_streams(&env, &contract_id, &creator, 3);

        env.as_contract(&contract_id, || {
            assert!(get_streams_by_creator_paginated(&env, creator.clone(), 3, 10).is_empty());
            assert!(get_streams_by_creator_paginated(&env, creator.clone(), 500, 10).is_empty());
        });
    }

    #[test]
    fn unknown_creator_returns_empty() {
        let env = Env::default();
        let contract_id = env.register_contract(None, DripFactory);
        let creator = Address::generate(&env);
        let other = Address::generate(&env);
        seed_creator_streams(&env, &contract_id, &other, 2);

        env.as_contract(&contract_id, || {
            assert!(get_streams_by_creator_paginated(&env, creator.clone(), 0, 10).is_empty());
        });
    }

    #[test]
    fn limit_is_clamped_to_max_page_size() {
        let env = Env::default();
        let contract_id = env.register_contract(None, DripFactory);
        let creator = Address::generate(&env);
        let count = MAX_PAGE_SIZE + 1;
        seed_creator_streams(&env, &contract_id, &creator, count);

        env.as_contract(&contract_id, || {
            let page = get_streams_by_creator_paginated(&env, creator.clone(), 0, u32::MAX);
            assert_eq!(page.len(), MAX_PAGE_SIZE);
        });
    }
}
