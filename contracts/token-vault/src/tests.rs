#![cfg(test)]

extern crate std;

use soroban_sdk::{
    symbol_short,
    testutils::{storage::Instance as _, Address as _, Events as _, Ledger as _},
    token, Address, Env, IntoVal, String, Symbol, TryIntoVal,
};

use crate::errors::Error;
use crate::{events, storage, ttl};
use crate::{DepositSpec, TokenVaultClient};

struct Setup {
    env: Env,
    client: TokenVaultClient<'static>,
    token: token::Client<'static>,
    owner: Address,
    user: Address,
}

impl Setup {
    fn new(max_limit: i128) -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let owner = Address::generate(&env);
        let user = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();

        let tok_admin = token::StellarAssetClient::new(&env, &token_addr);

        // Mint a large supply to the owner and user so authorized deposits succeed
        tok_admin.mint(&owner, &(max_limit));
        tok_admin.mint(&user, &(max_limit));

        let vault_id = env.register_contract(None, super::TokenVault);
        let client = TokenVaultClient::new(&env, &vault_id);

        client.initialize(&owner, &token_addr, &max_limit);

        // Leak env for 'static lifetime convenience in tests
        let env: &'static Env = std::boxed::Box::leak(std::boxed::Box::new(env));
        let client = TokenVaultClient::new(env, &vault_id);
        let token = token::Client::new(env, &token_addr);

        Setup {
            env: env.clone(),
            client,
            token,
            owner,
            user,
        }
    }
}

// ── Original deposit / withdraw / set_limit tests ──────────────────────────

#[test]
fn deposit_respects_max_limit() {
    let max_limit: i128 = 1_000_000;
    let s = Setup::new(max_limit);

    s.client.deposit(&s.owner, &max_limit);

    let result = s.client.try_deposit(&s.owner, &1);
    assert_eq!(result, Err(Ok(Error::LimitExceeded)));
}

#[test]
fn balance_and_max_limit_getters_report_current_state() {
    let s = Setup::new(1_000_000);

    assert_eq!(s.client.balance(), Some(0));
    assert_eq!(s.client.max_limit(), Some(1_000_000));

    s.client.deposit(&s.owner, &100);
    s.token.transfer(&s.user, &s.client.address, &25);
    assert_eq!(s.client.balance(), Some(125));

    s.client.set_limit(&s.owner, &2_000_000);
    assert_eq!(s.client.max_limit(), Some(2_000_000));
}

#[test]
fn deposit_rejects_amount_above_max_vault_deposit() {
    let s = Setup::new(i128::MAX);
    let too_large = crate::MAX_VAULT_DEPOSIT + 1;

    assert_eq!(
        s.client.try_deposit(&s.owner, &too_large),
        Err(Ok(Error::DepositExceedsMax))
    );
    assert_eq!(
        s.client.try_deposit_with_fee(&s.owner, &too_large, &1),
        Err(Ok(Error::DepositExceedsMax))
    );
    assert_eq!(
        s.client.try_batch_deposit(&soroban_sdk::vec![
            &s.env,
            DepositSpec {
                from: s.owner.clone(),
                amount: too_large
            }
        ]),
        Err(Ok(Error::DepositExceedsMax))
    );
    assert_eq!(s.token.balance(&s.client.address), 0);
}

#[test]
fn deposit_succeeds_with_real_sender_auth() {
    let max_limit: i128 = 1_000_000;
    let s = Setup::new(max_limit);

    s.client.deposit(&s.owner, &999_900);
    assert_eq!(
        s.client.try_deposit(&s.owner, &200),
        Err(Ok(Error::LimitExceeded))
    );
}

#[test]
fn operator_can_deposit() {
    let s = Setup::new(1_000_000);
    let op = Address::generate(&s.env);
    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&op, &500);
    s.client.set_operator(&s.owner, &op);

    s.client.deposit(&op, &500);
    assert_eq!(s.token.balance(&s.client.address), 500);
}

#[test]
fn stranger_cannot_deposit() {
    let s = Setup::new(1_000_000);
    let stranger = Address::generate(&s.env);

    let result = s.client.try_deposit(&stranger, &100);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn set_limit_succeeds_for_owner_with_real_auth() {
    let s = Setup::new(1_000_000);
    s.client.set_limit(&s.owner, &2_000_000);
}

#[test]
fn withdraw_succeeds_with_real_owner_auth() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &400);

    assert_eq!(s.token.balance(&recipient), 400);
}

#[test]
fn batch_withdraw_enforces_aggregate_operator_limit() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let operator = Address::generate(&s.env);
    let first = Address::generate(&s.env);
    let second = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &operator);
    s.client.set_operator_withdraw_limit(&s.owner, &500);

    let mut payouts = soroban_sdk::Vec::new(&s.env);
    payouts.push_back((first.clone(), 300));
    payouts.push_back((second.clone(), 300));
    assert_eq!(
        s.client.try_withdraw_batch(&operator, &payouts),
        Err(Ok(Error::LimitExceeded))
    );
    assert_eq!(s.client.balance(), Some(1_000));
    assert_eq!(s.token.balance(&first), 0);
    assert_eq!(s.token.balance(&second), 0);
}

#[test]
fn batch_withdraw_rejects_empty_payouts() {
    let s = Setup::new(1_000_000);
    let payouts = soroban_sdk::Vec::new(&s.env);
    assert_eq!(
        s.client.try_withdraw_batch(&s.owner, &payouts),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
fn batch_withdraw_emits_one_event_per_payout() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let operator = Address::generate(&s.env);
    let first = Address::generate(&s.env);
    let second = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &operator);
    s.client.set_operator_withdraw_limit(&s.owner, &500);

    let mut payouts = soroban_sdk::Vec::new(&s.env);
    payouts.push_back((first.clone(), 300));
    payouts.push_back((second.clone(), 200));
    s.client.withdraw_batch(&operator, &payouts);

    assert_eq!(s.client.balance(), Some(500));
    assert_eq!(s.token.balance(&first), 300);
    assert_eq!(s.token.balance(&second), 200);

    let events = vault_events(&s);
    assert_eq!(events.len(), 6);
    let first_payload: (Address, i128, i128, bool) =
        events[4].2.clone().try_into_val(&s.env).unwrap();
    let second_payload: (Address, i128, i128, bool) =
        events[5].2.clone().try_into_val(&s.env).unwrap();
    assert_eq!(first_payload, (first, 300, 700, true));
    assert_eq!(second_payload, (second, 200, 500, true));
}

#[test]
fn direct_transfers_count_toward_deposit_limit() {
    let s = Setup::new(1_000_000);

    s.client.deposit(&s.owner, &999_900);
    s.token.transfer(&s.user, &s.client.address, &100);

    assert_eq!(
        s.client.try_deposit(&s.owner, &1),
        Err(Ok(Error::LimitExceeded))
    );
}

#[test]
fn direct_transfers_are_withdrawable_by_owner() {
    let s = Setup::new(1_000_000);

    s.client.deposit(&s.owner, &500);
    s.token.transfer(&s.user, &s.client.address, &200);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &700);

    assert_eq!(s.token.balance(&recipient), 700);
}

// ── Auth-bypass regressions ─────────────────────────────────────────────────

fn seed_vault(
    env: &Env,
    vault_id: &Address,
    owner: &Address,
    token_addr: &Address,
    balance: i128,
    max_limit: i128,
) {
    env.as_contract(vault_id, || {
        storage::set_owner(env, owner);
        storage::set_token(env, token_addr);
        storage::set_max_limit(env, &max_limit);
        storage::set_balance(env, &balance);
    });
}

#[test]
#[should_panic]
fn initialize_without_owner_auth_panics() {
    let env = Env::default();
    let owner = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let vault_id = env.register_contract(None, super::TokenVault);
    let client = TokenVaultClient::new(&env, &vault_id);

    client.initialize(&owner, &token_addr, &1_000_000);
}

