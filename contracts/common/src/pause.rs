//! Shared emergency-pause helpers for the Drip protocol contracts.
//!
//! Every contract in the protocol can be halted by its authority
//! (`pause`/`unpause`/`is_paused`) and every state-mutating entry point is
//! expected to reject the call while halted. The gate itself is the same
//! everywhere — read one instance-storage flag, and refuse to proceed when it
//! is set — but it was previously open-coded per contract, with each copy
//! naming the flag key differently and mapping the failure to its own error
//! variant. A copy that silently loses its guard is a protocol-halt failure
//! mode, so the gate lives here once.
//!
//! Contracts keep their own `Error` enum (every code means something different
//! per contract), so [`PausedError`] is mapped at each call site:
//!
//! ```ignore
//! drip_common::pause::require_not_paused(env, &DataKey::Paused)
//!     .map_err(|_| Error::ContractPaused)?;
//! ```
//!
//! The flag is *not* gated here — reading and writing it is the contract's
//! business (only its pause authority may flip it), so [`is_paused`] and
//! [`set_paused`] are low-level storage helpers like the per-contract ones they
//! replace.

use soroban_sdk::{contracttype, Env, TryFromVal, Val};

use crate::rbac::StorageKey;

/// Emergency pauses automatically expire after fourteen days unless a shorter
/// maximum duration is explicitly configured.
pub const DEFAULT_MAX_PAUSE_DURATION: u64 = 14 * 24 * 60 * 60;

/// Errors the shared pause gate can return.
///
/// Each contract maps this onto its own `Error` variant (typically
/// `ContractPaused`), so no `#[contracterror]` attribute is needed here — the
/// same approach `drip_common::rbac::RbacError` takes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PausedError {
    /// The contract is under an emergency pause, so the call is refused.
    ContractPaused,
}

/// Operation class for granular pause checks (Issue #1020).
///
/// Pass to [`require_operation_not_paused`] to enforce per-operation gate
/// without reading flags the caller does not need.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PauseOperation {
    /// New stream creation (`PauseFlags::PAUSE_CREATIONS`).
    CreateStream,
    /// Recipient withdrawal (`PauseFlags::PAUSE_WITHDRAWALS`).
    Withdraw,
    /// Deposit / fund intake (`PauseFlags::PAUSE_DEPOSITS`).
    Deposit,
}

/// Granular gate: `Ok(())` when `operation` is not blocked by any active flag.
///
/// Preserves recipient access to already-vested funds during maintenance:
///
/// ```ignore
/// // Block new streams but keep withdrawals open:
/// let flags = PauseFlags::PAUSE_CREATIONS;
/// require_operation_not_paused(flags, PauseOperation::CreateStream)?; // → Err
/// require_operation_not_paused(flags, PauseOperation::Withdraw)?;     // → Ok
/// ```
pub fn require_operation_not_paused(
    flags: PauseFlags,
    operation: PauseOperation,
) -> Result<(), PausedError> {
    let blocked = match operation {
        PauseOperation::CreateStream => flags.contains(PauseFlags::PAUSE_CREATIONS),
        PauseOperation::Withdraw => flags.contains(PauseFlags::PAUSE_WITHDRAWALS),
        PauseOperation::Deposit => flags.contains(PauseFlags::PAUSE_DEPOSITS),
    };
    if blocked {
        Err(PausedError::ContractPaused)
    } else {
        Ok(())
    }
}

/// Detailed pause state for emergency pause.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    pub paused_at: u64,
    pub max_duration: Option<u64>,
    pub unpaused_at: Option<u64>,
    pub unpause_cooldown: Option<u64>,
}

