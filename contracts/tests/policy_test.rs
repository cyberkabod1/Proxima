#![cfg(test)]

use soroban_sdk::testutils::Ledger as _;
use soroban_sdk::{token, Env, String};

use proxima_contracts::policy::{PolicyContract, PolicyContractClient, PolicyError};

fn create_test_env() -> Env {
    Env::default()
}

/// Helper: deploy the policy contract and return its client + a mock USDC issuer.
fn setup(env: &Env) -> (PolicyContractClient<'_>, soroban_sdk::Address) {
    let contract_id = env.register_contract(None, PolicyContract);
    let client = PolicyContractClient::new(env, &contract_id);
    let issuer = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(env);
    (client, issuer)
}

/// Helper: mint `amount` of token to `to` using the Stellar token contract.
fn mint_token(
    env: &Env,
    issuer: &soroban_sdk::Address,
    to: &soroban_sdk::Address,
    amount: i128,
) -> soroban_sdk::Address {
    let token_contract_id = env.register_stellar_asset_contract_v2(issuer.clone());
    let token_admin = token::StellarAssetClient::new(env, &token_contract_id.address());
    token_admin.mint(to, &amount);
    token_contract_id.address()
}

/// Helper: set up a policy contract + a funded token account ready to
/// call execute_payment. Returns (client, policy_id, agent, recipient, token_id).
fn setup_payment_env(
    env: &Env,
) -> (
    PolicyContractClient<'_>,
    u64,
    soroban_sdk::Address,
    soroban_sdk::Address,
    soroban_sdk::Address,
) {
    let contract_id = env.register_contract(None, PolicyContract);
    let client = PolicyContractClient::new(env, &contract_id);

    let issuer = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(env);
    let recipient = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(env);

    // Mint 100 USDC (1_000_000_000 stroops) to the policy contract to fund transfers.
    let token_id = mint_token(env, &issuer, &contract_id, 1_000_000_000);

    // Extend instance TTLs so advancing ledger sequence does not archive contracts in tests
    env.as_contract(&contract_id, || {
        env.storage().instance().extend_ttl(50_000, 50_000);
    });
    env.as_contract(&token_id, || {
        env.storage().instance().extend_ttl(50_000, 50_000);
    });

    let policy_id = client.create_policy(
        &agent,
        &500_000_i128,    // 0.05 USDC max per tx
        &10_000_000_i128, // 1.00 USDC daily limit
        &String::from_str(env, "USDC"),
        &token_id, // token address as issuer
        &None,
    );

    (client, policy_id, agent, recipient, token_id)
}

// ─── Happy Path Tests ────────────────────────────────────────────────────────

/// 1. test_create_policy_success — create a policy, verify all fields are stored correctly.
#[test]
fn test_create_policy_success() {
    let env = create_test_env();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let policy_id = client.create_policy(
        &agent,
        &500_000_i128,    // 0.05 USDC max per tx
        &10_000_000_i128, // 1.00 USDC daily limit
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );

    assert_eq!(policy_id, 1);

    let policy = client.get_policy(&policy_id);
    assert_eq!(policy.id, policy_id);
    assert_eq!(policy.agent, agent);
    assert_eq!(policy.max_per_tx, 500_000);
    assert_eq!(policy.daily_limit, 10_000_000);
    assert_eq!(policy.asset, String::from_str(&env, "USDC"));
    assert_eq!(policy.issuer, issuer);
    assert_eq!(policy.spent_today, 0);
    assert_eq!(policy.total_spent, 0);
    assert!(policy.is_active);
    assert_eq!(policy.allowed_recipient, None);
    assert_eq!(policy.last_reset_ledger, env.ledger().sequence());
    assert_eq!(policy.created_at, env.ledger().sequence());
}

/// 2. test_execute_payment_success — agent executes a payment within limits, verify token transfer happens.
#[test]
fn test_execute_payment_success() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, agent, recipient, token_id) = setup_payment_env(&env);
    let payment_amount = 100_000_i128; // 0.01 USDC

    let record = client.execute_payment(
        &policy_id,
        &recipient,
        &payment_amount,
        &String::from_str(&env, "API call #1"),
    );

    // Verify PaymentRecord values
    assert_eq!(record.policy_id, policy_id);
    assert_eq!(record.agent, agent);
    assert_eq!(record.recipient, recipient);
    assert_eq!(record.amount, payment_amount);
    assert_eq!(record.asset, String::from_str(&env, "USDC"));
    assert_eq!(record.memo, String::from_str(&env, "API call #1"));

    // Verify policy state updated
    let policy = client.get_policy(&policy_id);
    assert_eq!(policy.spent_today, payment_amount);
    assert_eq!(policy.total_spent, payment_amount);

    // Verify token transfer happened: recipient balance increased
    let token = token::Client::new(&env, &token_id);
    assert_eq!(token.balance(&recipient), payment_amount);
}

