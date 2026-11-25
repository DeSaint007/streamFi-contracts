use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotAuthorized = 1,
    StreamNotFound = 2,
    StreamCancelled = 3,
    StreamNotStarted = 4,
    StreamEnded = 5,
    NothingToWithdraw = 6,
    InsufficientDeposit = 7,
    InvalidTimeRange = 8,
    AlreadyPaused = 9,
    NotPaused = 10,
    ClawbackDisabled = 11,
    ArithmeticOverflow = 12,
    PauseThresholdNotMet = 13,
    AlreadyInitialized = 14,
    InvalidAmount = 15,
    ReentrancyForbidden = 16,
    OperatorAlreadySet = 17,
    NotInitialized = 18,
    /// The recipient is invalid (e.g. the all-zero Stellar account address, or identical to `sender`).
    InvalidRecipient = 19,
    /// The stream's `start_time` is in the past at initialization.
    BackdatedStream = 20,
    /// The stream has accrued tokens but is not funded enough to cover the requested withdrawal.
    StreamUnderfunded = 21,
    /// The requested withdrawal violates the minimum interval rate limit.
    WithdrawalTooFrequent = 22,
    /// The stream is under an emergency administrative halt (Issue #699).
    ContractPaused = 23,
    /// The stream is non-cancelable (Issue #685).
    StreamNotCancelable = 24,
    /// The stream cannot be created with a zero rate per second.
    ZeroRateStreamNotAllowed = 25,
    /// A bounded stream is shorter than `MIN_STREAM_DURATION`.
    StreamDurationTooShort = 26,
    /// The optional human-readable description exceeds 128 bytes.
    DescriptionTooLong = 27,
    /// The keeper reward token must be valid and distinct from the stream token.
    InvalidKeeperRewardToken = 28,
    /// No keeper reward pool has been configured for this stream.
    KeeperRewardPoolNotConfigured = 29,
    /// The keeper reward pool cannot cover the configured bounty.
    InsufficientKeeperRewardPool = 30,
    /// Keeper reimbursement is not yet due for this stream.
    KeeperPingTooFrequent = 31,
    /// Keeper reward transfer did not move the expected balance.
    KeeperRewardTransferFailed = 32,
    /// Attempted to withdraw before the cliff timestamp has been reached (Issue #806).
    StreamCliffInFuture = 33,
    /// Attempted to interact with a stream that has already completed and drained to zero balance (Issue #751).
    StreamAlreadyCompleted = 34,
    /// The optional creation-time metadata URI exceeds 128 bytes (Issue #923).
    MetadataUriTooLong = 35,
    /// Cliff timestamp is before `start_time` or after `end_time` (Issue #884).
    InvalidCliffDuration = 36,
    /// `pause` was called on a stream that is already paused (#946).
    StreamAlreadyPaused = 120,
    /// `resume` was called on a stream that is not paused (#946).
    StreamNotPaused = 121,
    /// The caller is not an approved funder for this stream (#933).
    FunderNotApproved = 122,
    /// A cliff timestamp is before stream inception or before the current ledger (#947).
    CliffTimestampInvalid = 124,
    /// The recipient is the all-zero Stellar address (#947).
    RecipientInvalid = 125,
    /// `bump_long_term_stream_ttl` was called on a stream that is not an
    /// active multi-year schedule (#880).
    StreamNotLongTerm = 123,
    NoRecipientProposal = 126,
    RecipientProposalStale = 127,
}
