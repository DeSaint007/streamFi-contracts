use soroban_sdk::{panic_with_error, symbol_short, Address, Env, Symbol};

use crate::Error;

/// Allocate the next event sequence before publishing its payload.
///
/// The sequence is part of the persisted `StreamInfo`/`Config` payload,
/// persisting through contract operations and event delivery tracking.
///
/// Boundary check: `current` is validated to prevent arithmetic overflow
/// on the sequence counter (which would silently consume future events).
fn next_sequence(env: &Env) -> u64 {
    let current = crate::state::load(env).event_sequence;
    let Some(next) = current.checked_add(1) else {
        panic_with_error!(env, Error::ArithmeticOverflow);
    };

    let mut info = crate::state::load(env);
    info.event_sequence = next;
    crate::state::save(env, &info);
    next
}

/// Validate that an `i128` amount is non-negative. A negative value is always
/// invalid for a payout, withdrawal, or balance field and indicates a corrupted
/// or unexpected state. Panics with `Error::InvalidAmount` so the caller
/// cannot silently emit a malformed event.
fn assert_non_negative_amount(env: &Env, value: i128) {
    if value < 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn created(
    env: &Env,
    sender: &Address,
    recipient: &Address,
    token: &Address,
    rate_per_second: i128,
    start_time: u64,
    end_time: u64,
    storage_version: u32,
) {
    assert_non_negative_amount(env, rate_per_second);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("created"), sender.clone(), sequence),
        (
            recipient.clone(),
            token.clone(),
            rate_per_second,
            start_time,
            end_time,
            storage_version,
        ),
    );
}

/// Publish a `withdrawn` event for a completed withdrawal.
///
/// `amount` is what the recipient received this call, `total_withdrawn` is the
/// cumulative amount withdrawn across the stream's life, and `remaining` is the
/// escrow balance left after the transfer. All three are amount fields, so each
/// must be non-negative before the event is emitted; a negative value indicates
/// a corrupted or unexpected state and is rejected rather than published.
pub fn withdrawn(
    env: &Env,
    recipient: &Address,
    amount: i128,
    total_withdrawn: i128,
    remaining: i128,
) {
    // Boundary checks before any state mutation or event emission.
    assert_non_negative_amount(env, amount);
    assert_non_negative_amount(env, total_withdrawn);
    assert_non_negative_amount(env, remaining);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("withdrawn"), recipient.clone(), sequence),
        (amount, total_withdrawn, remaining),
    );
}

pub fn cancelled(
    env: &Env,
    sender: &Address,
    refund_amount: i128,
    withdrawn_so_far: i128,
    remaining_deposit: i128,
) {
    assert_non_negative_amount(env, refund_amount);
    assert_non_negative_amount(env, withdrawn_so_far);
    assert_non_negative_amount(env, remaining_deposit);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("cancelled"), sender.clone(), sequence),
        (refund_amount, withdrawn_so_far, remaining_deposit),
    );
}

/// Recipient-initiated cancellation via `force_cancel`, after the sender left
/// the stream paused past the threshold. Distinct from `cancelled` (the
/// sender/operator-initiated path via `cancel`) so consumers of the event log
/// can tell the two apart without correlating who signed the transaction.
pub fn force_cancelled(
    env: &Env,
    sender: &Address,
    refund_amount: i128,
    withdrawn_so_far: i128,
    remaining_deposit: i128,
) {
    assert_non_negative_amount(env, refund_amount);
    assert_non_negative_amount(env, withdrawn_so_far);
    assert_non_negative_amount(env, remaining_deposit);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("force_cxl"), sender.clone(), sequence),
        (refund_amount, withdrawn_so_far, remaining_deposit),
    );
}

pub fn paused(env: &Env, initiator: &Address, paused_at: u64, withdrawable: i128) {
    assert_non_negative_amount(env, withdrawable);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("paused"), initiator.clone(), sequence),
        (paused_at, withdrawable),
    );
}

/// Emitted when a stream is paused specifically by the recipient (Issue #754).
///
/// Topics: `("StreamPausedByRecipient", recipient)`
/// Data:   `(paused_at, withdrawable)`
pub fn stream_paused_by_recipient(
    env: &Env,
    recipient: &Address,
    paused_at: u64,
    withdrawable: i128,
) {
    assert_non_negative_amount(env, withdrawable);
    env.events().publish(
        (Symbol::new(env, "StreamPausedByRecipient"), recipient.clone()),
        (paused_at, withdrawable),
    );
}

pub fn resumed(env: &Env, initiator: &Address, resumed_at: u64) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("resumed"), initiator.clone(), sequence),
        resumed_at,
    );
}

pub fn topped_up(env: &Env, sender: &Address, amount: i128, new_balance: i128) {
    assert_non_negative_amount(env, amount);
    assert_non_negative_amount(env, new_balance);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("topped_up"), sender.clone(), sequence),
        (amount, new_balance),
    );
}