/// Reads the emergency-pause flag stored under `paused_key` in `instance()`
/// storage.
///
/// Defaults to `false` when the key was never written, so a contract deployed
/// before the pause feature existed is treated as running normally rather than
/// halting every call.
///
/// When a pause duration limit is configured, the current ledger timestamp at
/// or beyond `paused_at + max_duration` automatically clears the pause.
pub fn is_paused<K: StorageKey>(env: &Env, paused_key: &K) -> bool {
    let storage = env.storage().instance();
    let Some(raw_val) = storage.get::<K, Val>(paused_key) else {
        return false;
    };
    if let Ok(b) = bool::try_from_val(env, &raw_val) {
        if b {
            let now = env.ledger().timestamp();
            storage.set(
                paused_key,
                &PauseState {
                    paused: true,
                    paused_at: now,
                    max_duration: Some(DEFAULT_MAX_PAUSE_DURATION),
                    unpaused_at: None,
                    unpause_cooldown: None,
                },
            );
        }
        return b;
    }
    if let Ok(state) = PauseState::try_from_val(env, &raw_val) {
        if !state.paused {
            return false;
        }
        let now = env.ledger().timestamp();
        if let Some(max_dur) = state.max_duration {
            if now >= state.paused_at.saturating_add(max_dur) {
                let mut expired = state;
                expired.paused = false;
                expired.paused_at = 0;
                expired.max_duration = None;
                expired.unpaused_at = Some(now);
                storage.set(paused_key, &expired);
                return false;
            }
        }
        // A cooldown-based unpause stays paused until the scheduled timestamp.
        // `unpause_cooldown` distinguishes this scheduled state from the
        // anti-rapid-toggle case (where `paused` is already false).
        if let (Some(_), Some(unpause_at)) = (state.unpause_cooldown, state.unpaused_at) {
            if now >= unpause_at {
                let mut resumed = state;
                resumed.paused = false;
                resumed.paused_at = 0;
                resumed.max_duration = None;
                resumed.unpaused_at = Some(now);
                storage.set(paused_key, &resumed);
                return false;
            }
        }
        return true;
    }
    false
}

/// Writes the emergency-pause flag to `instance()` storage with a maximum
/// duration and optional cooldown. `None` uses [`DEFAULT_MAX_PAUSE_DURATION`].
/// Returns `Ok(())` if the pause state was set successfully.
/// Returns `Err(PausedError::ContractPaused)` if unpause is in cooldown period.
pub fn set_paused_with_duration<K: StorageKey>(
    env: &Env,
    paused_key: &K,
    paused: bool,
    max_duration: Option<u64>,
) -> Result<(), PausedError> {
    set_paused_with_cooldown(env, paused_key, paused, max_duration, None)
}

/// Writes the emergency-pause flag with duration limit and unpause cooldown period.
///
/// When `unpause_cooldown` is set, rapidly re-pausing (toggling) is prevented by requiring
/// `now >= last_unpaused_at + cooldown_seconds` before pausing again.
pub fn set_paused_with_cooldown<K: StorageKey>(
    env: &Env,
    paused_key: &K,
    paused: bool,
    max_duration: Option<u64>,
    unpause_cooldown: Option<u64>,
) -> Result<(), PausedError> {
    let now = env.ledger().timestamp();
    let current_state: Option<PauseState> = env.storage().instance().get(paused_key);

    // Check unpause cooldown if pausing and previous unpaused_at is recent
    if paused {
        if let Some(current) = &current_state {
            if !current.paused {
                // We're currently unpaused; check if we're still in cooldown
                if let Some(last_unpaused) = current.unpaused_at {
                    if let Some(cooldown) = unpause_cooldown {
                        if now <= last_unpaused.saturating_add(cooldown) {
                            return Err(PausedError::ContractPaused);
                        }
                    }
                }
            }
        }
    }

    let state = PauseState {
        paused,
        paused_at: if paused { now } else { 0 },
        max_duration: if paused {
            Some(
                max_duration
                    .unwrap_or(DEFAULT_MAX_PAUSE_DURATION)
                    .min(DEFAULT_MAX_PAUSE_DURATION),
            )
        } else {
            None
        },
        unpaused_at: if !paused && current_state.is_some() {
            Some(now)
        } else {
            None
        },
        unpause_cooldown,
    };
    env.storage().instance().set(paused_key, &state);
    Ok(())
}

