#![no_std]

mod errors;
mod events;
mod storage;
#[cfg(test)]
mod tests;
mod ttl;

use drip_common::{is_zero_address, TTL_EXTEND_TO};
use drip_stream::DripStreamClient;
use errors::Error;
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, BytesN, Env, Map, Vec};
use storage::{
    check_vault_balance, get_depositor_token_balance, get_emergency_admin, get_emergency_migration,
    get_max_limit, get_operator, get_operator_withdraw_limit, get_owner, get_owner_proposal_ttl,
    get_pending_owner, get_pending_owner_proposed_at, get_pending_owner_proposer, get_strategy_swept,
    get_token, get_token_account_balance, get_token_max_limit, get_token_min_deposit, is_paused,
    is_strategy_approved, is_token_approved, record_vault_balance, remove_emergency_migration,
    remove_operator, remove_pending_owner, remove_pending_owner_proposed_at, remove_pending_owner_proposer,
    set_depositor_token_balance, set_emergency_admin, set_emergency_migration, set_max_limit, set_operator,
    set_operator_withdraw_limit, set_owner, set_owner_proposal_ttl as store_owner_proposal_ttl, set_paused,
    set_pending_owner, set_pending_owner_proposed_at, set_pending_owner_proposer, set_strategy_approved,
    set_strategy_swept, set_token, set_token_account_balance, set_token_approved, set_token_max_limit,
    set_token_min_deposit, tracked_liability_for_token, DataKey, EmergencyMigration,
};
use ttl::bump_instance;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositSpec {
    pub from: Address,
    pub amount: i128,
}

/// Alias for [`DepositSpec`] matching the batch-deposit issue interface (Issue #924).
pub type DepositArgs = DepositSpec;

/// Per-deposit outcome returned by [`TokenVault::batch_deposit_partial`] (Issue #924).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchDepositResult {
    pub index: u32,
    pub success: bool,
    pub error_code: Option<u32>,
    pub deposited: i128,
}

#[contract]
pub struct TokenVault;

const OWNER_PROPOSAL_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const EMERGENCY_MIGRATION_TIMELOCK_SECONDS: u64 = 48 * 60 * 60;

/// Maximum amount accepted by a single deposit (10^30 stroops).
///
/// Streaming math multiplies deposits by rates and durations; keeping every
/// single deposit well below `i128::MAX` leaves headroom for those products
/// instead of allowing them to overflow downstream.
pub const MAX_VAULT_DEPOSIT: i128 = 1_000_000_000_000_000_000_000_000_000_000;

/// Maximum number of independent stream contracts processed by one call.
pub const MAX_STREAM_BATCH_WITHDRAWALS: u32 = 10;

/// Lightweight transient reentrancy guard macro protecting external token transfers.
///
/// Sets `DataKey::ReentrancyLock` in temporary storage before executing `$body`
/// and clears it upon completion. Any reentrant invocation while the lock is
/// active immediately returns `Err(Error::ReentrancyDetected)`.
#[macro_export]
macro_rules! non_reentrant {
    ($env:expr, $body:block) => {{
        if $crate::storage::is_reentrancy_locked(&$env) {
            return Err($crate::errors::Error::ReentrancyDetected);
        }
        $crate::storage::set_reentrancy_lock(&$env);
        let result = (|| -> Result<_, $crate::errors::Error> { $body })();
        $crate::storage::remove_reentrancy_lock(&$env);
        result
    }};
}

/// Alias for [`non_reentrant!`].
#[macro_export]
macro_rules! reentrancy_guard {
    ($env:expr, $body:block) => {
        $crate::non_reentrant!($env, $body)
    };
}

/// Pre-flight validation verifying that `token` adheres to standard SEP-0041
/// Soroban token interface (non-zero address, contract target, balance read probe) (Issue #696).
pub fn validate_sep41_token(env: &Env, token: &Address) -> Result<(), Error> {
    if is_zero_address(env, token) {
        return Err(Error::InvalidToken);
    }
    let client = token::Client::new(env, token);
    // Probe balance method to verify contract implements SEP-41 balance signature
    let _ = client.balance(&env.current_contract_address());
    Ok(())
}

fn token_client(env: &Env) -> Result<token::Client<'_>, Error> {
    let token_addr = get_token(env).ok_or(Error::NotInitialized)?;
    Ok(token::Client::new(env, &token_addr))
}

fn vault_balance(env: &Env) -> Result<i128, Error> {
    Ok(token_client(env)?.balance(&env.current_contract_address()))
}

fn withdraw_cap(env: &Env, caller: &Address, owner: &Address) -> Result<i128, Error> {
    if caller == owner {
        return Ok(i128::MAX);
    }

    if get_operator(env).as_ref() != Some(caller) {
        return Err(Error::NotAuthorized);
    }

    get_operator_withdraw_limit(env).ok_or(Error::LimitExceeded)
}

/// Checks that `caller` is either the vault owner or the currently delegated
/// operator, then consumes the caller's auth. Returns `NotAuthorized` if
/// `caller` matches neither role.
///
/// Mirrors `DripStream::require_sender_or_operator` — the owner can hand off
/// day-to-day withdrawal authority to a hot wallet / ops key without exposing
/// the cold owner key for routine operations.
fn require_owner_or_operator(env: &Env, caller: &Address, owner: &Address) -> Result<(), Error> {
    let operator = get_operator(env);
    let is_owner = caller == owner;
    let is_op = operator.as_ref().map(|op| caller == op).unwrap_or(false);
    if is_owner || is_op {
        caller.require_auth();
        Ok(())
    } else {
        Err(Error::NotAuthorized)
    }
}

/// Short-circuit helper: reject any state-modifying call while paused.
///
/// Delegates to the protocol-wide gate in [`drip_common::pause`], mapping the
/// shared `PausedError` onto the vault's own `Error::VaultPaused` — a distinct
/// code so callers never mistake a halted vault for a balance shortfall
/// (Issue #896).
fn assert_not_paused(env: &Env) -> Result<(), Error> {
    drip_common::pause::require_not_paused(env, &DataKey::Paused).map_err(|_| Error::VaultPaused)
}