/// Publish a single event for the atomic `top_up_and_extend` transition.
///
/// A dedicated event prevents indexers from having to correlate independent
/// top-up and duration-extension records from the same transaction.
pub fn topped_up_and_extended(
    env: &Env,
    caller: &Address,
    old_balance: i128,
    new_balance: i128,
    old_end_time: u64,
    new_end_time: u64,
) {
    assert_non_negative_amount(env, old_balance);
    assert_non_negative_amount(env, new_balance);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("top_ext"), caller.clone(), sequence),
        (old_balance, new_balance, old_end_time, new_end_time),
    );
}

/// Publish the schedule extension made by `top_up_and_extend` (#881), so
/// indexers can tell a term prolongation apart from a plain principal top-up.
pub fn top_up_extended(
    env: &Env,
    caller: &Address,
    old_end_time: u64,
    new_end_time: u64,
    added_amount: i128,
    new_total_deposit: i128,
) {
    assert_non_negative_amount(env, added_amount);
    assert_non_negative_amount(env, new_total_deposit);

    let sequence = next_sequence(env);
    env.events().publish(
        (
            Symbol::new(env, "StreamTopUpExtended"),
            caller.clone(),
            sequence,
        ),
        (old_end_time, new_end_time, added_amount, new_total_deposit),
    );
}

/// Recipient granted `delegate` the right to call `claim_for` (#878).
pub fn claim_delegate_set(env: &Env, recipient: &Address, delegate: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("claim_dlg"), recipient.clone(), sequence),
        delegate.clone(),
    );
}

/// Recipient revoked its claim delegate (#878).
pub fn claim_delegate_revoked(env: &Env, recipient: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("rm_cl_dlg"), recipient.clone(), sequence),
        (),
    );
}

/// A delegate claimed `amount` for `recipient` through `claim_for` (#878).
pub fn claimed_for(env: &Env, delegate: &Address, recipient: &Address, amount: i128) {
    assert_non_negative_amount(env, amount);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("claim_for"), delegate.clone(), sequence),
        (recipient.clone(), amount),
    );
}

/// Publish a prospective stream-rate change.
pub fn rate_changed(env: &Env, caller: &Address, old_rate: i128, new_rate: i128) {
    assert_non_negative_amount(env, old_rate);
    assert_non_negative_amount(env, new_rate);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("rate_chg"), caller.clone(), sequence),
        (old_rate, new_rate),
    );
}

pub fn clawback(env: &Env, sender: &Address, amount: i128) {
    assert_non_negative_amount(env, amount);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("clawback"), sender.clone(), sequence),
        amount,
    );
}

pub fn recipient_transferred(env: &Env, previous_recipient: &Address, new_recipient: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (
            Symbol::new(env, "StreamRecipientTransferred"),
            previous_recipient.clone(),
            sequence,
        ),
        (previous_recipient.clone(), new_recipient.clone()),
    );
    env.events().publish(
        (symbol_short!("xfer_rec"), previous_recipient.clone(), sequence),
        (previous_recipient.clone(), new_recipient.clone()),
    );
}

pub fn recipient_transfer_proposed(
    env: &Env,
    current_recipient: &Address,
    new_recipient: &Address,
) {
    let sequence = next_sequence(env);
    env.events().publish(
        (
            Symbol::new(env, "StreamRecipientTransferProposed"),
            current_recipient.clone(),
            sequence,
        ),
        (current_recipient.clone(), new_recipient.clone()),
    );
    env.events().publish(
        (
            symbol_short!("prop_rec"),
            current_recipient.clone(),
            sequence,
        ),
        (current_recipient.clone(), new_recipient.clone()),
    );
}

pub fn operator_set(env: &Env, sender: &Address, operator: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (
            symbol_short!("set_op"),
            sender.clone(),
            operator.clone(),
            sequence,
        ),
        operator.clone(),
    );
}

pub fn operator_revoked(env: &Env, sender: &Address) {
    let sequence = next_sequence(env);
    env.events()
        .publish((symbol_short!("rm_op"), sender.clone(), sequence), ());
}

pub fn split_recipient_set(
    env: &Env,
    caller: &Address,
    secondary_recipient: &Address,
    split_bps: u32,
) {
    let sequence = next_sequence(env);
    env.events().publish(
        (
            symbol_short!("set_split"),
            caller.clone(),
            secondary_recipient.clone(),
            sequence,
        ),
        split_bps,
    );
}

pub fn split_recipient_removed(env: &Env, caller: &Address) {
    let sequence = next_sequence(env);
    env.events()
        .publish((symbol_short!("rm_split"), caller.clone(), sequence), ());
}

pub fn cliff_configured(env: &Env, sender: &Address, cliff_time: u64, cliff_unlock_amount: i128) {
    assert_non_negative_amount(env, cliff_unlock_amount);
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("set_cliff"), sender.clone(), sequence),
        (cliff_time, cliff_unlock_amount),
    );
}

pub fn min_withdrawal_interval_set(env: &Env, sender: &Address, interval_seconds: u64) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("min_w_int"), sender.clone(), sequence),
        interval_seconds,
    );
}