/// Schedules an unpause after `cooldown_seconds`, keeping the contract halted
/// for the duration of the cooldown.
///
/// This is the gradual-unpause path: instead of restoring every entry point at
/// once, an operator can request an unpause and let the clock run out while the
/// gate still rejects state-mutating calls. The scheduled timestamp is written
/// into the existing pause state; [`is_paused`] / [`require_not_paused`] clear
/// it automatically once the cooldown has elapsed, so no separate keeper call
/// is needed.
///
/// A zero cooldown unpauses immediately (the same result as
/// [`set_paused`] with `false`). Calling this while the contract is already
/// running is a no-op. The caller still owns authorization and event emission.
pub fn unpause_with_cooldown<K: StorageKey>(
    env: &Env,
    paused_key: &K,
    cooldown_seconds: u64,
) -> Result<(), PausedError> {
    let Some(mut state) = get_pause_state(env, paused_key) else {
        return Ok(());
    };
    if !state.paused {
        return Ok(());
    }

    let now = env.ledger().timestamp();
    if cooldown_seconds == 0 {
        state.paused = false;
        state.paused_at = 0;
        state.max_duration = None;
        state.unpaused_at = Some(now);
        state.unpause_cooldown = None;
    } else {
        // Keep `paused = true` and record the target timestamp. `is_paused`
        // treats this pair as a pending unpause rather than an anti-rapid-
        // toggle marker (which is only stored while `paused` is false).
        state.unpaused_at = Some(now.saturating_add(cooldown_seconds));
        state.unpause_cooldown = Some(cooldown_seconds);
    }

    env.storage().instance().set(paused_key, &state);
    Ok(())
}

/// Writes the emergency-pause flag to `instance()` storage using the default
/// maximum pause duration.
///
/// Performs no authorization and no state-transition validation: the caller
/// (the contract's own `pause`/`unpause`) owns both, and is responsible for
/// emitting the matching event and bumping TTL.
pub fn set_paused<K: StorageKey>(
    env: &Env,
    paused_key: &K,
    paused: bool,
) -> Result<(), PausedError> {
    set_paused_with_duration(env, paused_key, paused, None)
}

/// Returns the current `PauseState` stored under `paused_key`, if any.
pub fn get_pause_state<K: StorageKey>(env: &Env, paused_key: &K) -> Option<PauseState> {
    let storage = env.storage().instance();
    let raw_val = storage.get::<K, Val>(paused_key)?;
    if PauseState::try_from_val(env, &raw_val).is_ok() {
        let _ = is_paused(env, paused_key);
        storage.get(paused_key)
    } else if let Ok(b) = bool::try_from_val(env, &raw_val) {
        if b {
            let _ = is_paused(env, paused_key);
            storage.get(paused_key)
        } else {
            Some(PauseState {
                paused: false,
                paused_at: 0,
                max_duration: None,
                unpaused_at: None,
                unpause_cooldown: None,
            })
        }
    } else {
        None
    }
}

/// The pause gate: `Ok(())` when the contract is running, `Err` while halted.
///
/// Place it at the top of a state-mutating entry point — before validation,
/// storage reads, and TTL payment — so a halted protocol rejects the call
/// immediately and cheaply, and so no state is touched on the rejected path.
pub fn require_not_paused<K: StorageKey>(env: &Env, paused_key: &K) -> Result<(), PausedError> {
    require_not_paused_flag(is_paused(env, paused_key))
}

/// [`require_not_paused`] for a flag the caller already has in memory — e.g. a
/// field inside a stored struct rather than a standalone instance key
/// (`DripStream` keeps its pause bit in the consolidated `StreamInfo`).
///
/// Accepting the flag instead of re-reading storage keeps the gate free to use
/// where the contract has already loaded the record it needs to gate on.
pub fn require_not_paused_flag(paused: bool) -> Result<(), PausedError> {
    if paused {
        Err(PausedError::ContractPaused)
    } else {
        Ok(())
    }
}

/// Granular pause flags for the protocol's three protected operation classes
/// (Issue #975).
///
/// A bitfield so each class can be halted independently of the others:
///
/// - [`PauseFlags::PAUSE_DEPOSITS`] blocks intake of new funds (deposit-bearing
///   stream creation, and top-ups once the stream side is wired to it).
/// - [`PauseFlags::PAUSE_WITHDRAWALS`] blocks recipient withdrawals.
/// - [`PauseFlags::PAUSE_CREATIONS`] blocks deployment of new stream contracts.
///
/// Cancellations are deliberately not covered by any flag: a cancel returns
/// the recipient's vested share and the sender's refund, so gating it would
/// trap funds in exactly the way the blanket pause does today. See `NOTES.md`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PauseFlags(u32);