#[test]
#[should_panic]
fn withdraw_without_owner_auth_panics() {
    let env = Env::default();
    let owner = Address::generate(&env);
    let stranger = Address::generate(&env);
    let to = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let vault_id = env.register_contract(None, super::TokenVault);

    seed_vault(&env, &vault_id, &owner, &token_addr, 1_000, 1_000_000);

    let client = TokenVaultClient::new(&env, &vault_id);
    // stranger is neither owner nor operator — must panic
    client.withdraw(&stranger, &to, &1);
}

#[test]
#[should_panic]
fn set_limit_with_owner_address_but_no_real_auth_panics() {
    let env = Env::default();
    let owner = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let vault_id = env.register_contract(None, super::TokenVault);

    seed_vault(&env, &vault_id, &owner, &token_addr, 0, 1_000_000);

    let client = TokenVaultClient::new(&env, &vault_id);
    // Passing the real owner address without signed auth must still panic.
    client.set_limit(&owner, &2_000_000);
}

#[test]
#[should_panic]
fn deposit_without_sender_auth_panics() {
    let env = Env::default();
    let owner = Address::generate(&env);
    let user = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let vault_id = env.register_contract(None, super::TokenVault);

    seed_vault(&env, &vault_id, &owner, &token_addr, 0, 1_000_000);

    let client = TokenVaultClient::new(&env, &vault_id);
    client.deposit(&user, &1);
}

// ── Operator delegation tests ───────────────────────────────────────────────

#[test]
fn owner_can_set_and_revoke_operator() {
    let s = Setup::new(1_000_000);

    let op = Address::generate(&s.env);
    assert_eq!(s.client.operator(), None);

    s.client.set_operator(&s.owner, &op);
    assert_eq!(s.client.operator(), Some(op.clone()));

    s.client.revoke_operator(&s.owner);
    assert_eq!(s.client.operator(), None);
}

#[test]
fn non_owner_cannot_set_operator() {
    let s = Setup::new(1_000_000);

    let stranger = Address::generate(&s.env);
    let op = Address::generate(&s.env);
    let result = s.client.try_set_operator(&stranger, &op);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_owner_cannot_revoke_operator() {
    let s = Setup::new(1_000_000);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    let stranger = Address::generate(&s.env);
    let result = s.client.try_revoke_operator(&stranger);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn operator_can_withdraw() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.set_operator_withdraw_limit(&s.owner, &1_000);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&op, &recipient, &200);
    assert_eq!(s.token.balance(&recipient), 200);
}

#[test]
fn operator_can_lower_limit() {
    let s = Setup::new(1_000_000);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    s.client.set_limit(&op, &500_000);

    let vault_id = s.client.address.clone();
    s.env.as_contract(&vault_id, || {
        assert_eq!(storage::get_max_limit(&s.env), Some(500_000));
    });
}

#[test]
fn operator_cannot_raise_limit() {
    let s = Setup::new(1_000_000);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    let result = s.client.try_set_limit(&op, &2_000_000);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn owner_can_raise_limit() {
    let s = Setup::new(1_000_000);

    s.client.set_limit(&s.owner, &2_000_000);

    let vault_id = s.client.address.clone();
    s.env.as_contract(&vault_id, || {
        assert_eq!(storage::get_max_limit(&s.env), Some(2_000_000));
    });
}

#[test]
fn operator_withdraw_limit_is_enforced() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.set_operator_withdraw_limit(&s.owner, &100);

    let recipient = Address::generate(&s.env);
    let result = s.client.try_withdraw(&op, &recipient, &200);
    assert_eq!(result, Err(Ok(Error::LimitExceeded)));

    s.client.withdraw(&op, &recipient, &100);
    assert_eq!(s.token.balance(&recipient), 100);
}

#[test]
fn owner_withdraw_is_not_limited_by_operator_cap() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.set_operator_withdraw_limit(&s.owner, &100);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &500);
    assert_eq!(s.token.balance(&recipient), 500);
}

#[test]
fn effective_withdraw_limit_reflects_owner_balance_and_operator_cap() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.set_operator_withdraw_limit(&s.owner, &800);

    assert_eq!(s.client.effective_withdraw_limit(&s.owner), 500);
    assert_eq!(s.client.effective_withdraw_limit(&op), 500);

    s.client.set_operator_withdraw_limit(&s.owner, &200);
    assert_eq!(s.client.effective_withdraw_limit(&op), 200);
}

#[test]
fn effective_withdraw_limit_rejects_unauthorized_and_uncapped_operator() {
    let s = Setup::new(1_000_000);
    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    assert_eq!(
        s.client.try_effective_withdraw_limit(&op),
        Err(Ok(Error::LimitExceeded))
    );

    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_effective_withdraw_limit(&stranger),
        Err(Ok(Error::NotAuthorized))
    );
}

#[test]
fn stranger_cannot_withdraw_even_with_operator_set() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    let stranger = Address::generate(&s.env);
    let result = s.client.try_withdraw(&stranger, &stranger, &1);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn after_revoke_operator_can_no_longer_withdraw() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.revoke_operator(&s.owner);

    let result = s.client.try_withdraw(&op, &op, &1);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoke_operator_when_none_set_is_ok() {
    let s = Setup::new(1_000_000);
    // No operator set — revoke should be a no-op, not an error.
    let result = s.client.try_revoke_operator(&s.owner);
    assert!(result.is_ok());
}

// ── Emergency pause tests ───────────────────────────────────────────────────

#[test]
fn owner_can_pause_and_unpause() {
    let s = Setup::new(1_000_000);

    assert!(!s.client.is_paused());
    s.client.pause(&s.owner);
    assert!(s.client.is_paused());
    s.client.unpause(&s.owner);
    assert!(!s.client.is_paused());
}

#[test]
fn non_owner_cannot_pause() {
    let s = Setup::new(1_000_000);
    let stranger = Address::generate(&s.env);
    let result = s.client.try_pause(&stranger);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn double_pause_is_rejected() {
    let s = Setup::new(1_000_000);
    s.client.pause(&s.owner);
    let result = s.client.try_pause(&s.owner);
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
}

#[test]
fn unpause_when_not_paused_is_rejected() {
    let s = Setup::new(1_000_000);
    let result = s.client.try_unpause(&s.owner);
    assert_eq!(result, Err(Ok(Error::NotPaused)));
}

#[test]
fn pause_blocks_deposit() {
    let s = Setup::new(1_000_000);
    s.client.pause(&s.owner);
    let result = s.client.try_deposit(&s.user, &100);
    assert_eq!(result, Err(Ok(Error::VaultPaused)));
}

#[test]
fn pause_blocks_withdraw() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);
    s.client.pause(&s.owner);

    let recipient = Address::generate(&s.env);
    let result = s.client.try_withdraw(&s.owner, &recipient, &100);
    assert_eq!(result, Err(Ok(Error::VaultPaused)));
}

#[test]
fn pause_blocks_set_limit() {
    let s = Setup::new(1_000_000);
    s.client.pause(&s.owner);
    let result = s.client.try_set_limit(&s.owner, &2_000_000);
    assert_eq!(result, Err(Ok(Error::VaultPaused)));
}

#[test]
fn operations_resume_after_unpause() {
    let s = Setup::new(1_000_000);
    s.client.pause(&s.owner);
    s.client.unpause(&s.owner);

    // All three should work again after unpause
    s.client.deposit(&s.owner, &100);
    s.client.set_limit(&s.owner, &2_000_000);
    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &50);
    assert_eq!(s.token.balance(&recipient), 50);
}

#[test]
fn operator_also_blocked_by_pause() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.pause(&s.owner);

    let recipient = Address::generate(&s.env);
    let result = s.client.try_withdraw(&op, &recipient, &100);
    assert_eq!(result, Err(Ok(Error::VaultPaused)));
}