/// 3. test_revoke_policy_success — owner revokes policy, verify is_active becomes false.
#[test]
fn test_revoke_policy_success() {
    let env = create_test_env();
    env.mock_all_auths();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let policy_id = client.create_policy(
        &agent,
        &100_000_i128,
        &1_000_000_i128,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );

    assert!(client.get_policy(&policy_id).is_active);

    client.revoke_policy(&policy_id);

    let policy = client.get_policy(&policy_id);
    assert!(!policy.is_active);
}

/// 4. test_remaining_allowance_full — new policy has full daily limit available.
#[test]
fn test_remaining_allowance_full() {
    let env = create_test_env();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let daily_limit = 5_000_000_i128;
    let policy_id = client.create_policy(
        &agent,
        &100_000_i128,
        &daily_limit,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );

    assert_eq!(client.remaining_allowance(&policy_id), daily_limit);
}

/// 5. test_remaining_allowance_after_spend — after a payment, remaining allowance decreases correctly.
#[test]
fn test_remaining_allowance_after_spend() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, _agent, recipient, _token_id) = setup_payment_env(&env);

    let initial = client.remaining_allowance(&policy_id);
    assert_eq!(initial, 10_000_000);

    let spend_amount = 500_000_i128;
    client.execute_payment(
        &policy_id,
        &recipient,
        &spend_amount,
        &String::from_str(&env, "payment #1"),
    );

    assert_eq!(
        client.remaining_allowance(&policy_id),
        initial - spend_amount
    );
}

/// 6. test_daily_reset — advance ledger by 17,280+ sequences, verify spent_today resets to 0.
#[test]
fn test_daily_reset() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, _agent, recipient, _token_id) = setup_payment_env(&env);

    // Spend 400_000 stroops on day 1
    let spend_day_1 = 400_000_i128;
    client.execute_payment(
        &policy_id,
        &recipient,
        &spend_day_1,
        &String::from_str(&env, "day 1 payment"),
    );

    let policy_day_1 = client.get_policy(&policy_id);
    assert_eq!(policy_day_1.spent_today, spend_day_1);
    assert_eq!(client.remaining_allowance(&policy_id), 10_000_000 - spend_day_1);

    // Advance ledger sequence by 17,281 (exceeds LEDGERS_PER_DAY = 17,280)
    env.ledger().with_mut(|l| l.sequence_number += 17_281);

    // remaining_allowance should now reflect full daily reset
    assert_eq!(client.remaining_allowance(&policy_id), 10_000_000);

    // Execute next payment on new day
    let spend_day_2 = 250_000_i128;
    client.execute_payment(
        &policy_id,
        &recipient,
        &spend_day_2,
        &String::from_str(&env, "day 2 payment"),
    );

    // spent_today should have reset to 0 before adding day 2 spend
    let policy_day_2 = client.get_policy(&policy_id);
    assert_eq!(policy_day_2.spent_today, spend_day_2);
    // total_spent maintains cumulative spending across all days
    assert_eq!(policy_day_2.total_spent, spend_day_1 + spend_day_2);
    assert_eq!(client.remaining_allowance(&policy_id), 10_000_000 - spend_day_2);
}

/// 7. test_is_authorized_true — correct agent returns true.
#[test]
fn test_is_authorized_true() {
    let env = create_test_env();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let policy_id = client.create_policy(
        &agent,
        &100_000_i128,
        &1_000_000_i128,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );

    assert!(client.is_authorized(&policy_id, &agent));
}

/// 8. test_is_authorized_false — wrong agent or inactive policy returns false.
#[test]
fn test_is_authorized_false() {
    let env = create_test_env();
    env.mock_all_auths();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);
    let wrong_agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let policy_id = client.create_policy(
        &agent,
        &100_000_i128,
        &1_000_000_i128,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );

    // Unauthorized agent returns false
    assert!(!client.is_authorized(&policy_id, &wrong_agent));

    // Revoked (inactive) policy returns false even for the authorized agent
    client.revoke_policy(&policy_id);
    assert!(!client.is_authorized(&policy_id, &agent));

    // Nonexistent policy returns false
    assert!(!client.is_authorized(&9999, &agent));
}