fn withdraw_payouts(
    env: &Env,
    caller: &Address,
    payouts: &Vec<(Address, i128)>,
) -> Result<(), Error> {
    assert_not_paused(env)?;
    non_reentrant!(env, {
        let owner = get_owner(env).ok_or(Error::NotInitialized)?;
        require_owner_or_operator(env, caller, &owner)?;

        if payouts.is_empty() {
            return Err(Error::InvalidAmount);
        }

        let token = get_token(env).ok_or(Error::NotInitialized)?;
        validate_sep41_token(env, &token)?;

        let mut total = 0_i128;
        for index in 0..payouts.len() {
            let (_, amount) = payouts.get(index).ok_or(Error::InvalidAmount)?;
            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }
            total = total.checked_add(amount).ok_or(Error::ArithmeticOverflow)?;
        }

        if total > withdraw_cap(env, caller, &owner)? {
            return Err(Error::LimitExceeded);
        }

        let balance = vault_balance(env)?;
        if total > balance {
            return Err(Error::InsufficientVaultLiquidity);
        }

        let tk = token_client(env)?;
        let contract_bal = tk.balance(&env.current_contract_address());
        if total > contract_bal {
            return Err(Error::InsufficientVaultLiquidity);
        }

        let mut expected_balance = balance;
        let by_operator = caller != &owner;
        for index in 0..payouts.len() {
            let (to, amount) = payouts.get(index).ok_or(Error::InvalidAmount)?;
            expected_balance = expected_balance
                .checked_sub(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            tk.transfer(&env.current_contract_address(), &to, &amount);
            let new_balance = vault_balance(env)?;
            if new_balance != expected_balance {
                return Err(Error::WithdrawTransferFailed);
            }
            record_vault_balance(env, &token, new_balance);

            let cur_dep_bal = get_depositor_token_balance(env, caller, &token);
            if cur_dep_bal >= amount {
                set_depositor_token_balance(env, caller, &token, cur_dep_bal - amount);
            }
            ttl::extend_balance_ttl(env, &token, caller);

            events::withdrawn(env, caller, &to, amount, new_balance, by_operator);
        }

        bump_instance(env);
        Ok(())
    })
}