#[test]
fn vault_paused_error_is_distinct_and_gates_every_state_modifying_entrypoint() {
    assert_eq!(Error::VaultPaused as u32, 25);
    assert_ne!(Error::VaultPaused, Error::InsufficientVaultBalance);
    assert_ne!(Error::VaultPaused, Error::InsufficientVaultLiquidity);

    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);
    s.client.pause(&s.owner);

    let token = s.token.address.clone();
    let recipient = Address::generate(&s.env);
    let strategy = Address::generate(&s.env);
    let wasm_hash = soroban_sdk::BytesN::from_array(&s.env, &[1u8; 32]);
    let deposits = soroban_sdk::vec![
        &s.env,
        DepositSpec {
            from: s.owner.clone(),
            amount: 100,
        }
    ];
    let partial_deposits = soroban_sdk::vec![
        &s.env,
        crate::DepositArgs {
            from: s.owner.clone(),
            amount: 100,
        }
    ];
    let payouts = soroban_sdk::vec![&s.env, (recipient.clone(), 100)];
    let streams = soroban_sdk::vec![&s.env, Address::generate(&s.env)];

    assert_eq!(
        s.client.try_deposit(&s.owner, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_deposit_with_fee(&s.owner, &100, &10),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_batch_deposit(&deposits),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_batch_deposit_partial(&partial_deposits),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_withdraw(&s.owner, &recipient, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_withdraw_batch(&s.owner, &payouts),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_vault_batch_withdraw(&streams, &s.owner),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_deposit_token(&s.owner, &token, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client
            .try_withdraw_token(&s.owner, &token, &recipient, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_deposit_with_failure_report(&s.owner, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_limit(&s.owner, &2_000_000),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_operator_withdraw_limit(&s.owner, &100),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_operator(&s.owner, &recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_revoke_operator(&s.owner),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_propose_owner(&s.owner, &recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_accept_owner(&recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_owner_proposal_ttl(&s.owner, &60),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_upgrade(&s.owner, &wasm_hash),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_token_max_limit(&s.owner, &token, &1_000),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_min_deposit_amount(&s.owner, &token, &1),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_token_min_deposit(&s.owner, &token, &1),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_set_emergency_admin(&s.owner, &recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_emergency_sweep(&s.owner, &token, &recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_approve_token(&s.owner, &token, &true),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client
            .try_initiate_emergency_migration(&s.owner, &recipient),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_execute_emergency_migration(&s.owner),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_cancel_emergency_migration(&s.owner),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client.try_approve_strategy(&s.owner, &strategy, &true),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &token, &strategy, &100),
        Err(Ok(Error::VaultPaused))
    );

    // Maintenance and recovery controls stay available while halted.
    s.client.keep_alive();
    s.client.bump_all_vault_keys();
    s.client.unpause(&s.owner);
    assert!(!s.client.is_paused());
    s.client.deposit(&s.owner, &100);
}

#[test]
fn pause_is_reported_before_insufficient_balance_errors() {
    let s = Setup::new(1_000_000);
    let strategy = Address::generate(&s.env);
    s.client.pause(&s.owner);

    assert_eq!(
        s.client.try_withdraw(&s.owner, &s.user, &1),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &1),
        Err(Ok(Error::VaultPaused))
    );
}

// ── Event emission (issue #311) ────────────────────────────────────────────

/// All events published by the vault contract, oldest first.
///
/// `env.events().all()` also captures events emitted by other contracts in the
/// same test (e.g. the token's own transfer events), so filter down to the
/// vault's contract address. `Setup::new` publishes the `init` event, so
/// `events[0]` is always the initialization event.
fn vault_events(
    s: &Setup,
) -> std::vec::Vec<(
    Address,
    soroban_sdk::Vec<soroban_sdk::Val>,
    soroban_sdk::Val,
)> {
    s.env
        .events()
        .all()
        .iter()
        .filter(|(contract, _, _)| contract == &s.client.address)
        .map(|(c, t, d)| (c.clone(), t.clone(), d))
        .collect()
}

#[test]
fn initialize_emits_initialized_event() {
    let s = Setup::new(1_000_000);

    let events = vault_events(&s);
    assert_eq!(events.len(), 1);

    let (_, topics, data) = &events[0];
    assert_eq!(
        topics.clone(),
        (symbol_short!("init"), s.owner.clone()).into_val(&s.env)
    );
    let payload: (Address, i128) = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, (s.token.address.clone(), 1_000_000_i128));
}

#[test]
fn deposit_emits_deposited_event() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let events = vault_events(&s);
    assert_eq!(events.len(), 2);

    let (_, topics, data) = &events[1];
    assert_eq!(
        topics.clone(),
        (symbol_short!("deposited"), s.owner.clone()).into_val(&s.env)
    );
    let payload: (i128, i128) = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, (500_i128, 500_i128));
}

#[test]
fn withdraw_emits_withdrawn_event() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &400);

    let events = vault_events(&s);
    assert_eq!(events.len(), 3);

    let (_, topics, data) = &events[2];
    assert_eq!(
        topics.clone(),
        (symbol_short!("withdrawn"), s.owner.clone()).into_val(&s.env)
    );
    let payload: (Address, i128, i128, bool) = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, (recipient.clone(), 400_i128, 600_i128, false));
}

#[test]
fn operator_withdraw_emits_by_operator_flag() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);
    s.client.set_operator_withdraw_limit(&s.owner, &1_000);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&op, &recipient, &300);

    let events = vault_events(&s);
    // initialized (from Setup::new) + deposited + operator_set +
    // operator_withdraw_limit_set + withdrawn.
    assert_eq!(events.len(), 5);

    let (_, topics, data) = &events[4];
    assert_eq!(
        topics.clone(),
        (symbol_short!("withdrawn"), op.clone()).into_val(&s.env)
    );
    let payload: (Address, i128, i128, bool) = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, (recipient.clone(), 300_i128, 700_i128, true));
}

#[test]
fn set_limit_emits_limit_set_event_with_old_and_new() {
    let s = Setup::new(1_000_000);
    s.client.set_limit(&s.owner, &2_000_000);

    let events = vault_events(&s);
    assert_eq!(events.len(), 2);

    let (_, topics, data) = &events[1];
    assert_eq!(
        topics.clone(),
        (symbol_short!("limit_set"), s.owner.clone()).into_val(&s.env)
    );
    let payload: (i128, i128) = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, (1_000_000_i128, 2_000_000_i128));
}

#[test]
fn operator_set_and_revoke_emit_events() {
    let s = Setup::new(1_000_000);

    let op = Address::generate(&s.env);
    s.client.set_operator(&s.owner, &op);

    let events = vault_events(&s);
    assert_eq!(events.len(), 2);
    let (_, topics, data) = &events[1];
    assert_eq!(
        topics.clone(),
        (symbol_short!("set_op"), s.owner.clone()).into_val(&s.env)
    );
    let payload: Address = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, op);

    s.client.revoke_operator(&s.owner);

    let events = vault_events(&s);
    assert_eq!(events.len(), 3);
    let (_, topics, data) = &events[2];
    assert_eq!(
        topics.clone(),
        (symbol_short!("rm_op"), s.owner.clone()).into_val(&s.env)
    );
    let payload: () = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, ());
}

#[test]
fn keep_alive_emits_target_ttl() {
    let s = Setup::new(1_000_000);
    s.client.keep_alive();

    let events = vault_events(&s);
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics.clone(),
        (Symbol::new(&s.env, "kept_alive"),).into_val(&s.env)
    );
    let new_ttl: u32 = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(new_ttl, crate::TTL_EXTEND_TO);
}

#[test]
fn pause_and_unpause_emit_events() {
    let s = Setup::new(1_000_000);
    let paused_at = s.env.ledger().timestamp();

    s.client.pause(&s.owner);
    let events = vault_events(&s);
    assert_eq!(events.len(), 2);
    let (_, topics, data) = &events[1];
    assert_eq!(
        topics.clone(),
        (symbol_short!("paused"), s.owner.clone()).into_val(&s.env)
    );
    let payload: u64 = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, paused_at);

    s.client.unpause(&s.owner);
    let events = vault_events(&s);
    assert_eq!(events.len(), 3);
    let (_, topics, data) = &events[2];
    assert_eq!(
        topics.clone(),
        (symbol_short!("unpaused"), s.owner.clone()).into_val(&s.env)
    );
    let payload: u64 = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, paused_at);
}

