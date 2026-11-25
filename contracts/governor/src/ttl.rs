//! Instance TTL management for `DripGovernor`.
//!
//! The threshold/extend-to values are protocol-wide policy and live in
//! [`drip_common::ttl`], so every contract renews instance storage on the same
//! schedule. This module only keeps the crate-local `ttl::bump` call sites
//! unchanged — it must not restate the constants (issue #649).

/// Extends the governor's instance storage TTL.
///
/// Re-exported from [`drip_common::ttl::bump_instance`]; kept under the
/// `bump` name because the governor's rbac `on_success` hook is wired to
/// `Some(ttl::bump)` (see `drip_common::rbac::require_role_or_admin`).
pub use drip_common::ttl::bump_instance as bump;

/// Extends proposal storage TTL to cover the entire proposal lifecycle.
///
/// Ensures proposal entries remain available through the execution window to prevent
/// storage expiration before execution completes. Extends TTL by timelock_delay +
/// execution_window_secs to cover the full proposal lifetime from creation through
/// potential delayed execution.
pub fn extend_for_proposal(env: &soroban_sdk::Env, timelock_secs: u64, execution_window_secs: u64) {
    let total_lifetime = timelock_secs.saturating_add(execution_window_secs);
    drip_common::ttl::extend_instance(env, total_lifetime);
}

/// #1015: Bump persistent storage TTL when a vote is cast on a proposal.
/// Ensures the proposal record survives through the execution window
/// even for proposals with long voting + timelock durations.
pub fn bump_on_vote(env: &soroban_sdk::Env, timelock_secs: u64, execution_window_secs: u64) {
    extend_for_proposal(env, timelock_secs, execution_window_secs);
}

/// #1015: Bump persistent storage TTL when a proposal is executed.
/// Ensures the proposal record and all related state remain retrievable
/// through final archiving.
pub fn bump_on_execute(env: &soroban_sdk::Env, timelock_secs: u64, execution_window_secs: u64) {
    extend_for_proposal(env, timelock_secs, execution_window_secs);
}
