use soroban_sdk::{contracttype, Address, Env};

use drip_common::is_zero_address;

use crate::storage::DataKey;
use crate::Error;

/// Minimum voting period: 17280 ledgers (~1 day on Soroban) (Issue #772).
/// Prevents flash-loan voting attacks and allows DAO participants time to react.
pub const MIN_VOTING_PERIOD_LEDGERS: u32 = 17280;

/// Maximum voting period: 241920 ledgers (~14 days on Soroban) (#969).
/// Bounds how long a proposal can stay open, so governance cannot stall.
pub const MAX_VOTING_PERIOD_LEDGERS: u32 = 241_920;

/// Default timelock delay between proposal creation and execution: 48 hours (Issue #967).
pub const DEFAULT_TIMELOCK_DELAY_SECS: u64 = 172_800;

#[contracttype]
#[derive(Clone)]
pub struct GovernorConfig {
    pub fee_bps: u32,
    pub fee_recipient: Address,
    pub min_duration_seconds: u64,
    pub max_duration_seconds: u64,
    pub max_rate_per_second: i128,
    pub factory_address: Address,
    /// Seconds a stream must remain paused before its recipient may call
    /// `DripStream::force_cancel`. Governance-configurable per deployment
    /// (e.g. 7 days for payroll, 90 days for long-horizon vesting) instead
    /// of the previously hardcoded 30-day constant.
    pub force_cancel_pause_secs: u64,
    /// Minimum quorum threshold in basis points (10,000 = 100%).
    pub min_quorum_bps: u32,
    /// Minimum voting period in ledger sequences (Issue #772).
    pub voting_period_ledgers: u32,
    /// Token address used to escrow proposal stake, if configured.
    pub proposal_stake_token: Option<Address>,
    /// Minimum token amount required to create a proposal.
    pub min_proposal_stake: i128,
    /// Seconds that must elapse between proposal creation and execution (Issue #967).
    /// Governance-configurable via `set_timelock_delay`.
    pub timelock_delay_secs: u64,
}

/// Load the governor configuration from instance storage.
///
/// Returns `Err(NotInitialized)` when the governor has not been
/// initialised — rather than panicking on missing keys — so callers
/// (including cross-contract callers in `DripFactory`) get a
/// meaningful error instead of a generic host trap.
pub fn load(env: &Env) -> Result<GovernorConfig, Error> {
    let s = env.storage().instance();

    let fee_recipient: Address = s.get(&DataKey::FeeRecipient).ok_or(Error::NotInitialized)?;
    let factory_address: Address = s
        .get(&DataKey::FactoryAddress)
        .ok_or(Error::NotInitialized)?;

    Ok(GovernorConfig {
        fee_bps: s.get(&DataKey::FeeBps).unwrap_or(30),
        fee_recipient,
        min_duration_seconds: s.get(&DataKey::MinDurationSeconds).unwrap_or(3600),
        max_duration_seconds: s.get(&DataKey::MaxDurationSeconds).unwrap_or(315_360_000), // 10 years
        max_rate_per_second: s
            .get(&DataKey::MaxRatePerSecond)
            .unwrap_or(1_000_000_000_000_000),
        factory_address,
        force_cancel_pause_secs: s.get(&DataKey::ForceCancelPauseSecs).unwrap_or(2_592_000), // 30 days
        min_quorum_bps: s.get(&DataKey::MinQuorumBps).unwrap_or(0),
        voting_period_ledgers: s.get(&DataKey::VotingPeriodLedgers).unwrap_or(MIN_VOTING_PERIOD_LEDGERS),
        proposal_stake_token: s.get(&DataKey::ProposalStakeToken),
        min_proposal_stake: s.get(&DataKey::MinProposalStake).unwrap_or(0),
        timelock_delay_secs: s
            .get(&DataKey::TimelockDelaySecs)
            .unwrap_or(DEFAULT_TIMELOCK_DELAY_SECS),
    })
}

/// Validate and set the voting period (#772, #969).
/// The period must lie within MIN_VOTING_PERIOD_LEDGERS..=MAX_VOTING_PERIOD_LEDGERS
/// (about 1 to 14 days).
pub fn set_voting_period(env: &Env, voting_period_ledgers: u32) -> Result<(), Error> {
    if !(MIN_VOTING_PERIOD_LEDGERS..=MAX_VOTING_PERIOD_LEDGERS).contains(&voting_period_ledgers) {
        return Err(Error::InvalidParam);
    }
    env.storage()
        .instance()
        .set(&DataKey::VotingPeriodLedgers, &voting_period_ledgers);
    Ok(())
}

/// #1012: Validate quorum percentage boundaries during governance initialization.
/// Quorum must be in [0, 10_000] (0% to 100%) and voting period must be
/// at least MIN_VOTING_PERIOD_LEDGERS. Rejects unworkable configs like
/// >100% quorum or a 0-ledger voting period.
pub fn validate_config_bounds(
    min_quorum_bps: u32,
    voting_period_ledgers: u32,
) -> Result<(), Error> {
    if min_quorum_bps > 10_000 {
        return Err(Error::InvalidParam);
    }
    if voting_period_ledgers < MIN_VOTING_PERIOD_LEDGERS {
        return Err(Error::InvalidParam);
    }
    Ok(())
}

/// #1012: Validate and set the quorum threshold.
pub fn set_quorum(env: &Env, min_quorum_bps: u32) -> Result<(), Error> {
    validate_config_bounds(min_quorum_bps, get_voting_period(env))?;
    env.storage()
        .instance()
        .set(&DataKey::MinQuorumBps, &min_quorum_bps);
    Ok(())
}

/// Get the current voting period in ledgers.
pub fn get_voting_period(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::VotingPeriodLedgers)
        .unwrap_or(MIN_VOTING_PERIOD_LEDGERS)
}

/// Store the timelock delay in seconds (Issue #967).
///
/// Authority is checked by the caller; this only writes the value.
pub fn set_timelock_delay(env: &Env, timelock_delay_secs: u64) {
    env.storage()
        .instance()
        .set(&DataKey::TimelockDelaySecs, &timelock_delay_secs);
}

/// Validate and store a proposal token stake threshold.
pub fn set_proposal_stake_config(
    env: &Env,
    token: &Address,
    min_stake: i128,
) -> Result<(), Error> {
    if min_stake <= 0 || is_zero_address(env, token) {
        return Err(Error::InvalidParam);
    }
    let storage = env.storage().instance();
    storage.set(&DataKey::ProposalStakeToken, token);
    storage.set(&DataKey::MinProposalStake, &min_stake);
    Ok(())
}