#[contractimpl]
impl TokenVault {
    /// Initialise the vault.
    ///
    /// # Errors
    ///
    /// - `InvalidAmount` — `max_limit` is not positive.
    /// - `AlreadyInitialized` — the vault already has an owner.
    ///
    /// Returns `Result` rather than panicking so the typed error reaches the
    /// caller through `try_initialize`. A `panic_with_error!` surfaces only as
    /// an untyped host error, which means a caller cannot distinguish
    /// `AlreadyInitialized` from any other trap without matching on a raw
    /// numeric code. `DripOracle::initialize` already returns `Result` for the
    /// same reason.
    pub fn initialize(
        env: Env,
        owner: Address,
        token: Address,
        max_limit: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        owner.require_auth();

        if max_limit <= 0 {
            return Err(Error::InvalidAmount);
        }
        validate_sep41_token(&env, &token)?;

        if get_owner(&env).is_some() {
            return Err(Error::AlreadyInitialized);
        }

        bump_instance(&env);
        set_owner(&env, &owner);
        set_emergency_admin(&env, &owner);
        set_token(&env, &token);
        set_token_approved(&env, &token, true);
        set_max_limit(&env, &max_limit);
        store_owner_proposal_ttl(&env, OWNER_PROPOSAL_TTL_SECONDS);

        events::initialized(&env, &owner, &token, max_limit);
        Ok(())
    }

    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<(), Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            require_owner_or_operator(&env, &from, &owner)?;

            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }
            if amount > MAX_VAULT_DEPOSIT {
                return Err(Error::DepositExceedsMax);
            }

            let _owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            let balance = vault_balance(&env)?;
            let default_token = get_token(&env).ok_or(Error::NotInitialized)?;
            if !is_token_approved(&env, &default_token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            let min_deposit = get_token_min_deposit(&env, &default_token);
            if min_deposit > 0 && amount < min_deposit {
                return Err(Error::DepositBelowMin);
            }
            let max = get_max_limit(&env).ok_or(Error::NotInitialized)?;

            let expected_balance = balance
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            if expected_balance > max {
                return Err(Error::LimitExceeded);
            }
            if !check_vault_balance(&env, &default_token, balance) {
                return Err(Error::VaultBalanceChanged);
            }

            let tk = token_client(&env)?;
            tk.transfer(&from, &env.current_contract_address(), &amount);
            let new_balance = tk.balance(&env.current_contract_address());
            if new_balance > max {
                return Err(Error::LimitExceeded);
            }
            if new_balance != expected_balance {
                return Err(Error::DepositTransferFailed);
            }

            let default_token = get_token(&env).ok_or(Error::NotInitialized)?;
            let cur_acc_bal = get_token_account_balance(&env, &default_token, &from);
            if let Some(new_acc_bal) = cur_acc_bal.checked_add(amount) {
                set_token_account_balance(&env, &default_token, &from, new_acc_bal);
            }
            let cur_dep_bal = get_depositor_token_balance(&env, &from, &default_token);
            if let Some(new_dep_bal) = cur_dep_bal.checked_add(amount) {
                set_depositor_token_balance(&env, &from, &default_token, new_dep_bal);
            }
            ttl::extend_balance_ttl(&env, &default_token, &from);

            record_vault_balance(&env, &default_token, new_balance);
            bump_instance(&env);
            events::deposited(&env, &from, amount, new_balance);
            Ok(())
        })
    }

    /// Deposits tokens with an explicit fee breakdown for financial accounting (Issue #687).
    ///
    /// Deducts `fee_amount` from `gross_amount` and escrows the remaining `net_amount` in the vault.
    /// Emits `TokenDeposited` event containing `DepositEvent { gross_amount, fee_amount, net_amount }`.
    pub fn deposit_with_fee(
        env: Env,
        from: Address,
        gross_amount: i128,
        fee_amount: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        if fee_amount < 0 || gross_amount <= 0 || fee_amount >= gross_amount {
            return Err(Error::InvalidAmount);
        }
        if gross_amount > MAX_VAULT_DEPOSIT {
            return Err(Error::DepositExceedsMax);
        }
        let net_amount = gross_amount
            .checked_sub(fee_amount)
            .ok_or(Error::ArithmeticOverflow)?;

        Self::deposit(env.clone(), from.clone(), net_amount)?;
        events::token_deposited(&env, &from, gross_amount, fee_amount, net_amount);
        Ok(())
    }

    /// Consolidates deposits from multiple accounts in a single transaction.
    ///
    /// Requires authorization from each depositor individually.
    pub fn batch_deposit(env: Env, deposits: Vec<DepositSpec>) -> Result<(), Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            if deposits.is_empty() {
                return Err(Error::InvalidAmount);
            }
            let _owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            let max = get_max_limit(&env).ok_or(Error::NotInitialized)?;
            let default_token = get_token(&env).ok_or(Error::NotInitialized)?;
            let min_deposit = get_token_min_deposit(&env, &default_token);

            let mut total_deposit: i128 = 0;
            for deposit in deposits.iter() {
                if deposit.amount <= 0 {
                    return Err(Error::InvalidAmount);
                }
                if deposit.amount > MAX_VAULT_DEPOSIT {
                    return Err(Error::DepositExceedsMax);
                }
                if min_deposit > 0 && deposit.amount < min_deposit {
                    return Err(Error::DepositBelowMin);
                }
                deposit.from.require_auth();
                total_deposit = total_deposit
                    .checked_add(deposit.amount)
                    .ok_or(Error::ArithmeticOverflow)?;
            }

            let balance = vault_balance(&env)?;
            let default_token = get_token(&env).ok_or(Error::NotInitialized)?;
            if !is_token_approved(&env, &default_token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            let expected_balance = balance
                .checked_add(total_deposit)
                .ok_or(Error::ArithmeticOverflow)?;
            if expected_balance > max {
                return Err(Error::LimitExceeded);
            }
            if !check_vault_balance(&env, &default_token, balance) {
                return Err(Error::VaultBalanceChanged);
            }

            let tk = token_client(&env)?;
            let contract_addr = env.current_contract_address();

            for deposit in deposits.iter() {
                tk.transfer(&deposit.from, &contract_addr, &deposit.amount);
                let current_balance = tk.balance(&contract_addr);
                events::deposited(&env, &deposit.from, deposit.amount, current_balance);

                let cur_acc_bal = get_token_account_balance(&env, &default_token, &deposit.from);
                if let Some(new_acc_bal) = cur_acc_bal.checked_add(deposit.amount) {
                    set_token_account_balance(&env, &default_token, &deposit.from, new_acc_bal);
                }
                let cur_dep_bal = get_depositor_token_balance(&env, &deposit.from, &default_token);
                if let Some(new_dep_bal) = cur_dep_bal.checked_add(deposit.amount) {
                    set_depositor_token_balance(&env, &deposit.from, &default_token, new_dep_bal);
                }
                ttl::extend_balance_ttl(&env, &default_token, &deposit.from);
            }

            let final_balance = tk.balance(&contract_addr);
            if final_balance > max {
                return Err(Error::LimitExceeded);
            }
            if final_balance != expected_balance {
                return Err(Error::DepositTransferFailed);
            }
            record_vault_balance(&env, &default_token, final_balance);

            bump_instance(&env);
            Ok(())
        })
    }

    /// Deposits from multiple senders and reports a per-item outcome (Issue #924).
    ///
    /// Unlike [`Self::batch_deposit`], which is atomic and reverts the whole
    /// batch on the first failure, this variant validates and transfers each
    /// entry independently so funded senders can settle even when another
    /// sender is ineligible or its transfer fails. The vault accounting
    /// (depositor balance, tracked account balance, and balance snapshot) is
    /// updated only for the entries that actually transferred.
    ///
    /// Per-entry failures are reported as `success = false` with a stable
    /// [`Error`] code in `error_code`; they do not revert the batch. Note that
    /// every entry is authorized up front, so a missing authorization still
    /// aborts the transaction at the host level — partial success covers
    /// validation and token-transfer failures, not missing signatures.
    ///
    /// Every successful transfer is followed by a balance read compared
    /// against the running expected total; if the token moved a different
    /// amount, the whole call reverts with [`Error::DepositTransferFailed`]
    /// rather than persisting corrupt accounting.
    pub fn batch_deposit_partial(
        env: Env,
        deposits: Vec<DepositArgs>,
    ) -> Result<Vec<BatchDepositResult>, Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            if deposits.is_empty() {
                return Err(Error::InvalidAmount);
            }

            let _owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            let max = get_max_limit(&env).ok_or(Error::NotInitialized)?;
            let default_token = get_token(&env).ok_or(Error::NotInitialized)?;
            if !is_token_approved(&env, &default_token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            let min_deposit = get_token_min_deposit(&env, &default_token);

            let balance = vault_balance(&env)?;
            if !check_vault_balance(&env, &default_token, balance) {
                return Err(Error::VaultBalanceChanged);
            }

            let tk = token_client(&env)?;
            let contract_addr = env.current_contract_address();
            let mut results = Vec::new(&env);
            let mut expected_balance = balance;

            // Authorize every distinct depositor before moving any funds,
            // matching the two-phase shape of `batch_deposit`.
            let mut authorized: Map<Address, ()> = Map::new(&env);
            for deposit in deposits.iter() {
                if !authorized.contains_key(deposit.from.clone()) {
                    deposit.from.require_auth();
                    authorized.set(deposit.from.clone(), ());
                }
            }

            for (index, deposit) in deposits.iter().enumerate() {
                let failure = if deposit.amount <= 0 {
                    Some(Error::InvalidAmount as u32)
                } else if deposit.amount > MAX_VAULT_DEPOSIT {
                    Some(Error::DepositExceedsMax as u32)
                } else if min_deposit > 0 && deposit.amount < min_deposit {
                    Some(Error::DepositBelowMin as u32)
                } else {
                    match expected_balance.checked_add(deposit.amount) {
                        None => Some(Error::ArithmeticOverflow as u32),
                        Some(projected) if projected > max => Some(Error::LimitExceeded as u32),
                        Some(_) => None,
                    }
                };

                if let Some(code) = failure {
                    results.push_back(BatchDepositResult {
                        index: index as u32,
                        success: false,
                        error_code: Some(code),
                        deposited: 0,
                    });
                    continue;
                }

                if tk
                    .try_transfer(&deposit.from, &contract_addr, &deposit.amount)
                    .is_err()
                {
                    results.push_back(BatchDepositResult {
                        index: index as u32,
                        success: false,
                        error_code: Some(Error::DepositTransferFailed as u32),
                        deposited: 0,
                    });
                    continue;
                }

                expected_balance = expected_balance
                    .checked_add(deposit.amount)
                    .ok_or(Error::ArithmeticOverflow)?;

                let cur_acc_bal = get_token_account_balance(&env, &default_token, &deposit.from);
                let new_acc_bal = cur_acc_bal
                    .checked_add(deposit.amount)
                    .ok_or(Error::ArithmeticOverflow)?;
                set_token_account_balance(&env, &default_token, &deposit.from, new_acc_bal);

                let cur_dep_bal = get_depositor_token_balance(&env, &deposit.from, &default_token);
                let new_dep_bal = cur_dep_bal
                    .checked_add(deposit.amount)
                    .ok_or(Error::ArithmeticOverflow)?;
                set_depositor_token_balance(&env, &deposit.from, &default_token, new_dep_bal);
                ttl::extend_balance_ttl(&env, &default_token, &deposit.from);

                let new_balance = tk.balance(&contract_addr);
                if new_balance != expected_balance {
                    return Err(Error::DepositTransferFailed);
                }
                events::deposited(&env, &deposit.from, deposit.amount, new_balance);

                results.push_back(BatchDepositResult {
                    index: index as u32,
                    success: true,
                    error_code: None,
                    deposited: deposit.amount,
                });
            }

            record_vault_balance(&env, &default_token, expected_balance);

            bump_instance(&env);
            Ok(results)
        })
    }

    pub fn withdraw(env: Env, caller: Address, to: Address, amount: i128) -> Result<(), Error> {
        let mut payouts = Vec::new(&env);
        payouts.push_back((to, amount));
        withdraw_payouts(&env, &caller, &payouts)
    }

    /// Atomically pay several recipients, enforcing the operator's limit
    /// against the sum of all payouts before transferring any tokens.
    pub fn withdraw_batch(
        env: Env,
        caller: Address,
        payouts: Vec<(Address, i128)>,
    ) -> Result<(), Error> {
        withdraw_payouts(&env, &caller, &payouts)
    }

    /// Withdraw each stream's currently available balance for `recipient`.
    ///
    /// The recipient must be the beneficiary of every stream and authorize
    /// each nested stream withdrawal. Each stream still applies its own
    /// withdrawal rules; one failure reverts the entire batch.
    pub fn vault_batch_withdraw(
        env: Env,
        stream_ids: Vec<Address>,
        recipient: Address,
    ) -> Result<i128, Error> {
        if stream_ids.is_empty() || stream_ids.len() > MAX_STREAM_BATCH_WITHDRAWALS {
            return Err(Error::InvalidStreamBatch);
        }

        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let mut payouts = Vec::new(&env);
            let mut total = 0_i128;

            for index in 0..stream_ids.len() {
                let stream_address = stream_ids.get(index).ok_or(Error::InvalidStreamBatch)?;
                for previous_index in 0..index {
                    if stream_ids.get(previous_index).as_ref() == Some(&stream_address) {
                        return Err(Error::InvalidStreamBatch);
                    }
                }

                let stream = DripStreamClient::new(&env, &stream_address);
                let stream_recipient = stream
                    .try_recipient()
                    .map_err(|_| Error::StreamCallFailed)?
                    .map_err(|_| Error::StreamCallFailed)?;
                if stream_recipient != recipient {
                    return Err(Error::NotAuthorized);
                }

                let amount = stream
                    .try_withdrawable()
                    .map_err(|_| Error::StreamCallFailed)?
                    .map_err(|_| Error::StreamCallFailed)?;
                if amount < 0 {
                    return Err(Error::InvalidAmount);
                }
                if amount > 0 {
                    total = total.checked_add(amount).ok_or(Error::ArithmeticOverflow)?;
                    payouts.push_back((stream_address, amount));
                }
            }

            for (stream_address, amount) in payouts.iter() {
                DripStreamClient::new(&env, &stream_address)
                    .try_withdraw(&amount)
                    .map_err(|_| Error::StreamCallFailed)?
                    .map_err(|_| Error::StreamCallFailed)?;
            }

            Ok(total)
        })
    }

    /// Owner sets the per-call cap that bounds how much a delegated operator
    /// may move in a single `withdraw`. Without a limit an operator cannot
    /// withdraw at all — `withdraw` rejects operator calls with
    /// `LimitExceeded` until this is configured. Owner-only.
    pub fn set_operator_withdraw_limit(
        env: Env,
        caller: Address,
        new_limit: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();

        if new_limit <= 0 {
            return Err(Error::InvalidAmount);
        }

        let old_limit = get_operator_withdraw_limit(&env).unwrap_or(0);
        bump_instance(&env);
        set_operator_withdraw_limit(&env, &new_limit);
        events::operator_withdraw_limit_set(&env, &caller, old_limit, new_limit);
        Ok(())
    }

    /// Raising `max_limit` requires `caller == owner`; an operator may only
    /// lower it. `max_limit` is the vault's core risk parameter — it caps
    /// total exposure — so a delegated operator key (a hot wallet meant for
    /// day-to-day operations) must not be able to expand it. This matches
    /// the general principle that delegated keys can reduce but not expand
    /// authority; operators can still tighten the cap on their own.
    pub fn set_limit(env: Env, caller: Address, new_limit: i128) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;

        if new_limit <= 0 {
            return Err(Error::InvalidAmount);
        }
        let balance = vault_balance(&env)?;
        if new_limit < balance {
            return Err(Error::LimitExceeded);
        }
        let old_limit = get_max_limit(&env).ok_or(Error::ArithmeticOverflow)?;

        if new_limit > old_limit {
            if caller != owner {
                return Err(Error::NotAuthorized);
            }
            caller.require_auth();
        } else {
            require_owner_or_operator(&env, &caller, &owner)?;
        }

        bump_instance(&env);
        set_max_limit(&env, &new_limit);
        events::limit_set(&env, &caller, old_limit, new_limit);
        Ok(())
    }

    // ── Operator delegation (owner-gated) ─────────────────────────────────

    /// Owner designates an operator who can perform day-to-day actions on
    /// this vault: `withdraw`, and `set_limit` to *lower* (not raise) the
    /// deposit cap. Raising `max_limit` remains owner-only — see
    /// [`TokenVault::set_limit`] — so a compromised operator key cannot
    /// expand the vault's risk exposure.
    ///
    /// Only the owner may call this. Matches `DripStream::set_operator` — the
    /// owner can delegate day-to-day operations to a hot wallet while keeping
    /// the owner key in cold storage.
    pub fn set_operator(env: Env, caller: Address, operator: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        bump_instance(&env);
        set_operator(&env, &operator);
        events::operator_set(&env, &caller, &operator);
        Ok(())
    }

    /// Owner revokes the operator, removing all delegated authority.
    ///
    /// No-op (not an error) if no operator is currently set.
    pub fn revoke_operator(env: Env, caller: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        bump_instance(&env);
        remove_operator(&env);
        events::operator_revoked(&env, &caller);
        Ok(())
    }

    // ── Owner transfer (owner-gated, 2-step) ──────────────────────────────

    /// Propose a new owner (step 1 of 2).
    ///
    /// Only the current owner may call this. The transfer is not complete
    /// until the proposed address calls `accept_owner`. This two-step pattern
    /// (matching `DripGovernor::propose_authority`) allows a lost or
    /// compromised owner key to be rotated out without risking a mistake: the
    /// new address must prove it is live and can actually sign before the old
    /// owner is relinquished.
    ///
    /// # Errors
    ///
    /// - `NotAuthorized` — `caller` is not the current owner.
    /// - `InvalidParam` — `new_owner` is the zero address.
    pub fn propose_owner(env: Env, caller: Address, new_owner: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if is_zero_address(&env, &new_owner) {
            return Err(Error::InvalidParam);
        }
        bump_instance(&env);
        set_pending_owner(&env, &new_owner);
        set_pending_owner_proposer(&env, &caller);
        set_pending_owner_proposed_at(&env, env.ledger().timestamp());
        events::owner_proposed(&env, &caller, &new_owner);
        Ok(())
    }

    /// Accept the pending owner transfer (step 2 of 2).
    ///
    /// Must be called by the pending owner address itself. Completes the
    /// transfer: the pending owner becomes the vault owner and the previous
    /// owner (the proposer) is removed.
    ///
    /// # Errors
    ///
    /// - `NoPendingOwner` — no owner transfer has been proposed.
    /// - `NotPendingOwner` — `caller` is not the proposed pending owner.
    pub fn accept_owner(env: Env, caller: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let pending = get_pending_owner(&env).ok_or(Error::NoPendingOwner)?;
        if caller != pending {
            return Err(Error::NotPendingOwner);
        }
        caller.require_auth();
        let proposed_at = get_pending_owner_proposed_at(&env).ok_or(Error::PendingOwnerExpired)?;
        let proposal_ttl = get_owner_proposal_ttl(&env).unwrap_or(OWNER_PROPOSAL_TTL_SECONDS);
        let expires_at = proposed_at
            .checked_add(proposal_ttl)
            .ok_or(Error::ArithmeticOverflow)?;
        if env.ledger().timestamp() >= expires_at {
            return Err(Error::PendingOwnerExpired);
        }
        let proposer = get_pending_owner_proposer(&env).ok_or(Error::NoPendingOwner)?;
        bump_instance(&env);
        set_owner(&env, &caller);
        remove_pending_owner(&env);
        remove_pending_owner_proposer(&env);
        remove_pending_owner_proposed_at(&env);
        events::owner_accepted(&env, &caller, &proposer);
        Ok(())
    }

    /// Read-only: the current operator address, if any.
    pub fn operator(env: Env) -> Option<Address> {
        get_operator(&env)
    }

    /// Read-only: the maximum single-call withdrawal a delegated operator may
    /// execute before the owner raises or removes the cap.
    pub fn operator_withdraw_limit(env: Env) -> Option<i128> {
        get_operator_withdraw_limit(&env)
    }

    /// Read-only: the token contract's current balance held by this vault.
    /// Includes direct token transfers that bypass `deposit`.
    pub fn balance(env: Env) -> Option<i128> {
        vault_balance(&env).ok()
    }

    /// Read-only: the maximum token balance the vault accepts.
    pub fn max_limit(env: Env) -> Option<i128> {
        get_max_limit(&env)
    }

    /// Read-only: the maximum amount `caller` can withdraw at this moment.
    ///
    /// The owner is limited only by the vault's current token balance. The
    /// delegated operator is limited by both that balance and its per-call cap.
    /// Returns `NotAuthorized` for any address that is neither the owner nor
    /// the current operator, and `LimitExceeded` if the operator has no cap.
    pub fn effective_withdraw_limit(env: Env, caller: Address) -> Result<i128, Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        let cap = withdraw_cap(&env, &caller, &owner)?;
        Ok(vault_balance(&env)?.min(cap))
    }

    /// Read-only: the current owner address, if any.
    pub fn owner(env: Env) -> Option<Address> {
        get_owner(&env)
    }

    /// Read-only: the pending owner address for an in-flight ownership transfer,
    /// if any. Returns `None` when no transfer has been proposed.
    pub fn pending_owner(env: Env) -> Option<Address> {
        get_pending_owner(&env)
    }

    /// Read-only: the current owner who proposed the pending ownership transfer.
    /// Returns `None` when no transfer has been proposed.
    pub fn pending_owner_proposer(env: Env) -> Option<Address> {
        get_pending_owner_proposer(&env)
    }

    /// Read-only: how long a pending owner proposal remains valid, in seconds.
    pub fn owner_proposal_ttl(env: Env) -> Option<u64> {
        get_owner_proposal_ttl(&env)
    }

    /// Configure the validity period for pending and future owner proposals.
    pub fn set_owner_proposal_ttl(
        env: Env,
        caller: Address,
        ttl_seconds: u64,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if ttl_seconds == 0 {
            return Err(Error::InvalidAmount);
        }

        let old_ttl = get_owner_proposal_ttl(&env).unwrap_or(OWNER_PROPOSAL_TTL_SECONDS);
        bump_instance(&env);
        store_owner_proposal_ttl(&env, ttl_seconds);
        events::owner_proposal_ttl_set(&env, &caller, old_ttl, ttl_seconds);
        Ok(())
    }

    // ── Emergency pause (owner-gated) ─────────────────────────────────────

    /// Emergency halt: freeze all state-modifying operations.
    ///
    /// While paused, every state-modifying entrypoint reverts with
    /// `VaultPaused` before touching any state. `unpause` and the permissionless
    /// TTL keep-alive helpers remain available so a paused vault can be
    /// recovered and kept warm. Matches the `pause`/`unpause`/`is_paused` triple
    /// present on `DripFactory`, `DripGovernor`, and `TwapOracle`.
    pub fn pause(env: Env, caller: Address) -> Result<(), Error> {
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if is_paused(&env) {
            return Err(Error::AlreadyPaused);
        }
        bump_instance(&env);
        set_paused(&env, true);
        events::paused(&env, &caller, env.ledger().timestamp());
        Ok(())
    }

    /// Lift the emergency pause, re-enabling all state-mutating operations.
    pub fn unpause(env: Env, caller: Address) -> Result<(), Error> {
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if !is_paused(&env) {
            return Err(Error::NotPaused);
        }
        bump_instance(&env);
        set_paused(&env, false);
        events::unpaused(&env, &caller, env.ledger().timestamp());
        Ok(())
    }

    /// Permissionless keep-alive: extend the vault instance TTL so a paused
    /// vault can be kept warm during long investigations without requiring
    /// the owner to re-open the contract or submit a `RestoreFootprint`.
    pub fn keep_alive(env: Env) -> Result<(), Error> {
        bump_instance(&env);
        ttl::bump_all_vault_keys(&env);
        events::kept_alive(&env, TTL_EXTEND_TO);
        Ok(())
    }

    /// Read-only: whether the vault is currently under an emergency pause.
    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }

    // ── Self-upgrade (owner-gated) ──────────────────────────────────────────

    /// Replace this contract's own WASM bytecode.
    ///
    /// The new WASM must already be uploaded to the ledger (via
    /// `stellar contract upload`); only the hash is passed here. Gated on the
    /// vault owner — the same authority that can already drain the vault via
    /// `withdraw`, so an owner able to take the funds can also replace the
    /// contract. A delegated `operator` is deliberately *not* enough, matching
    /// `set_operator`'s documented scope (day-to-day ops, not ownership).
    ///
    /// Mirrors `DripGovernor::upgrade` and `TwapOracle::upgrade`, which the
    /// vault previously lacked entirely (issue #651).
    ///
    /// Blocked while the vault is paused, matching those two — a halted
    /// contract should accept no code changes.
    ///
    /// An all-zero hash is rejected: `update_current_contract_wasm` with it
    /// would replace the contract with something that cannot release escrowed
    /// funds.
    pub fn upgrade(env: Env, caller: Address, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();

        if new_wasm_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(Error::InvalidWasmHash);
        }

        bump_instance(&env);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        events::upgraded(&env, &caller, env.ledger().timestamp());
        Ok(())
    }

    // ── Multi-token vault support (Issue #724) ─────────────────────────────

    /// Deposits an arbitrary token into the vault for a specific account (Issue #724).
    pub fn deposit_token(
        env: Env,
        from: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            require_owner_or_operator(&env, &from, &owner)?;

            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }
            if amount > MAX_VAULT_DEPOSIT {
                return Err(Error::DepositExceedsMax);
            }

            if !is_token_approved(&env, &token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            let min_deposit = get_token_min_deposit(&env, &token);
            if min_deposit > 0 && amount < min_deposit {
                return Err(Error::DepositBelowMin);
            }
            let before = token::Client::new(&env, &token).balance(&env.current_contract_address());
            if !check_vault_balance(&env, &token, before) {
                return Err(Error::VaultBalanceChanged);
            }
            if let Some(max) = get_token_max_limit(&env, &token) {
                let tk_client = token::Client::new(&env, &token);
                let current_vault_balance = tk_client.balance(&env.current_contract_address());
                let expected_balance = current_vault_balance
                    .checked_add(amount)
                    .ok_or(Error::ArithmeticOverflow)?;
                if expected_balance > max {
                    return Err(Error::LimitExceeded);
                }
            }

            let tk = token::Client::new(&env, &token);
            tk.transfer(&from, &env.current_contract_address(), &amount);
            let after = tk.balance(&env.current_contract_address());
            if after
                != before
                    .checked_add(amount)
                    .ok_or(Error::ArithmeticOverflow)?
            {
                return Err(Error::DepositTransferFailed);
            }
            record_vault_balance(&env, &token, after);

            let current_acc_balance = get_token_account_balance(&env, &token, &from);
            let new_acc_balance = current_acc_balance
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            set_token_account_balance(&env, &token, &from, new_acc_balance);

            let cur_dep_bal = get_depositor_token_balance(&env, &from, &token);
            let new_dep_bal = cur_dep_bal
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            set_depositor_token_balance(&env, &from, &token, new_dep_bal);
            ttl::extend_balance_ttl(&env, &token, &from);

            bump_instance(&env);
            events::deposited(
                &env,
                &from,
                amount,
                tk.balance(&env.current_contract_address()),
            );
            events::vault_deposit(&env, &from, &token, amount);
            Ok(())
        })
    }

    /// Withdraws a specific token from the vault to a destination address (Issue #724).
    pub fn withdraw_token(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            require_owner_or_operator(&env, &caller, &owner)?;

            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }

            let cap = withdraw_cap(&env, &caller, &owner)?;
            if amount > cap {
                return Err(Error::LimitExceeded);
            }

            let tk = token::Client::new(&env, &token);
            let vault_bal = tk.balance(&env.current_contract_address());
            if amount > vault_bal {
                return Err(Error::InsufficientVaultLiquidity);
            }

            let current_acc_balance = get_token_account_balance(&env, &token, &caller);
            if caller != owner && current_acc_balance < amount {
                return Err(Error::LimitExceeded);
            }

            tk.transfer(&env.current_contract_address(), &to, &amount);

            if current_acc_balance >= amount {
                set_token_account_balance(&env, &token, &caller, current_acc_balance - amount);
            }

            let current_dep_bal = get_depositor_token_balance(&env, &caller, &token);
            if current_dep_bal >= amount {
                set_depositor_token_balance(&env, &caller, &token, current_dep_bal - amount);
            }
            ttl::extend_balance_ttl(&env, &token, &caller);

            let new_vault_bal = tk.balance(&env.current_contract_address());
            if new_vault_bal
                != vault_bal
                    .checked_sub(amount)
                    .ok_or(Error::ArithmeticOverflow)?
            {
                return Err(Error::WithdrawTransferFailed);
            }
            record_vault_balance(&env, &token, new_vault_bal);
            let by_operator = caller != owner;
            events::withdrawn(&env, &caller, &to, amount, new_vault_bal, by_operator);
            events::vault_withdrawal(
                &env,
                &caller,
                &token,
                &to,
                amount,
                vault_bal,
                new_vault_bal,
            );

            bump_instance(&env);
            Ok(())
        })
    }

    /// Read-only: balance of a specific token deposited by a user into the vault (Issue #757).
    pub fn get_depositor_token_balance(env: Env, depositor: Address, token: Address) -> i128 {
        get_depositor_token_balance(&env, &depositor, &token)
    }

    /// Extends storage TTL for an account's token balance (Issue #759).
    pub fn extend_balance_ttl(env: Env, token: Address, account: Address) {
        ttl::extend_balance_ttl(&env, &token, &account);
    }

    /// Read-only: balance of a specific token held for an account in persistent storage (Issue #724).
    pub fn get_token_account_balance(env: Env, token: Address, account: Address) -> i128 {
        get_token_account_balance(&env, &token, &account)
    }

    /// Read-only: alias for get_token_account_balance (Issue #724).
    pub fn balance_by_token(env: Env, token: Address, account: Address) -> i128 {
        get_token_account_balance(&env, &token, &account)
    }

    /// Read-only: total balance of `token` currently held by this vault (Issue #949).
    pub fn token_vault_balance(env: Env, token: Address) -> i128 {
        token::Client::new(&env, &token).balance(&env.current_contract_address())
    }

    /// Sets the maximum deposit limit for a specific token (Issue #724).
    pub fn set_token_max_limit(
        env: Env,
        caller: Address,
        token: Address,
        limit: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        require_owner_or_operator(&env, &caller, &owner)?;

        if limit <= 0 {
            return Err(Error::InvalidAmount);
        }

        set_token_max_limit(&env, &token, &limit);
        bump_instance(&env);
        Ok(())
    }

    /// Read-only: maximum deposit limit for a specific token, if set (Issue #724).
    pub fn token_max_limit(env: Env, token: Address) -> Option<i128> {
        get_token_max_limit(&env, &token)
    }

    /// Sets the minimum deposit amount threshold for an asset (Issue #756).
    pub fn set_min_deposit_amount(
        env: Env,
        caller: Address,
        token: Address,
        min_amount: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        require_owner_or_operator(&env, &caller, &owner)?;

        if min_amount < 0 {
            return Err(Error::InvalidAmount);
        }

        set_token_min_deposit(&env, &token, min_amount);
        bump_instance(&env);
        Ok(())
    }

    /// Alias for `set_min_deposit_amount` (Issue #756).
    pub fn set_token_min_deposit(
        env: Env,
        caller: Address,
        token: Address,
        min_amount: i128,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        Self::set_min_deposit_amount(env, caller, token, min_amount)
    }

    /// Read-only: minimum deposit amount threshold for an asset (Issue #756).
    pub fn min_deposit_amount(env: Env, token: Address) -> i128 {
        get_token_min_deposit(&env, &token)
    }

    /// Read-only alias for `min_deposit_amount` (Issue #756).
    pub fn get_min_deposit_amount(env: Env, token: Address) -> i128 {
        get_token_min_deposit(&env, &token)
    }

    /// Read-only alias for `min_deposit_amount` (Issue #756).
    pub fn get_token_min_deposit(env: Env, token: Address) -> i128 {
        get_token_min_deposit(&env, &token)
    }

    /// Read-only: pre-flight check validating that target token adheres to standard SEP-0041 interface (Issue #696).
    pub fn is_token_compliant(env: Env, token: Address) -> bool {
        validate_sep41_token(&env, &token).is_ok()
    }

    /// Set the dedicated emergency administrator. Owner-only (#948).
    pub fn set_emergency_admin(env: Env, caller: Address, admin: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        set_emergency_admin(&env, &admin);
        bump_instance(&env);
        Ok(())
    }

    /// Recover tokens accidentally sent directly to the vault (#948).
    ///
    /// Only the emergency administrator may call this. The function sweeps
    /// exactly the token balance above tracked account liabilities, so stream/
    /// depositor backing can never be removed through this escape hatch.
    pub fn emergency_sweep(
        env: Env,
        caller: Address,
        token: Address,
        recipient: Address,
    ) -> Result<i128, Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let admin = get_emergency_admin(&env).ok_or(Error::NotInitialized)?;
            if caller != admin {
                return Err(Error::NotAuthorized);
            }
            caller.require_auth();
            validate_sep41_token(&env, &token)?;

            let tk = token::Client::new(&env, &token);
            let actual = tk.balance(&env.current_contract_address());
            let locked = tracked_liability_for_token(&env, &token);
            let sweepable = actual
                .checked_sub(locked)
                .ok_or(Error::ArithmeticOverflow)?;
            if sweepable <= 0 {
                return Err(Error::NothingToSweep);
            }

            tk.transfer(&env.current_contract_address(), &recipient, &sweepable);
            record_vault_balance(&env, &token, actual - sweepable);
            bump_instance(&env);
            Ok(sweepable)
        })
    }

    /// Owner-maintained audited token contract allowlist (issue #812).
    /// Revocation blocks new deposits but never blocks withdrawals of existing funds.
    pub fn approve_token(
        env: Env,
        caller: Address,
        token: Address,
        approved: bool,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if approved {
            validate_sep41_token(&env, &token)?;
        }
        set_token_approved(&env, &token, approved);
        bump_instance(&env);
        events::token_approval_changed(&env, &token, approved);
        Ok(())
    }

    /// Check whether the owner has approved this exact token contract address.
    pub fn is_token_approved(env: Env, token: Address) -> bool {
        is_token_approved(&env, &token)
    }

    /// Renew every tracked, still-present persistent token balance and limit key.
    ///
    /// This is permissionless so keepers can preserve balances during long pauses.
    /// Call regularly before persistent entries expire. See issue #814.
    pub fn bump_all_vault_keys(env: Env) -> Result<u32, Error> {
        let _owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        bump_instance(&env);
        let count = ttl::bump_all_vault_keys(&env);
        events::vault_keys_refreshed(&env, count);
        Ok(count)
    }

    /// Non-reverting variant of a deposit for durable failure monitoring (#813).
    ///
    /// A reverted Soroban invocation rolls back all its events. Thus unlike
    /// `deposit`, a rejected token transfer returns `Ok(false)` and commits a
    /// VaultDepositFailed event. `Ok(true)` means the deposit actually settled.
    /// Preflight authorization, limits and invariant errors still return Err.
    pub fn deposit_with_failure_report(
        env: Env,
        from: Address,
        amount: i128,
    ) -> Result<bool, Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            require_owner_or_operator(&env, &from, &owner)?;
            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }
            if amount > MAX_VAULT_DEPOSIT {
                return Err(Error::DepositExceedsMax);
            }
            let token = get_token(&env).ok_or(Error::NotInitialized)?;
            if !is_token_approved(&env, &token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            let min_deposit = get_token_min_deposit(&env, &token);
            if min_deposit > 0 && amount < min_deposit {
                return Err(Error::DepositBelowMin);
            }
            let tk = token::Client::new(&env, &token);
            let before = tk.balance(&env.current_contract_address());
            let max = get_max_limit(&env).ok_or(Error::NotInitialized)?;
            let expected = before
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            if expected > max {
                return Err(Error::LimitExceeded);
            }
            if !check_vault_balance(&env, &token, before) {
                return Err(Error::VaultBalanceChanged);
            }
            if tk
                .try_transfer(&from, &env.current_contract_address(), &amount)
                .is_err()
            {
                events::vault_deposit_failed(&env, &from, &token, amount);
                return Ok(false);
            }
            let after = tk.balance(&env.current_contract_address());
            if after != expected {
                return Err(Error::DepositTransferFailed);
            }
            let previous = get_token_account_balance(&env, &token, &from);
            let credited = previous
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;
            set_token_account_balance(&env, &token, &from, credited);
            record_vault_balance(&env, &token, after);
            bump_instance(&env);
            events::deposited(&env, &from, amount, after);
            Ok(true)
        })
    }

    // ── Emergency Migration to cold storage (Issue #679) ──────────────────

    /// Initiates an emergency migration of the vault's assets to a cold storage destination (Issue #679).
    ///
    /// Requires owner/governor authorization. Enforces a 48-hour timelock before the migration
    /// can be executed, giving stakeholders time to detect and respond to unauthorized attempts.
    pub fn initiate_emergency_migration(
        env: Env,
        caller: Address,
        destination: Address,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if is_zero_address(&env, &destination) {
            return Err(Error::InvalidParam);
        }

        let now = env.ledger().timestamp();
        let executable_at = now
            .checked_add(EMERGENCY_MIGRATION_TIMELOCK_SECONDS)
            .ok_or(Error::ArithmeticOverflow)?;

        let migration = EmergencyMigration {
            destination: destination.clone(),
            proposed_at: now,
            executable_at,
        };

        bump_instance(&env);
        set_emergency_migration(&env, &migration);
        events::emergency_migration_initiated(&env, &caller, &destination, executable_at);
        Ok(())
    }

    /// Executes a pending emergency migration after the 48-hour timelock has elapsed (Issue #679).
    ///
    /// Transfers all vault token balance to the configured cold storage destination.
    pub fn execute_emergency_migration(env: Env, caller: Address) -> Result<i128, Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            if caller != owner {
                return Err(Error::NotAuthorized);
            }
            caller.require_auth();

            let migration = get_emergency_migration(&env).ok_or(Error::NoMigrationProposed)?;
            let now = env.ledger().timestamp();
            if now < migration.executable_at {
                return Err(Error::TimelockNotExpired);
            }

            let tk = token_client(&env)?;
            let balance = tk.balance(&env.current_contract_address());
            if balance > 0 {
                tk.transfer(
                    &env.current_contract_address(),
                    &migration.destination,
                    &balance,
                );
            }

            remove_emergency_migration(&env);
            bump_instance(&env);
            events::emergency_migration_executed(&env, &caller, &migration.destination, balance);
            Ok(balance)
        })
    }

    /// Cancels a pending emergency migration before or after it becomes executable (Issue #679).
    pub fn cancel_emergency_migration(env: Env, caller: Address) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();

        let _ = get_emergency_migration(&env).ok_or(Error::NoMigrationProposed)?;
        remove_emergency_migration(&env);
        bump_instance(&env);
        events::emergency_migration_cancelled(&env, &caller);
        Ok(())
    }

    /// Read-only: pending emergency migration details, if any (Issue #679).
    pub fn emergency_migration(env: Env) -> Option<EmergencyMigration> {
        get_emergency_migration(&env)
    }

    /// Read-only: alias for emergency_migration (Issue #679).
    pub fn get_emergency_migration(env: Env) -> Option<EmergencyMigration> {
        get_emergency_migration(&env)
    }

    // ── Yield strategy sweeping (Issue #893) ─────────────────────────────

    /// Governance-maintained allowlist of third-party strategy vaults (AMM or
    /// lending positions) that idle tokens may be swept into. Only the owner,
    /// which is expected to be the `DripGovernor` contract so that every
    /// change passes its proposal/consensus flow, may approve or revoke one.
    pub fn approve_strategy(
        env: Env,
        caller: Address,
        strategy: Address,
        approved: bool,
    ) -> Result<(), Error> {
        assert_not_paused(&env)?;
        let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
        if caller != owner {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        if approved
            && (is_zero_address(&env, &strategy) || strategy == env.current_contract_address())
        {
            return Err(Error::InvalidParam);
        }
        set_strategy_approved(&env, &strategy, approved);
        bump_instance(&env);
        events::strategy_approval_changed(&env, &strategy, approved);
        Ok(())
    }

    /// Sweeps `amount` of idle `token` from the vault into an approved
    /// `strategy_vault` so locked capital can earn yield (Issue #893).
    ///
    /// Permissioned: `caller` must be the owner (the governor, so the sweep
    /// itself needs governance consensus), the vault must not be paused, the
    /// token must be on the vault's token allowlist and the strategy on the
    /// strategy allowlist, and the vault must actually hold `amount`. The
    /// running total per (token, strategy) is recorded for accounting.
    ///
    /// Returns the new running total swept into that strategy for `token`.
    pub fn sweep_to_strategy(
        env: Env,
        caller: Address,
        token: Address,
        strategy_vault: Address,
        amount: i128,
    ) -> Result<i128, Error> {
        assert_not_paused(&env)?;
        non_reentrant!(&env, {
            let owner = get_owner(&env).ok_or(Error::NotInitialized)?;
            if caller != owner {
                return Err(Error::NotAuthorized);
            }
            caller.require_auth();

            if amount <= 0 {
                return Err(Error::InvalidAmount);
            }
            if !is_token_approved(&env, &token) {
                return Err(Error::TokenNotApprovedForVault);
            }
            if !is_strategy_approved(&env, &strategy_vault) {
                return Err(Error::StrategyNotApproved);
            }

            let vault = env.current_contract_address();
            let tk = token::Client::new(&env, &token);
            if tk.balance(&vault) < amount {
                return Err(Error::InsufficientVaultLiquidity);
            }

            let total = get_strategy_swept(&env, &token, &strategy_vault)
                .checked_add(amount)
                .ok_or(Error::ArithmeticOverflow)?;

            tk.transfer(&vault, &strategy_vault, &amount);
            set_strategy_swept(&env, &token, &strategy_vault, total);
            bump_instance(&env);
            events::swept_to_strategy(&env, &caller, &token, &strategy_vault, amount);
            Ok(total)
        })
    }

    /// Read-only: whether governance approved `strategy` as a sweep target.
    pub fn is_strategy_approved(env: Env, strategy: Address) -> bool {
        is_strategy_approved(&env, &strategy)
    }

    /// Read-only: running total of `token` swept into `strategy`.
    pub fn strategy_swept(env: Env, token: Address, strategy: Address) -> i128 {
        get_strategy_swept(&env, &token, &strategy)
    }

    /// Read-only: the emergency migration timelock duration in seconds (Issue #679).
    pub fn emergency_timelock_seconds(_env: Env) -> u64 {
        EMERGENCY_MIGRATION_TIMELOCK_SECONDS
    }
}