/// 9. test_policy_count — increments correctly after each create_policy call.
#[test]
fn test_policy_count() {
    let env = create_test_env();
    let (client, issuer) = setup(&env);

    assert_eq!(client.policy_count(), 0);

    for i in 1..=3 {
        let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);
        client.create_policy(
            &agent,
            &100_000_i128,
            &1_000_000_i128,
            &String::from_str(&env, "USDC"),
            &issuer,
            &None,
        );
        assert_eq!(client.policy_count(), i);
    }
}

// ─── Error / Negative Tests ──────────────────────────────────────────────────

/// 10. test_payment_exceeds_per_tx_limit — amount > max_per_tx → ExceedsPerTxLimit.
#[test]
fn test_payment_exceeds_per_tx_limit() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, _agent, recipient, _token_id) = setup_payment_env(&env);

    // max_per_tx is 500_000; attempt to spend 600_000
    let res = client.try_execute_payment(
        &policy_id,
        &recipient,
        &600_000_i128,
        &String::from_str(&env, "too much"),
    );

    assert_eq!(res, Err(Ok(PolicyError::ExceedsPerTxLimit.into())));
}

/// 11. test_payment_exceeds_daily_limit — cumulative spend > daily_limit → ExceedsDailyLimit.
#[test]
fn test_payment_exceeds_daily_limit() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, _agent, recipient, _token_id) = setup_payment_env(&env);

    // daily_limit is 10_000_000; make 20 payments of 500_000 = 10_000_000 exactly
    for _ in 0..20 {
        client.execute_payment(
            &policy_id,
            &recipient,
            &500_000_i128,
            &String::from_str(&env, "batch"),
        );
    }

    // 21st payment would exceed daily limit
    let res = client.try_execute_payment(
        &policy_id,
        &recipient,
        &500_000_i128,
        &String::from_str(&env, "over limit"),
    );

    assert_eq!(res, Err(Ok(PolicyError::ExceedsDailyLimit.into())));
}

/// 12. test_payment_on_revoked_policy — execute_payment on inactive policy → PolicyInactive.
#[test]
fn test_payment_on_revoked_policy() {
    let env = create_test_env();
    env.mock_all_auths();

    let (client, policy_id, _agent, recipient, _token_id) = setup_payment_env(&env);

    client.revoke_policy(&policy_id);

    let res = client.try_execute_payment(
        &policy_id,
        &recipient,
        &100_000_i128,
        &String::from_str(&env, "revoked payment"),
    );

    assert_eq!(res, Err(Ok(PolicyError::PolicyInactive.into())));
}

/// 13. test_revoke_nonexistent_policy → PolicyNotFound.
#[test]
fn test_revoke_nonexistent_policy() {
    let env = create_test_env();
    env.mock_all_auths();
    let (client, _) = setup(&env);

    let res = client.try_revoke_policy(&9999);

    assert_eq!(res, Err(Ok(PolicyError::PolicyNotFound.into())));
}

/// 14. test_payment_wrong_recipient — policy has allowed_recipient set, different address passed → RecipientNotAllowed.
#[test]
fn test_payment_wrong_recipient() {
    let env = create_test_env();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, PolicyContract);
    let client = PolicyContractClient::new(&env, &contract_id);
    let issuer = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);
    let allowed = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);
    let wrong = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    let token_id = mint_token(&env, &issuer, &contract_id, 100_000_000);

    let policy_id = client.create_policy(
        &agent,
        &500_000_i128,
        &10_000_000_i128,
        &String::from_str(&env, "USDC"),
        &token_id,
        &Some(allowed),
    );

    let res = client.try_execute_payment(
        &policy_id,
        &wrong,
        &100_000_i128,
        &String::from_str(&env, "wrong recipient"),
    );

    assert_eq!(res, Err(Ok(PolicyError::RecipientNotAllowed.into())));
}

/// 15. test_create_policy_zero_amount — max_per_tx = 0 → InvalidAmount.
#[test]
fn test_create_policy_zero_amount() {
    let env = create_test_env();
    let (client, issuer) = setup(&env);
    let agent = <soroban_sdk::Address as soroban_sdk::testutils::Address>::generate(&env);

    // max_per_tx = 0 should return InvalidAmount
    let res_max_tx = client.try_create_policy(
        &agent,
        &0_i128,
        &1_000_000_i128,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );
    assert_eq!(res_max_tx, Err(Ok(PolicyError::InvalidAmount.into())));

    // daily_limit = 0 should also return InvalidAmount
    let res_daily = client.try_create_policy(
        &agent,
        &100_000_i128,
        &0_i128,
        &String::from_str(&env, "USDC"),
        &issuer,
        &None,
    );
    assert_eq!(res_daily, Err(Ok(PolicyError::InvalidAmount.into())));
}