#[test]
fn failed_operations_emit_no_events() {
    let s = Setup::new(1_000_000);
    let base = vault_events(&s).len();

    // Deposit over the limit reverts without publishing a `deposited` event.
    s.client.deposit(&s.owner, &999_900);
    let result = s.client.try_deposit(&s.owner, &200);
    assert_eq!(result, Err(Ok(Error::LimitExceeded)));
    assert_eq!(vault_events(&s).len(), base + 1);

    // Double pause reverts without publishing a second `paused` event.
    s.client.pause(&s.owner);
    let result = s.client.try_pause(&s.owner);
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
    assert_eq!(vault_events(&s).len(), base + 2);

    // Unpause when not paused reverts without publishing an `unpaused` event.
    s.client.unpause(&s.owner);
    let result = s.client.try_unpause(&s.owner);
    assert_eq!(result, Err(Ok(Error::NotPaused)));
    assert_eq!(vault_events(&s).len(), base + 3);

    // Unauthorized withdraw reverts without publishing a `withdrawn` event.
    let stranger = Address::generate(&s.env);
    let result = s.client.try_withdraw(&stranger, &stranger, &1);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    assert_eq!(vault_events(&s).len(), base + 3);

    // Setting a limit below the current balance reverts without publishing.
    let result = s.client.try_set_limit(&s.owner, &100);
    assert_eq!(result, Err(Ok(Error::LimitExceeded)));
    assert_eq!(vault_events(&s).len(), base + 3);
}

// ── Uninitialized state tests ───────────────────────────────────────────────

#[test]
fn uninitialized_vault_returns_not_initialized() {
    let env = Env::default();
    env.mock_all_auths();
    let vault_id = env.register_contract(None, super::TokenVault);
    let client = TokenVaultClient::new(&env, &vault_id);

    let user = Address::generate(&env);
    let recipient = Address::generate(&env);
    let operator = Address::generate(&env);

    assert_eq!(
        client.try_deposit(&user, &100),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(
        client.try_withdraw(&user, &recipient, &100),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(
        client.try_set_limit(&user, &100),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(
        client.try_set_operator(&user, &operator),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(
        client.try_revoke_operator(&user),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(client.try_pause(&user), Err(Ok(Error::NotInitialized)));
    assert_eq!(client.try_unpause(&user), Err(Ok(Error::NotInitialized)));
}

#[test]
fn re_initializing_vault_reports_already_initialized() {
    // Asserted against the named variant rather than `Error(Contract, #9)`.
    // A numeric assertion keeps passing if the enum is ever reordered, which
    // is precisely the "caller gets the wrong error code" failure this check
    // exists to catch.
    let s = Setup::new(1_000_000);
    let another_owner = Address::generate(&s.env);
    let token_admin = Address::generate(&s.env);
    let token_addr = s
        .env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let result = s
        .client
        .try_initialize(&another_owner, &token_addr, &2_000_000);

    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn failed_re_initialization_leaves_the_original_owner_in_place() {
    // The error code matters, but so does the state: a rejected re-init must
    // not have partially overwritten the vault's owner or limit.
    let s = Setup::new(1_000_000);
    let attacker = Address::generate(&s.env);
    let token_admin = Address::generate(&s.env);
    let token_addr = s
        .env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let _ = s.client.try_initialize(&attacker, &token_addr, &2_000_000);

    let vault_id = s.client.address.clone();
    s.env.as_contract(&vault_id, || {
        assert_eq!(storage::get_owner(&s.env), Some(s.owner.clone()));
        assert_eq!(storage::get_max_limit(&s.env), Some(1_000_000));
    });
}

// ── TTL extension tests ───────────────────────────────────────────────────

#[test]
fn initialize_extends_instance_ttl() {
    let s = Setup::new(1_000_000);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after initialize: {ttl}");
}

#[test]
fn deposit_extends_instance_ttl() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after deposit: {ttl}");
}

#[test]
fn withdraw_extends_instance_ttl() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &50);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after withdraw: {ttl}");
}

#[test]
fn set_limit_extends_instance_ttl() {
    let s = Setup::new(1_000_000);
    s.client.set_limit(&s.owner, &2_000_000);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after set_limit: {ttl}");
}

#[test]
fn operator_and_pause_mutations_extend_instance_ttl() {
    let s = Setup::new(1_000_000);
    let operator = Address::generate(&s.env);

    s.client.set_operator(&s.owner, &operator);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after set_operator: {ttl}");

    s.client.revoke_operator(&s.owner);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after revoke_operator: {ttl}");

    s.client.pause(&s.owner);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after pause: {ttl}");

    s.client.unpause(&s.owner);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert!(ttl >= 100_000, "instance TTL after unpause: {ttl}");
}

// ── Two-step owner transfer tests ───────────────────────────────────────────

#[test]
fn owner_can_propose_and_new_owner_accepts() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    assert_eq!(s.client.owner(), Some(s.owner.clone()));
    s.client.propose_owner(&s.owner, &new_owner);
    s.client.accept_owner(&new_owner);

    assert_eq!(s.client.owner(), Some(new_owner));
}

#[test]
fn expired_owner_proposal_requires_a_fresh_proposal() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);
    let proposed_at = s.env.ledger().timestamp();

    assert_eq!(s.client.owner_proposal_ttl(), Some(7 * 24 * 60 * 60));
    s.client.set_owner_proposal_ttl(&s.owner, &60);
    s.client.propose_owner(&s.owner, &new_owner);
    s.env.ledger().set_timestamp(proposed_at + 60);

    assert_eq!(
        s.client.try_accept_owner(&new_owner),
        Err(Ok(Error::PendingOwnerExpired))
    );
    assert_eq!(s.client.owner(), Some(s.owner.clone()));
    assert_eq!(s.client.pending_owner(), Some(new_owner.clone()));

    s.client.propose_owner(&s.owner, &new_owner);
    s.client.accept_owner(&new_owner);
    assert_eq!(s.client.owner(), Some(new_owner));
    assert_eq!(s.client.pending_owner(), None);
}

#[test]
fn zero_owner_proposal_ttl_is_rejected() {
    let s = Setup::new(1_000_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_set_owner_proposal_ttl(&stranger, &60),
        Err(Ok(Error::NotAuthorized))
    );
    assert_eq!(
        s.client.try_set_owner_proposal_ttl(&s.owner, &0),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
fn pending_owner_and_proposer_are_readable() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    // Before any proposal, both getters return None.
    assert_eq!(s.client.pending_owner(), None);
    assert_eq!(s.client.pending_owner_proposer(), None);

    s.client.propose_owner(&s.owner, &new_owner);

    // After proposal, both getters reflect the pending state.
    assert_eq!(s.client.pending_owner(), Some(new_owner.clone()));
    assert_eq!(s.client.pending_owner_proposer(), Some(s.owner.clone()));

    s.client.accept_owner(&new_owner);

    // After acceptance, pending state is cleared.
    assert_eq!(s.client.pending_owner(), None);
    assert_eq!(s.client.pending_owner_proposer(), None);
    assert_eq!(s.client.owner(), Some(new_owner));
}

#[test]
fn propose_owner_rejects_zero_address() {
    let s = Setup::new(1_000_000);
    let zero = Address::from_string(&String::from_str(
        &s.env,
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    ));
    let result = s.client.try_propose_owner(&s.owner, &zero);
    assert_eq!(result, Err(Ok(Error::InvalidParam)));
}

#[test]
fn non_owner_cannot_propose_owner() {
    let s = Setup::new(1_000_000);
    let stranger = Address::generate(&s.env);
    let new_owner = Address::generate(&s.env);
    let result = s.client.try_propose_owner(&stranger, &new_owner);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn accept_owner_when_none_pending_is_rejected() {
    let s = Setup::new(1_000_000);
    let stranger = Address::generate(&s.env);
    let result = s.client.try_accept_owner(&stranger);
    assert_eq!(result, Err(Ok(Error::NoPendingOwner)));
}

#[test]
fn accept_owner_by_non_pending_address_is_rejected() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);
    let stranger = Address::generate(&s.env);

    s.client.propose_owner(&s.owner, &new_owner);
    let result = s.client.try_accept_owner(&stranger);
    assert_eq!(result, Err(Ok(Error::NotPendingOwner)));
}

