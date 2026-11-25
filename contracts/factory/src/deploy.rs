use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{Address, BytesN, Env, Symbol, Val, Vec};

use crate::errors::Error;
use crate::governance;
use crate::storage::DataKey;
use crate::{events, pause, ttl};

/// Derives a deterministic salt from the stream ID (Issue #684).
pub fn compute_salt(env: &Env, stream_id: u64) -> BytesN<32> {
    env.crypto()
        .sha256(&soroban_sdk::Bytes::from_array(
            env,
            &stream_id.to_be_bytes(),
        ))
        .into()
}

/// Derives a deterministic deployment salt from sender, recipient, nonce, token, ledger sequence, and timestamp (Issue #764).
/// Incorporates ledger sequence and timestamp to prevent front-running and address collision.
pub fn derive_salt(
    env: &Env,
    sender: &Address,
    recipient: &Address,
    nonce: u64,
    token: &Address,
) -> BytesN<32> {
    let mut payload = soroban_sdk::Bytes::new(env);
    payload.append(&sender.to_xdr(env));
    payload.append(&recipient.to_xdr(env));
    payload.append(&soroban_sdk::Bytes::from_array(env, &nonce.to_be_bytes()));
    payload.append(&token.to_xdr(env));
    
    // Append ledger sequence (u32 as 4 BE bytes) and timestamp (u64 as 8 BE bytes)
    // to prevent pre-computation of the salt address before transaction inclusion.
    let sequence = env.ledger().sequence();
    payload.append(&soroban_sdk::Bytes::from_array(env, &sequence.to_be_bytes()));
    
    let timestamp = env.ledger().timestamp();
    payload.append(&soroban_sdk::Bytes::from_array(env, &timestamp.to_be_bytes()));
    
    env.crypto().sha256(&payload).into()
}

pub fn deploy_stream_with_salt(
    env: &Env,
    wasm_hash: &BytesN<32>,
    salt: BytesN<32>,
    init_args: Vec<Val>,
) -> Result<Address, Error> {
    let salt_key = DataKey::SaltUsed(salt.clone());
    if env.storage().persistent().has(&salt_key) {
        return Err(Error::SaltAlreadyUsed);
    }

    // Step 1: deploy the WASM — no constructor called yet.
    let addr = env
        .deployer()
        .with_current_contract(salt)
        .deploy(wasm_hash.clone());

    env.storage().persistent().set(&salt_key, &true);

    // Step 2: call `initialize_with_metadata` on the freshly deployed
    // contract. The variant accepts the optional metadata URI passed by
    // `create_stream`; direct deployments can still use `initialize`.
    let _: () = env.invoke_contract(
        &addr,
        &Symbol::new(env, "initialize_with_metadata"),
        init_args,
    );

    Ok(addr)
}

/// Returns the sender's current deployment nonce and advances the stored
/// counter (Issue #957).
///
/// The first deployment by a sender uses nonce `0`. The counter is a persistent
/// entry keyed by sender, and its TTL is bumped on every write. Overflow fails
/// with `ArithmeticOverflow` instead of wrapping, so a nonce is never reused.
pub fn next_sender_nonce(env: &Env, sender: &Address) -> Result<u64, Error> {
    let key = DataKey::SenderDeployNonce(sender.clone());
    let current: u64 = env.storage().persistent().get(&key).unwrap_or(0);
    let next = current.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
    env.storage().persistent().set(&key, &next);
    ttl::bump_persistent(env, &key);
    Ok(current)
}

/// Deploys and initializes a stream contract with a sender-scoped salt
/// (Issue #957), then emits `StreamContractDeployed` (Issue #958).
///
/// The salt is `derive_salt(sender, recipient, nonce, token)`, where `nonce` is
/// the sender's next deployment nonce. Two users, or one user deploying twice,
/// therefore never derive the same address.
///
/// Returns the stream address and the salt actually used. Callers must use the
/// returned salt for any event or record that reports it.
#[allow(clippy::too_many_arguments)]
pub fn deploy_stream(
    env: &Env,
    wasm_hash: &BytesN<32>,
    stream_id: u64,
    sender: &Address,
    recipient: &Address,
    token: &Address,
    init_args: Vec<Val>,
) -> Result<(Address, BytesN<32>), Error> {
    let nonce = next_sender_nonce(env, sender)?;
    let salt = derive_salt(env, sender, recipient, nonce, token);
    let addr = deploy_stream_with_salt(env, wasm_hash, salt.clone(), init_args)?;
    events::stream_contract_deployed(env, stream_id, sender, recipient, &addr);
    Ok((addr, salt))
}

/// Governance-guarded update of the WASM hash used for newly deployed streams.
///
/// Only the governor may call this. Streams created after the call deploy the
/// new code; streams that already exist are independent contracts and are
/// unaffected. Re-setting the current hash is a no-op and emits no event.
///
/// Checked in order: governor auth, non-zero hash, factory not paused.
/// Also bumps the instance and persistent-bucket TTLs, so the registry stays
/// alive during protocol-upgrade activity.
pub fn set_stream_wasm_hash(env: &Env, new_wasm_hash: &BytesN<32>) -> Result<(), Error> {
    let governor: Address = env
        .storage()
        .instance()
        .get(&DataKey::GovernorAddress)
        .ok_or(Error::NotInitialized)?;
    governor.require_auth();

    // A zero hash would deploy a no-op contract, burning funds sent to future
    // `create_stream` calls with no way to recover them.
    if *new_wasm_hash == BytesN::from_array(env, &[0u8; 32]) {
        return Err(Error::InvalidWasmHash);
    }

    // A paused factory accepts no state changes, even from the governor.
    pause::require_not_paused(env)?;

    let old_wasm_hash: BytesN<32> = env
        .storage()
        .instance()
        .get(&DataKey::StreamWasmHash)
        .ok_or(Error::NotInitialized)?;
    ttl::bump_instance(env);
    ttl::bump_persistent_bucket(env);
    if old_wasm_hash != *new_wasm_hash {
        env.storage()
            .instance()
            .set(&DataKey::StreamWasmHash, new_wasm_hash);
        events::stream_contract_upgraded(env, &old_wasm_hash, new_wasm_hash);
        // Version history and deprecation notice for the superseded hash (Issue #902).
        governance::record_wasm_version_change(env, &old_wasm_hash, new_wasm_hash);
    }
    Ok(())
}