impl PauseFlags {
    /// Blocks deposit intake.
    pub const PAUSE_DEPOSITS: Self = Self(1 << 0);
    /// Blocks withdrawals; deposits and creations are unaffected by this bit.
    pub const PAUSE_WITHDRAWALS: Self = Self(1 << 1);
    /// Blocks deployment of new stream contracts.
    pub const PAUSE_CREATIONS: Self = Self(1 << 2);
    /// Every defined flag set together.
    pub const ALL: Self = Self(Self::PAUSE_DEPOSITS.0 | Self::PAUSE_WITHDRAWALS.0 | Self::PAUSE_CREATIONS.0);

    /// No flags set: nothing is paused.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw bit representation, as stored and exposed on-chain.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Builds flags from raw bits. Returns `None` when any bit outside
    /// [`PauseFlags::ALL`] is set, so a typo cannot silently pause nothing.
    pub fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Union of two flag sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// True when every flag in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// True when at least one flag in `other` is set in `self`.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// True when no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::{contract, contractimpl, contracttype};

    #[contract]
    struct PauseTestHost;

    #[contractimpl]
    impl PauseTestHost {
        pub fn noop(_env: Env) {}
    }

    fn in_contract<T>(env: &Env, f: impl FnOnce() -> T) -> T {
        env.as_contract(&env.register_contract(None, PauseTestHost), f)
    }

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum TestKey {
        Paused,
        StreamAddr(u64),
    }

    const PAUSED: TestKey = TestKey::Paused;