#[test]
fn propose_owner_does_not_change_ownership_until_accept() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    s.client.propose_owner(&s.owner, &new_owner);
    // Old owner retains authority until the transfer is accepted.
    assert_eq!(s.client.owner(), Some(s.owner.clone()));

    // Proposing while a transfer is already pending is rejected for a
    // non-owner... but the old owner can still propose, which simply
    // overwrites the pending owner. The real owner stays until accept.
    let other = Address::generate(&s.env);
    s.client.propose_owner(&s.owner, &other);
    assert_eq!(s.client.owner(), Some(s.owner.clone()));
}

#[test]
fn new_owner_gains_privileges_after_accept() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    // Before accept, the new owner cannot pause.
    assert_eq!(
        s.client.try_pause(&new_owner),
        Err(Ok(Error::NotAuthorized))
    );

    s.client.propose_owner(&s.owner, &new_owner);
    s.client.accept_owner(&new_owner);

    // After accept, the new owner can pause and the old owner cannot.
    s.client.pause(&new_owner);
    assert!(s.client.is_paused());
    assert_eq!(
        s.client.try_unpause(&s.owner),
        Err(Ok(Error::NotAuthorized))
    );
    s.client.unpause(&new_owner);
}

#[test]
fn owner_transfer_emits_events() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    s.client.propose_owner(&s.owner, &new_owner);
    let events = vault_events(&s);
    assert_eq!(events.len(), 2);
    let (_, topics, data) = &events[1];
    assert_eq!(
        topics.clone(),
        (symbol_short!("propose"), s.owner.clone()).into_val(&s.env)
    );
    let payload: Address = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, new_owner);

    s.client.accept_owner(&new_owner);
    let events = vault_events(&s);
    assert_eq!(events.len(), 3);
    let (_, topics, data) = &events[2];
    assert_eq!(
        topics.clone(),
        (symbol_short!("accept"), new_owner.clone()).into_val(&s.env)
    );
    let payload: Address = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(payload, s.owner);
}

#[test]
fn re_propose_overwrites_pending_owner() {
    let s = Setup::new(1_000_000);
    let first_pending = Address::generate(&s.env);
    let second_pending = Address::generate(&s.env);

    s.client.propose_owner(&s.owner, &first_pending);
    assert_eq!(
        s.env
            .as_contract(&s.client.address, || storage::get_pending_owner(&s.env)),
        Some(first_pending.clone())
    );

    // Owner re-proposes to a different address.
    s.client.propose_owner(&s.owner, &second_pending);
    assert_eq!(
        s.env
            .as_contract(&s.client.address, || storage::get_pending_owner(&s.env)),
        Some(second_pending.clone())
    );

    // The first pending owner can no longer accept.
    let result = s.client.try_accept_owner(&first_pending);
    assert_eq!(result, Err(Ok(Error::NotPendingOwner)));

    // Only the second pending owner can accept.
    s.client.accept_owner(&second_pending);
    assert_eq!(s.client.owner(), Some(second_pending));
}

#[test]
fn operations_during_pending_transfer_succeed() {
    let s = Setup::new(1_000_000);
    let new_owner = Address::generate(&s.env);

    s.client.deposit(&s.owner, &500);
    s.client.propose_owner(&s.owner, &new_owner);

    // Pending transfer must not block deposits / withdrawals.
    s.client.deposit(&s.owner, &200);

    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &100);
    assert_eq!(s.token.balance(&recipient), 100);

    // Old owner can still pause.
    s.client.pause(&s.owner);
    assert!(s.client.is_paused());
    s.client.unpause(&s.owner);

    // Transfer completes normally afterwards.
    s.client.accept_owner(&new_owner);
    assert_eq!(s.client.owner(), Some(new_owner));
}

#[test]
fn uninitialized_vault_rejects_owner_transfer() {
    let env = Env::default();
    env.mock_all_auths();
    let vault_id = env.register_contract(None, super::TokenVault);
    let client = TokenVaultClient::new(&env, &vault_id);

    let caller = Address::generate(&env);
    let new_owner = Address::generate(&env);

    assert_eq!(
        client.try_propose_owner(&caller, &new_owner),
        Err(Ok(Error::NotInitialized))
    );
    assert_eq!(
        client.try_accept_owner(&caller),
        Err(Ok(Error::NoPendingOwner))
    );
}

// ── Batch deposit tests ───────────────────────────────────────────────────

#[test]
fn batch_deposit_multiple_accounts_succeeds() {
    let s = Setup::new(1_000_000);
    let sponsor1 = Address::generate(&s.env);
    let sponsor2 = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&sponsor1, &10_000);
    asset.mint(&sponsor2, &20_000);

    let deposits = soroban_sdk::vec![
        &s.env,
        DepositSpec {
            from: sponsor1.clone(),
            amount: 5_000,
        },
        DepositSpec {
            from: sponsor2.clone(),
            amount: 15_000,
        },
    ];

    s.client.batch_deposit(&deposits);

    assert_eq!(s.token.balance(&s.client.address), 20_000);
    assert_eq!(s.token.balance(&sponsor1), 5_000);
    assert_eq!(s.token.balance(&sponsor2), 5_000);
}

#[test]
fn batch_deposit_empty_fails() {
    let s = Setup::new(1_000_000);
    let empty_deposits = soroban_sdk::vec![&s.env];

    assert_eq!(
        s.client.try_batch_deposit(&empty_deposits),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
fn batch_deposit_zero_or_negative_amount_fails() {
    let s = Setup::new(1_000_000);
    let sponsor1 = Address::generate(&s.env);
    let sponsor2 = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&sponsor1, &10_000);
    asset.mint(&sponsor2, &10_000);

    let deposits = soroban_sdk::vec![
        &s.env,
        DepositSpec {
            from: sponsor1,
            amount: 5_000,
        },
        DepositSpec {
            from: sponsor2,
            amount: 0,
        },
    ];

    assert_eq!(
        s.client.try_batch_deposit(&deposits),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
fn batch_deposit_limit_exceeded_fails() {
    let s = Setup::new(10_000);
    let sponsor1 = Address::generate(&s.env);
    let sponsor2 = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&sponsor1, &10_000);
    asset.mint(&sponsor2, &10_000);

    let deposits = soroban_sdk::vec![
        &s.env,
        DepositSpec {
            from: sponsor1,
            amount: 6_000,
        },
        DepositSpec {
            from: sponsor2,
            amount: 5_000,
        },
    ];

    assert_eq!(
        s.client.try_batch_deposit(&deposits),
        Err(Ok(Error::LimitExceeded))
    );
}

#[test]
fn batch_deposit_while_paused_fails() {
    let s = Setup::new(1_000_000);
    let sponsor = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&sponsor, &10_000);

    s.client.pause(&s.owner);

    let deposits = soroban_sdk::vec![
        &s.env,
        DepositSpec {
            from: sponsor,
            amount: 5_000,
        },
    ];

    assert_eq!(
        s.client.try_batch_deposit(&deposits),
        Err(Ok(Error::VaultPaused))
    );
}

#[test]
fn batch_deposit_partial_reports_successes_and_failures_without_reverting() {
    let s = Setup::new(1_000_000);
    let funded = Address::generate(&s.env);
    let second_sender = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&funded, &10_000);
    asset.mint(&second_sender, &10_000);

    let deposits = soroban_sdk::vec![
        &s.env,
        crate::DepositArgs {
            from: funded.clone(),
            amount: 5_000,
        },
        crate::DepositArgs {
            from: second_sender.clone(),
            amount: 0,
        },
        crate::DepositArgs {
            from: funded.clone(),
            amount: crate::MAX_VAULT_DEPOSIT + 1,
        },
    ];

    let results = s.client.batch_deposit_partial(&deposits);
    assert_eq!(results.len(), 3);

    let first = results.get(0).unwrap();
    assert!(first.success);
    assert_eq!(first.error_code, None);
    assert_eq!(first.deposited, 5_000);

    let second = results.get(1).unwrap();
    assert!(!second.success);
    assert_eq!(second.error_code, Some(Error::InvalidAmount as u32));
    assert_eq!(second.deposited, 0);

    let third = results.get(2).unwrap();
    assert!(!third.success);
    assert_eq!(third.error_code, Some(Error::DepositExceedsMax as u32));

    // Only the valid entry settled.
    assert_eq!(s.token.balance(&s.client.address), 5_000);
    assert_eq!(s.token.balance(&funded), 5_000);
    assert_eq!(
        s.client
            .get_depositor_token_balance(&funded, &s.token.address),
        5_000
    );
}