pub fn keeper_ping(env: &Env, keeper: &Address, bounty_amount: i128) {
    assert_non_negative_amount(env, bounty_amount);

    let sequence = next_sequence(env);
    env.events().publish(
        (
            Symbol::new(env, "StreamKeeperPing"),
            keeper.clone(),
            sequence,
        ),
        bounty_amount,
    );
}

pub fn keeper_reward_pool_configured(
    env: &Env,
    caller: &Address,
    reward_token: &Address,
    reward_per_ping: i128,
) {
    assert_non_negative_amount(env, reward_per_ping);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("keepercfg"), caller.clone(), sequence),
        (reward_token.clone(), reward_per_ping),
    );
}

pub fn keeper_reward_pool_funded(
    env: &Env,
    funder: &Address,
    reward_token: &Address,
    amount: i128,
) {
    assert_non_negative_amount(env, amount);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("keeperfnd"), funder.clone(), sequence),
        (reward_token.clone(), amount),
    );
}

pub fn keeper_reward_pool_withdrawn(
    env: &Env,
    caller: &Address,
    reward_token: &Address,
    amount: i128,
) {
    assert_non_negative_amount(env, amount);

    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("keeper_wd"), caller.clone(), sequence),
        (reward_token.clone(), amount),
    );
}

pub fn emergency_paused(env: &Env, caller: &Address, timestamp: u64) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("emg_pause"), caller.clone(), sequence),
        timestamp,
    );
}

pub fn emergency_unpaused(env: &Env, caller: &Address, timestamp: u64) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("emg_unps"), caller.clone(), sequence),
        timestamp,
    );
}

/// Publish a milestone event when a cliff timestamp has been reached and passed (Issue #676).
pub fn cliff_reached(env: &Env, recipient: &Address, timestamp: u64, unlock_amount: i128) {
    assert_non_negative_amount(env, unlock_amount);
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("cliff_rch"), recipient.clone(), sequence),
        (timestamp, unlock_amount),
    );
}

/// Emitted upon the first withdrawal after cliff timestamp (Issue #752).
///
/// Topics: `("StreamCliffReached", recipient)`
/// Data:   `(timestamp, unlock_amount)`
pub fn stream_cliff_reached(env: &Env, recipient: &Address, timestamp: u64, unlock_amount: i128) {
    assert_non_negative_amount(env, unlock_amount);
    env.events().publish(
        (Symbol::new(env, "StreamCliffReached"), recipient.clone()),
        (timestamp, unlock_amount),
    );
}

/// Publish a milestone event when a stream naturally runs to completion and is fully settled to zero remaining balance (Issue #676).
pub fn stream_completed(env: &Env, recipient: &Address, total_withdrawn: i128) {
    assert_non_negative_amount(env, total_withdrawn);
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("completed"), recipient.clone(), sequence),
        total_withdrawn,
    );
}

/// Emitted when a stream enters the paused state (#935).
///
/// Topics: `("StreamPaused", stream_id)` where `stream_id` is this contract's address.
/// Data:   `paused_at`
pub fn stream_paused(env: &Env, paused_at: u64) {
    env.events().publish(
        (Symbol::new(env, "StreamPaused"), env.current_contract_address()),
        paused_at,
    );
}

/// Emitted when a paused stream is resumed (#935).
///
/// Topics: `("StreamResumed", stream_id)` where `stream_id` is this contract's address.
/// Data:   `resumed_at`
pub fn stream_resumed(env: &Env, resumed_at: u64) {
    env.events().publish(
        (Symbol::new(env, "StreamResumed"), env.current_contract_address()),
        resumed_at,
    );
}

/// Emitted when the sender approves a third-party funder (#933).
pub fn funder_approved(env: &Env, sender: &Address, funder: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("fund_appr"), sender.clone(), sequence),
        funder.clone(),
    );
}

/// Emitted when the sender revokes a third-party funder approval (#933).
pub fn funder_revoked(env: &Env, sender: &Address, funder: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("fund_rvk"), sender.clone(), sequence),
        funder.clone(),
    );
}

/// Emitted when stream metadata or cancellation permissions are modified.
///
/// Topics: `("StreamConfigUpdated", caller, sequence)`
/// Data:   `(old_rate_per_second, new_rate_per_second, old_end_time, new_end_time, old_cancel_delegated, new_cancel_delegated)`
///
/// Provides detailed telemetry for off-chain indexing observability on stream parameter changes.
pub fn stream_config_updated(
    env: &Env,
    caller: &Address,
    old_rate_per_second: i128,
    new_rate_per_second: i128,
    old_end_time: u64,
    new_end_time: u64,
    old_cancel_delegated: bool,
    new_cancel_delegated: bool,
) {
    assert_non_negative_amount(env, old_rate_per_second);
    assert_non_negative_amount(env, new_rate_per_second);

    let sequence = next_sequence(env);
    env.events().publish(
        (
            Symbol::new(env, "StreamConfigUpdated"),
            caller.clone(),
            sequence,
        ),
        (
            old_rate_per_second,
            new_rate_per_second,
            old_end_time,
            new_end_time,
            old_cancel_delegated,
            new_cancel_delegated,
        ),
    );
}