    #[test]
    fn unset_flag_reads_as_not_paused() {
        let env = Env::default();

        in_contract(&env, || {
            assert!(!is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn set_paused_round_trips_and_clears() {
        let env = Env::default();

        in_contract(&env, || {
            assert!(set_paused(&env, &PAUSED, true).is_ok());
            assert!(is_paused(&env, &PAUSED));

            assert!(set_paused(&env, &PAUSED, false).is_ok());
            assert!(!is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn require_not_paused_accepts_a_running_contract() {
        let env = Env::default();

        in_contract(&env, || {
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));
        });
    }

    #[test]
    fn require_not_paused_rejects_a_halted_contract() {
        let env = Env::default();

        in_contract(&env, || {
            set_paused(&env, &PAUSED, true).ok();
            assert_eq!(
                require_not_paused(&env, &PAUSED),
                Err(PausedError::ContractPaused)
            );
        });
    }

    #[test]
    fn flag_based_gate_matches_the_key_based_gate() {
        let env = Env::default();

        in_contract(&env, || {
            for paused in [false, true] {
                set_paused(&env, &PAUSED, paused).ok();
                assert_eq!(
                    require_not_paused(&env, &PAUSED),
                    require_not_paused_flag(paused)
                );
            }
        });
    }

    #[test]
    fn gate_is_generic_over_the_flag_key() {
        let env = Env::default();
        let stream_key = TestKey::StreamAddr(7);

        in_contract(&env, || {
            assert_eq!(require_not_paused(&env, &stream_key), Ok(()));

            set_paused(&env, &stream_key, true).ok();
            assert_eq!(
                require_not_paused(&env, &stream_key),
                Err(PausedError::ContractPaused)
            );

            assert!(!is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn pause_with_duration_automatically_unpauses_after_expiry() {
        let env = Env::default();
        let max_duration = 3600;

        in_contract(&env, || {
            env.ledger().set_timestamp(1000);

            assert!(set_paused_with_duration(&env, &PAUSED, true, Some(max_duration)).is_ok());
            assert!(is_paused(&env, &PAUSED));
            assert_eq!(
                require_not_paused(&env, &PAUSED),
                Err(PausedError::ContractPaused)
            );

            let state = get_pause_state(&env, &PAUSED).unwrap();
            assert!(state.paused);
            assert_eq!(state.paused_at, 1000);
            assert_eq!(state.max_duration, Some(max_duration));

            env.ledger().set_timestamp(4600);
            assert!(!is_paused(&env, &PAUSED));
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));
            let state = get_pause_state(&env, &PAUSED).unwrap();
            assert!(!state.paused);
            assert_eq!(state.unpaused_at, Some(4600));
        });
    }

    #[test]
    fn pause_without_explicit_duration_uses_fourteen_day_default() {
        let env = Env::default();

        in_contract(&env, || {
            env.ledger().set_timestamp(1_000);
            assert!(set_paused(&env, &PAUSED, true).is_ok());
            let state = get_pause_state(&env, &PAUSED).unwrap();
            assert_eq!(state.max_duration, Some(DEFAULT_MAX_PAUSE_DURATION));

            env.ledger()
                .set_timestamp(1_000 + DEFAULT_MAX_PAUSE_DURATION - 1);
            assert!(is_paused(&env, &PAUSED));
            env.ledger()
                .set_timestamp(1_000 + DEFAULT_MAX_PAUSE_DURATION);
            assert!(!is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn unpause_clears_duration_and_paused_state() {
        let env = Env::default();

        in_contract(&env, || {
            env.ledger().set_timestamp(1000);
            assert!(set_paused_with_duration(&env, &PAUSED, true, Some(60)).is_ok());
            assert!(is_paused(&env, &PAUSED));

            assert!(set_paused(&env, &PAUSED, false).is_ok());
            assert!(!is_paused(&env, &PAUSED));
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));

            let state = get_pause_state(&env, &PAUSED).unwrap();
            assert!(!state.paused);
            assert_eq!(state.paused_at, 0);
            assert_eq!(state.max_duration, None);
        });
    }

    #[test]
    fn unpause_cooldown_prevents_rapid_re_pause() {
        let env = Env::default();
        let cooldown = 300;

        in_contract(&env, || {
            env.ledger().set_timestamp(1000);

            assert!(set_paused_with_cooldown(&env, &PAUSED, true, None, Some(cooldown)).is_ok());
            assert!(is_paused(&env, &PAUSED));

            env.ledger().set_timestamp(2000);
            assert!(set_paused_with_cooldown(&env, &PAUSED, false, None, Some(cooldown)).is_ok());
            assert!(!is_paused(&env, &PAUSED));

            env.ledger().set_timestamp(2100);
            let result = set_paused_with_cooldown(&env, &PAUSED, true, None, Some(cooldown));
            assert_eq!(result, Err(PausedError::ContractPaused));
            assert!(!is_paused(&env, &PAUSED));

            env.ledger().set_timestamp(2300);
            let result = set_paused_with_cooldown(&env, &PAUSED, true, None, Some(cooldown));
            assert_eq!(result, Err(PausedError::ContractPaused));

            env.ledger().set_timestamp(2301);
            assert!(set_paused_with_cooldown(&env, &PAUSED, true, None, Some(cooldown)).is_ok());
            assert!(is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn unpause_with_cooldown_keeps_the_gate_closed_until_the_timestamp() {
        let env = Env::default();
        let cooldown = 300;

        in_contract(&env, || {
            env.ledger().set_timestamp(1_000);
            set_paused(&env, &PAUSED, true).ok();

            assert!(unpause_with_cooldown(&env, &PAUSED, cooldown).is_ok());
            assert!(is_paused(&env, &PAUSED));
            assert_eq!(
                require_not_paused(&env, &PAUSED),
                Err(PausedError::ContractPaused)
            );

            env.ledger().set_timestamp(1_299);
            assert!(is_paused(&env, &PAUSED));

            env.ledger().set_timestamp(1_300);
            assert!(!is_paused(&env, &PAUSED));
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));
        });
    }

    #[test]
    fn zero_cooldown_unpauses_immediately() {
        let env = Env::default();

        in_contract(&env, || {
            env.ledger().set_timestamp(1_000);
            set_paused(&env, &PAUSED, true).ok();

            assert!(unpause_with_cooldown(&env, &PAUSED, 0).is_ok());
            assert!(!is_paused(&env, &PAUSED));
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));
        });
    }

    #[test]
    fn unpause_with_cooldown_is_a_no_op_when_running() {
        let env = Env::default();

        in_contract(&env, || {
            assert!(unpause_with_cooldown(&env, &PAUSED, 300).is_ok());
            assert!(!is_paused(&env, &PAUSED));
        });
    }

    #[test]
    fn legacy_boolean_storage_compatibility() {
        let env = Env::default();

        in_contract(&env, || {
            env.storage().instance().set(&PAUSED, &true);
            assert!(is_paused(&env, &PAUSED));
            assert_eq!(
                get_pause_state(&env, &PAUSED)
                    .unwrap()
                    .max_duration,
                Some(DEFAULT_MAX_PAUSE_DURATION)
            );
            assert_eq!(
                require_not_paused(&env, &PAUSED),
                Err(PausedError::ContractPaused)
            );

            env.ledger()
                .set_timestamp(DEFAULT_MAX_PAUSE_DURATION);
            assert!(!is_paused(&env, &PAUSED));

            env.storage().instance().set(&PAUSED, &false);
            assert!(!is_paused(&env, &PAUSED));
            assert_eq!(require_not_paused(&env, &PAUSED), Ok(()));
        });
    }
}

#[cfg(test)]
mod pause_flag_tests {
    use super::PauseFlags;

    #[test]
    fn flags_are_distinct_single_bits_and_all_covers_them() {
        let mut seen = 0u32;
        for f in [
            PauseFlags::PAUSE_DEPOSITS,
            PauseFlags::PAUSE_WITHDRAWALS,
            PauseFlags::PAUSE_CREATIONS,
        ] {
            assert_eq!(f.bits().count_ones(), 1);
            assert_eq!(seen & f.bits(), 0);
            seen |= f.bits();
        }
        assert_eq!(PauseFlags::ALL.bits(), seen);
    }

    #[test]
    fn default_and_empty_are_nothing_paused() {
        assert!(PauseFlags::default().is_empty());
        assert_eq!(PauseFlags::default(), PauseFlags::empty());
    }

    #[test]
    fn from_bits_rejects_unknown_bits() {
        assert_eq!(PauseFlags::from_bits(0), Some(PauseFlags::empty()));
        assert_eq!(
            PauseFlags::from_bits(PauseFlags::ALL.bits()),
            Some(PauseFlags::ALL)
        );
        assert_eq!(PauseFlags::from_bits(1 << 31), None);
        assert_eq!(PauseFlags::from_bits(PauseFlags::ALL.bits() | 1 << 3), None);
    }

    #[test]
    fn union_contains_and_intersects_follow_bitwise_semantics() {
        let dep = PauseFlags::PAUSE_DEPOSITS;
        let wd = PauseFlags::PAUSE_WITHDRAWALS;
        let both = dep.union(wd);
        assert!(both.contains(dep));
        assert!(both.contains(wd));
        assert!(!dep.contains(wd));
        assert!(both.intersects(wd));
        assert!(!dep.intersects(wd));
        assert!(!PauseFlags::empty().intersects(PauseFlags::ALL));
    }

    #[test]
    fn operation_pause_allows_withdrawals_while_creations_are_blocked() {
        // Issue #1020: granular pause — block new streams but keep withdrawals open.
        let flags = PauseFlags::PAUSE_CREATIONS;
        assert_eq!(
            require_operation_not_paused(flags, PauseOperation::CreateStream),
            Err(PausedError::ContractPaused),
        );
        assert_eq!(
            require_operation_not_paused(flags, PauseOperation::Withdraw),
            Ok(()),
        );
        assert_eq!(
            require_operation_not_paused(flags, PauseOperation::Deposit),
            Ok(()),
        );
    }

    #[test]
    fn all_flags_blocks_every_operation() {
        for op in [
            PauseOperation::CreateStream,
            PauseOperation::Withdraw,
            PauseOperation::Deposit,
        ] {
            assert_eq!(
                require_operation_not_paused(PauseFlags::ALL, op),
                Err(PausedError::ContractPaused),
            );
        }
    }

    #[test]
    fn empty_flags_allows_every_operation() {
        for op in [
            PauseOperation::CreateStream,
            PauseOperation::Withdraw,
            PauseOperation::Deposit,
        ] {
            assert_eq!(
                require_operation_not_paused(PauseFlags::empty(), op),
                Ok(()),
            );
        }
    }
}
 