#[test]
fn batch_deposit_partial_rejects_entries_that_would_exceed_the_limit() {
    let s = Setup::new(10_000);
    let sponsor1 = Address::generate(&s.env);
    let sponsor2 = Address::generate(&s.env);

    let asset = token::StellarAssetClient::new(&s.env, &s.token.address);
    asset.mint(&sponsor1, &10_000);
    asset.mint(&sponsor2, &10_000);

    let deposits = soroban_sdk::vec![
        &s.env,
        crate::DepositArgs {
            from: sponsor1,
            amount: 6_000,
        },
        crate::DepositArgs {
            from: sponsor2,
            amount: 6_000,
        },
    ];

    let results = s.client.batch_deposit_partial(&deposits);
    assert!(results.get(0).unwrap().success);
    assert!(!results.get(1).unwrap().success);
    assert_eq!(
        results.get(1).unwrap().error_code,
        Some(Error::LimitExceeded as u32)
    );
    assert_eq!(s.token.balance(&s.client.address), 6_000);
}

// ── Emergency Migration tests (Issue #679) ────────────────────────────────

#[test]
fn emergency_migration_flow_enforces_timelock_and_transfers() {
    let s = Setup::new(1_000_000);
    let cold_storage = Address::generate(&s.env);
    let stranger = Address::generate(&s.env);

    s.client.deposit(&s.owner, &50_000);
    assert_eq!(s.token.balance(&s.client.address), 50_000);

    // Stranger cannot initiate
    assert_eq!(
        s.client
            .try_initiate_emergency_migration(&stranger, &cold_storage),
        Err(Ok(Error::NotAuthorized))
    );

    // Owner initiates
    let now = s.env.ledger().timestamp();
    s.client
        .initiate_emergency_migration(&s.owner, &cold_storage);

    let migration = s.client.emergency_migration().unwrap();
    assert_eq!(migration.destination, cold_storage);
    assert_eq!(migration.proposed_at, now);
    assert_eq!(migration.executable_at, now + 48 * 3600);

    // Stranger cannot execute
    assert_eq!(
        s.client.try_execute_emergency_migration(&stranger),
        Err(Ok(Error::NotAuthorized))
    );

    // Before 48h timelock expires: fails
    s.env.ledger().set_timestamp(now + 48 * 3600 - 1);
    assert_eq!(
        s.client.try_execute_emergency_migration(&s.owner),
        Err(Ok(Error::TimelockNotExpired))
    );

    // At or after 48h: succeeds
    s.env.ledger().set_timestamp(now + 48 * 3600);
    let migrated = s.client.execute_emergency_migration(&s.owner);
    assert_eq!(migrated, 50_000);

    assert_eq!(s.token.balance(&s.client.address), 0);
    assert_eq!(s.token.balance(&cold_storage), 50_000);
    assert_eq!(s.client.emergency_migration(), None);
}

#[test]
fn emergency_migration_can_be_cancelled() {
    let s = Setup::new(1_000_000);
    let cold_storage = Address::generate(&s.env);
    let stranger = Address::generate(&s.env);

    s.client
        .initiate_emergency_migration(&s.owner, &cold_storage);
    assert!(s.client.emergency_migration().is_some());

    // Stranger cannot cancel
    assert_eq!(
        s.client.try_cancel_emergency_migration(&stranger),
        Err(Ok(Error::NotAuthorized))
    );

    // Owner cancels
    s.client.cancel_emergency_migration(&s.owner);
    assert_eq!(s.client.emergency_migration(), None);

    // Cancelling when none is pending fails
    assert_eq!(
        s.client.try_cancel_emergency_migration(&s.owner),
        Err(Ok(Error::NoMigrationProposed))
    );
}

#[test]
fn test_deposit_with_fee_emits_token_deposited_event() {
    let s = Setup::new(1_000_000);
    s.client.deposit_with_fee(&s.owner, &1_000, &50);

    let events = vault_events(&s);
    // initialized + deposited + token_deposited
    assert_eq!(events.len(), 3);

    let (_, topics, data) = &events[2];
    assert_eq!(
        topics.clone(),
        (Symbol::new(&s.env, "TokenDeposited"), s.owner.clone()).into_val(&s.env)
    );
    let payload: crate::events::DepositEvent = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(
        payload,
        crate::events::DepositEvent {
            gross_amount: 1_000,
            fee_amount: 50,
            net_amount: 950,
        }
    );
    assert_eq!(s.client.balance(), Some(950));
}

#[test]
fn test_reentrancy_guard_blocks_nested_transfers() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &10_000);

    // Simulate active transient reentrancy lock during an external token call
    s.env.as_contract(&s.client.address, || {
        s.env
            .storage()
            .temporary()
            .set(&crate::storage::DataKey::ReentrancyLock, &true);
    });

    assert_eq!(
        s.client.try_deposit(&s.owner, &1_000),
        Err(Ok(Error::ReentrancyDetected))
    );
    assert_eq!(
        s.client.try_withdraw(&s.owner, &s.user, &1_000),
        Err(Ok(Error::ReentrancyDetected))
    );
    assert_eq!(
        s.client
            .try_deposit_token(&s.owner, &s.token.address, &1_000),
        Err(Ok(Error::ReentrancyDetected))
    );
    assert_eq!(
        s.client
            .try_withdraw_token(&s.owner, &s.token.address, &s.user, &1_000),
        Err(Ok(Error::ReentrancyDetected))
    );

    // Clear lock and verify transfers succeed again and clean up transient lock
    s.env.as_contract(&s.client.address, || {
        s.env
            .storage()
            .temporary()
            .remove(&crate::storage::DataKey::ReentrancyLock);
    });
    assert!(s.client.try_withdraw(&s.owner, &s.user, &1_000).is_ok());
}

// ── Ledger balance snapshots / asset approval / failure receipts / TTL ──

#[test]
fn vault_rejects_unexpected_same_ledger_balance_changes() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    // An unsolicited transfer changes live token balances without updating
    // the vault's last verified expected balance.
    s.token.transfer(&s.user, &s.client.address, &10);
    assert_eq!(
        s.client.try_deposit(&s.owner, &20),
        Err(Ok(Error::VaultBalanceChanged))
    );
}

#[test]
fn approved_tokens_only_and_revocation_preserves_withdrawal() {
    let s = Setup::new(1_000_000);
    assert!(s.client.is_token_approved(&s.token.address));
    let admin = Address::generate(&s.env);
    let other_token = s.env.register_stellar_asset_contract_v2(admin).address();
    let other = token::Client::new(&s.env, &other_token);
    token::StellarAssetClient::new(&s.env, &other_token).mint(&s.owner, &1000);
    assert_eq!(
        s.client.try_deposit_token(&s.owner, &other_token, &10),
        Err(Ok(Error::TokenNotApprovedForVault))
    );
    s.client.approve_token(&s.owner, &other_token, &true);
    s.client.deposit_token(&s.owner, &other_token, &100);
    assert_eq!(other.balance(&s.client.address), 100);
    s.client.approve_token(&s.owner, &other_token, &false);
    assert_eq!(
        s.client.try_deposit_token(&s.owner, &other_token, &10),
        Err(Ok(Error::TokenNotApprovedForVault))
    );
    s.client
        .withdraw_token(&s.owner, &other_token, &s.user, &25);
    assert_eq!(other.balance(&s.client.address), 75);
}

#[test]
fn failed_token_transfer_can_be_reported_without_reverting_event() {
    let s = Setup::new(1_000_000);
    // Move away the owner's entire balance before attempting a deposit.
    s.token.transfer(&s.owner, &s.user, &1_000_000);
    assert!(!s.client.deposit_with_failure_report(&s.owner, &100));
    assert_eq!(s.token.balance(&s.client.address), 0);
    let events = vault_events(&s);
    let (_, topics, _) = events.last().unwrap();
    assert_eq!(
        topics.clone(),
        (Symbol::new(&s.env, "VaultDepositFailed"), s.owner.clone()).into_val(&s.env)
    );
}

