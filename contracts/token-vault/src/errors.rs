use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    InvalidAmount = 1,
    ArithmeticOverflow = 2,
    LimitExceeded = 3,
    NotAuthorized = 4,
    /// Legacy pause error retained for ABI compatibility. Vault pause gates now
    /// return [`Error::VaultPaused`] so a halted vault is not reported as a
    /// balance shortfall (Issue #896).
    ContractPaused = 5,
    /// `pause` was called while the vault was already paused.
    AlreadyPaused = 6,
    /// `unpause` was called while the vault was not paused.
    NotPaused = 7,
    /// The vault has not been initialized yet.
    NotInitialized = 8,
    /// `initialize` was called on a vault that is already initialized.
    AlreadyInitialized = 9,
    /// A provided argument was invalid (e.g. proposing a zero-address owner).
    InvalidParam = 10,
    /// `accept_owner` was called but there is no pending owner to accept.
    NoPendingOwner = 11,
    /// `accept_owner` was called by an address that is not the pending owner.
    NotPendingOwner = 12,
    /// The token transfer did not move exactly the expected amount for `deposit`.
    DepositTransferFailed = 13,
    /// The token transfer did not move exactly the expected amount for `withdraw`.
    WithdrawTransferFailed = 14,
    /// The WASM hash provided to `upgrade` is all zeros (invalid).
    InvalidWasmHash = 15,
    /// `accept_owner` was called after the ownership proposal expired.
    PendingOwnerExpired = 16,
    /// Target token does not comply with the standard SEP-0041 interface (Issue #696).
    InvalidToken = 17,
    /// Timelock has not expired yet for emergency migration (Issue #679).
    TimelockNotExpired = 18,
    /// No emergency migration has been proposed (Issue #679).
    NoMigrationProposed = 19,
    /// Reentrant invocation detected during an active external token transfer.
    ReentrancyDetected = 20,
    /// A single deposit exceeded `MAX_VAULT_DEPOSIT`.
    DepositExceedsMax = 21,
    /// A stream withdrawal batch is empty, duplicated, or exceeds its cap.
    InvalidStreamBatch = 22,
    /// A stream query or withdrawal failed during batch processing.
    StreamCallFailed = 23,
    /// Target token has not been approved for vault deposits (Issue #815).
    TokenNotApprovedForVault = 24,
    /// The vault is under an emergency pause (Issue #896).
    ///
    /// Returned by every state-modifying entrypoint while paused, so frontends
    /// can tell users their funds are temporarily inaccessible rather than
    /// lost. Distinct from the balance/liquidity errors below.
    VaultPaused = 25,
    /// Vault balance changed unexpectedly within the same ledger.
    VaultBalanceChanged = 26,
    /// Vault does not hold sufficient token liquidity for withdrawal (Issue #760).
    InsufficientVaultLiquidity = 27,
    /// Deposit amount is below the configured minimum deposit amount for this asset (Issue #756).
    DepositBelowMin = 28,
    /// `sweep_to_strategy` targeted a strategy vault governance has not approved (Issue #893).
    StrategyNotApproved = 29,
    /// No untracked token excess is available for emergency recovery (#948).
    NothingToSweep = 30,
    /// The vault balance cannot cover this withdrawal (#954). Distinct from a liquidity
    /// shortfall at the token contract, which is `InsufficientVaultLiquidity`.
    InsufficientVaultBalance = 210,
    /// The caller is not an authorized vault operator (#954). Distinct from `NotAuthorized`,
    /// which covers owner-only calls.
    UnauthorizedOperator = 211,
}