#[test]
fn batch_key_renewal_covers_account_balances_and_token_limits() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    s.client
        .set_token_max_limit(&s.owner, &s.token.address, &500_000);
    assert_eq!(s.client.bump_all_vault_keys(), 2);
    // Existing permissionless keep_alive must also extend indexed entries.
    s.client.keep_alive();
    let contract = s.client.address.clone();
    s.env.as_contract(&contract, || {
        let key = storage::DataKey::TokenAccountBalance(s.token.address.clone(), s.owner.clone());
        assert!(s.env.storage().persistent().has(&key));
    });
}

#[test]
fn default_asset_revocation_stops_deposits_not_withdrawals() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    s.client.approve_token(&s.owner, &s.token.address, &false);
    assert_eq!(
        s.client.try_deposit(&s.owner, &10),
        Err(Ok(Error::TokenNotApprovedForVault))
    );
    s.client.withdraw(&s.owner, &s.user, &25);
    assert_eq!(s.token.balance(&s.client.address), 75);
}

// ── Issue #757: DepositorTokens mapping & faster balance lookup ──────────────

#[test]
fn test_depositor_token_balance_tracking() {
    let s = Setup::new(1_000_000);
    assert_eq!(
        s.client.get_depositor_token_balance(&s.owner, &s.token.address),
        0
    );

    // Deposit updates depositor token balance
    s.client.deposit(&s.owner, &500);
    assert_eq!(
        s.client.get_depositor_token_balance(&s.owner, &s.token.address),
        500
    );

    // Further deposit accumulates
    s.client.deposit(&s.owner, &250);
    assert_eq!(
        s.client.get_depositor_token_balance(&s.owner, &s.token.address),
        750
    );

    // Direct storage helper check
    s.env.as_contract(&s.client.address, || {
        let bal = storage::get_depositor_token_balance(&s.env, &s.owner, &s.token.address);
        assert_eq!(bal, 750);
    });

    // Withdrawal decreases depositor balance
    let recipient = Address::generate(&s.env);
    s.client.withdraw(&s.owner, &recipient, &300);
    assert_eq!(
        s.client.get_depositor_token_balance(&s.owner, &s.token.address),
        450
    );
}

// ── Issue #758: VaultWithdrawal event with previous and new balances ─────────

#[test]
fn test_vault_withdrawal_event_includes_previous_and_new_balance() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &1_000);

    let recipient = Address::generate(&s.env);
    s.client
        .withdraw_token(&s.owner, &s.token.address, &recipient, &400);

    let events = s.env.events().all();
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics.clone(),
        (
            Symbol::new(&s.env, "VaultWithdrawal"),
            s.owner.clone(),
            recipient.clone()
        )
            .into_val(&s.env)
    );

    let payload: events::VaultWithdrawal = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(
        payload,
        events::VaultWithdrawal {
            token: s.token.address.clone(),
            recipient: recipient.clone(),
            amount: 400,
            previous_balance: 1_000,
            new_balance: 600,
            timestamp: s.env.ledger().timestamp(),
        }
    );
}

// ── Issue #950: VaultDeposit event with token and ledger timestamp ──────────

#[test]
fn deposit_token_emits_vault_deposit_event() {
    let s = Setup::new(1_000_000);
    s.client.deposit_token(&s.owner, &s.token.address, &250);

    let events = s.env.events().all();
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics.clone(),
        (Symbol::new(&s.env, "VaultDeposit"), s.owner.clone()).into_val(&s.env)
    );

    let payload: events::VaultDeposit = data.clone().try_into_val(&s.env).unwrap();
    assert_eq!(
        payload,
        events::VaultDeposit {
            token: s.token.address.clone(),
            from: s.owner.clone(),
            amount: 250,
            timestamp: s.env.ledger().timestamp(),
        }
    );
}

// ── Issue #759: Persistent storage TTL extension on balance updates ─────────

#[test]
fn test_extend_balance_ttl_does_not_panic() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &200);

    // Call extension helper
    s.client.extend_balance_ttl(&s.token.address, &s.owner);

    s.env.as_contract(&s.client.address, || {
        ttl::extend_balance_ttl(&s.env, &s.token.address, &s.owner);
    });
}

// ── Issue #760: InsufficientVaultLiquidity error variant on excess withdrawal ─

#[test]
fn test_insufficient_vault_liquidity_rejection() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &500);

    let recipient = Address::generate(&s.env);

    // Attempting to withdraw more than the vault has in liquidity returns InsufficientVaultLiquidity
    let res = s.client.try_withdraw(&s.owner, &recipient, &600);
    assert_eq!(res, Err(Ok(Error::InsufficientVaultLiquidity)));

    // withdraw_token also returns InsufficientVaultLiquidity
    let res_tok = s
        .client
        .try_withdraw_token(&s.owner, &s.token.address, &recipient, &600);
    assert_eq!(res_tok, Err(Ok(Error::InsufficientVaultLiquidity)));

    // Withdrawing within available liquidity succeeds
    let ok_res = s.client.try_withdraw(&s.owner, &recipient, &500);
    assert!(ok_res.is_ok());
}

// ── Issue #756: Enforce minimum deposit amount threshold per asset ──────────

#[test]
fn test_min_deposit_amount_threshold_enforcement() {
    let s = Setup::new(1_000_000);

    // Initial min deposit amount is 0
    assert_eq!(s.client.get_min_deposit_amount(&s.token.address), 0);
    assert_eq!(s.client.min_deposit_amount(&s.token.address), 0);

    // Non-owner cannot configure min deposit amount
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_set_min_deposit_amount(&stranger, &s.token.address, &100),
        Err(Ok(Error::NotAuthorized))
    );

    // Negative min deposit amount is rejected
    assert_eq!(
        s.client.try_set_min_deposit_amount(&s.owner, &s.token.address, &-5),
        Err(Ok(Error::InvalidAmount))
    );

    // Owner configures min deposit to 100
    let res = s.client.try_set_min_deposit_amount(&s.owner, &s.token.address, &100);
    assert!(res.is_ok());
    assert_eq!(s.client.get_min_deposit_amount(&s.token.address), 100);
    assert_eq!(s.client.min_deposit_amount(&s.token.address), 100);

    // Deposit below min (e.g. 50) is rejected with DepositBelowMin
    let res_below = s.client.try_deposit(&s.owner, &50);
    assert_eq!(res_below, Err(Ok(Error::DepositBelowMin)));

    // deposit_token below min is also rejected
    let res_tok_below = s.client.try_deposit_token(&s.owner, &s.token.address, &50);
    assert_eq!(res_tok_below, Err(Ok(Error::DepositBelowMin)));

    // Deposit at or above min (e.g. 100) succeeds
    let res_ok = s.client.try_deposit(&s.owner, &100);
    assert!(res_ok.is_ok());

    // batch_deposit with an entry below min is also rejected
    let deposits = soroban_sdk::vec![
        &s.env,
        crate::DepositSpec {
            from: s.owner.clone(),
            amount: 50,
        }
    ];
    let res_batch_below = s.client.try_batch_deposit(&deposits);
    assert_eq!(res_batch_below, Err(Ok(Error::DepositBelowMin)));
}

// ── Issue #893: governance-permissioned yield strategy sweeping ─────────────

#[test]
fn sweep_to_strategy_moves_idle_tokens_and_tracks_totals() {
    let s = Setup::new(1_000_000);
    let strategy = Address::generate(&s.env);
    s.client.deposit(&s.owner, &10_000);

    s.client.approve_strategy(&s.owner, &strategy, &true);
    assert!(s.client.is_strategy_approved(&strategy));

    let total = s
        .client
        .sweep_to_strategy(&s.owner, &s.token.address, &strategy, &4_000);
    assert_eq!(total, 4_000);
    assert_eq!(s.token.balance(&strategy), 4_000);
    assert_eq!(s.token.balance(&s.client.address), 6_000);

    let total = s
        .client
        .sweep_to_strategy(&s.owner, &s.token.address, &strategy, &1_000);
    assert_eq!(total, 5_000);
    assert_eq!(s.client.strategy_swept(&s.token.address, &strategy), 5_000);

    let swept_events = s
        .env
        .events()
        .all()
        .iter()
        .filter(|(contract, topics, _)| {
            contract == &s.client.address
                && topics.get(0).map(|t| {
                    <Symbol as soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>::try_from_val(
                        &s.env, &t,
                    )
                    .ok()
                        == Some(symbol_short!("swept"))
                }) == Some(true)
        })
        .count();
    assert!(swept_events >= 1, "sweep must publish a `swept` event");
}

#[test]
fn sweep_to_strategy_requires_governance_owner() {
    let s = Setup::new(1_000_000);
    let strategy = Address::generate(&s.env);
    s.client.deposit(&s.owner, &10_000);
    s.client.approve_strategy(&s.owner, &strategy, &true);

    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.user, &s.token.address, &strategy, &1_000),
        Err(Ok(Error::NotAuthorized))
    );
    assert_eq!(
        s.client.try_approve_strategy(&s.user, &strategy, &true),
        Err(Ok(Error::NotAuthorized))
    );
    assert_eq!(s.token.balance(&strategy), 0);
}

#[test]
fn sweep_to_strategy_rejects_unapproved_targets_and_tokens() {
    let s = Setup::new(1_000_000);
    let strategy = Address::generate(&s.env);
    s.client.deposit(&s.owner, &10_000);

    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &1_000),
        Err(Ok(Error::StrategyNotApproved))
    );

    s.client.approve_strategy(&s.owner, &strategy, &true);
    let other_token = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &other_token, &strategy, &1_000),
        Err(Ok(Error::TokenNotApprovedForVault))
    );

    // Revocation blocks further sweeps.
    s.client.approve_strategy(&s.owner, &strategy, &false);
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &1_000),
        Err(Ok(Error::StrategyNotApproved))
    );

    // The vault itself can never be approved as a strategy.
    assert_eq!(
        s.client
            .try_approve_strategy(&s.owner, &s.client.address, &true),
        Err(Ok(Error::InvalidParam))
    );
}

#[test]
fn sweep_to_strategy_validates_amount_liquidity_and_pause() {
    let s = Setup::new(1_000_000);
    let strategy = Address::generate(&s.env);
    s.client.deposit(&s.owner, &10_000);
    s.client.approve_strategy(&s.owner, &strategy, &true);

    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &0),
        Err(Ok(Error::InvalidAmount))
    );
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &10_001),
        Err(Ok(Error::InsufficientVaultLiquidity))
    );

    s.client.pause(&s.owner);
    assert_eq!(
        s.client
            .try_sweep_to_strategy(&s.owner, &s.token.address, &strategy, &1_000),
        Err(Ok(Error::VaultPaused))
    );
    assert_eq!(s.token.balance(&strategy), 0);
    assert_eq!(s.client.strategy_swept(&s.token.address, &strategy), 0);
}

// ── Issue #952: persistent entries are removed at zero balance ──────────────

#[test]
fn zero_balance_removes_persistent_balance_entries() {
    let s = Setup::new(1_000_000);
    s.client.deposit(&s.owner, &100);
    let recipient = Address::generate(&s.env);
    s.client
        .withdraw_token(&s.owner, &s.token.address, &recipient, &100);

    assert_eq!(
        s.client.get_token_account_balance(&s.token.address, &s.owner),
        0
    );
    assert_eq!(
        s.client.get_depositor_token_balance(&s.owner, &s.token.address),
        0
    );
    let contract = s.client.address.clone();
    s.env.as_contract(&contract, || {
        let acc_key =
            storage::DataKey::TokenAccountBalance(s.token.address.clone(), s.owner.clone());
        let dep_key = storage::DataKey::DepositorTokens(s.owner.clone(), s.token.address.clone());
        assert!(!s.env.storage().persistent().has(&acc_key));
        assert!(!s.env.storage().persistent().has(&dep_key));
    });
    // Tracked keys that were removed are skipped rather than renewed.
    assert_eq!(s.client.bump_all_vault_keys(), 0);
}

// ── Issue #949: per-token vault balance query ───────────────────────────────

#[test]
fn token_vault_balance_reports_holdings_per_token() {
    let s = Setup::new(1_000_000);
    assert_eq!(s.client.token_vault_balance(&s.token.address), 0);
    s.client.deposit(&s.owner, &100);
    assert_eq!(s.client.token_vault_balance(&s.token.address), 100);
    assert_eq!(
        s.client.token_vault_balance(&s.token.address),
        s.token.balance(&s.client.address)
    );
}


// ── Vault balance reconciliation under concurrent operations ────────────────

#[test]
fn vault_balance_reconciliation_concurrent_deposit_and_pause() {
    let s = Setup::new(1_000_000);

    // Perform a valid deposit before pause
    s.client.deposit(&s.owner, &100);
    assert_eq!(s.client.balance(), Some(100));
    assert_eq!(s.token.balance(&s.client.address), 100);

    // Record ledger balance before pause
    let balance_before_pause = s.token.balance(&s.client.address);
    let vault_ledger_before = s.client.balance();

    // Trigger emergency pause
    s.client.pause(&s.owner);
    assert!(s.client.is_paused());

    // Attempt deposit after pause — must fail and not modify state
    let result = s.client.try_deposit(&s.owner, &200);

    // Deposit attempt must reject due to pause
    assert_eq!(result, Err(Ok(Error::ContractPaused)));

    // Verify ledger consistency: vault balance unchanged
    assert_eq!(s.client.balance(), vault_ledger_before);
    assert_eq!(s.client.balance(), Some(100));

    // Verify token contract reserves unchanged
    assert_eq!(s.token.balance(&s.client.address), balance_before_pause);
    assert_eq!(s.token.balance(&s.client.address), 100);

    // Unpause and verify operations resume normally
    s.client.unpause(&s.owner);
    assert!(!s.client.is_paused());

    // After unpause, deposits should succeed and balance reconcile
    s.client.deposit(&s.owner, &300);
    assert_eq!(s.client.balance(), Some(400));
    assert_eq!(s.token.balance(&s.client.address), 400);

    // Simulate concurrent deposit attempts by depositing more
    s.client.deposit(&s.owner, &50);
    assert_eq!(s.client.balance(), Some(450));
    assert_eq!(s.token.balance(&s.client.address), 450);

    // Final reconciliation: ledger equals token contract reserves
    assert_eq!(s.client.balance(), Some(450));
    assert_eq!(s.token.balance(&s.client.address), 450);
}

#[test]
fn vault_balance_reconciliation_emergency_drain_consistency() {
    let s = Setup::new(1_000_000);
    let recipient = Address::generate(&s.env);

    // Build initial vault balance with multiple deposits
    s.client.deposit(&s.owner, &400);
    assert_eq!(s.client.balance(), Some(400));
    assert_eq!(s.token.balance(&s.client.address), 400);

    let initial_balance = s.client.balance();

    // Perform emergency pause (simulates emergency drain scenario)
    s.client.pause(&s.owner);

    // Verify balance is frozen during pause
    assert_eq!(s.client.balance(), initial_balance);
    assert_eq!(s.token.balance(&s.client.address), 400);

    // Attempt to withdraw during pause — must fail and preserve state
    let withdraw_result = s.client.try_withdraw(&s.owner, &recipient, &100);
    assert_eq!(withdraw_result, Err(Ok(Error::ContractPaused)));

    // Ledger must remain consistent: no partial drain
    assert_eq!(s.client.balance(), initial_balance);
    assert_eq!(s.client.balance(), Some(400));
    assert_eq!(s.token.balance(&s.client.address), 400);
    assert_eq!(s.token.balance(&recipient), 0);

    // Unpause and perform controlled withdrawal to verify post-pause integrity
    s.client.unpause(&s.owner);
    s.client.withdraw(&s.owner, &recipient, &100);

    // Verify consistent state after withdrawal: ledger matches token reserves
    assert_eq!(s.client.balance(), Some(300));
    assert_eq!(s.token.balance(&s.client.address), 300);
    assert_eq!(s.token.balance(&recipient), 100);
}
