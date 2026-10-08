//! Conviction's spending budgets: the asset list, the bar a budget sets,
//! and the record/check steps wrapped around every proposal. Build first
//! with `anchor build`.

use vortex_tests::*;

/// A conviction DAO (bar: 20% of deposits, at least 100) listing SOL and
/// its token with these weights. Alice 600 and Bob 400 deposited; the
/// treasury holds 5 SOL and 100 of the DAO's token.
fn budget_dao(sol_weight: u64, token_weight: u64) -> (Env, Member, Member) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Careful", CONV, |env| env.ix_init_conviction_weighted(&creator, &creator, conv_config(), sol_weight, token_weight));
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(CONV, &alice, 600).unwrap();
    env.deposit(CONV, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    let ata = env.treasury_ata();
    let creator = env.creator.insecure_clone();
    MintTo::new(&mut env.svm, &creator, &env.mint, &ata, 100).send().unwrap();
    (env, alice, bob)
}

fn tokens(env: &Env, amount: u64) -> AssetAmount {
    AssetAmount { mint: env.mint, amount }
}

/// The treasury sends `amount` of the DAO's token to `to`.
fn pay_tokens(env: &mut Env, to: &Pubkey, amount: u64) -> StoredInstruction {
    let from = env.treasury_ata();
    stored(spl_token_interface::instruction::transfer_checked(&env.token_program, &from, &env.mint, to, &env.treasury, &[], amount, 6).unwrap())
}

/// Alice backs `p` (moving her support from `previous`), waits until it
/// reaches its bar, and queues it.
fn back_and_queue(env: &mut Env, alice: &Member, p: Pubkey, previous: Option<Pubkey>) {
    env.support(alice, p, previous).unwrap();
    let wait = env.conv_proposal(p).required_conviction.div_ceil(conv_config().growth_rate);
    let t = env.now();
    env.warp(t + wait as i64);
    env.queue(CONV, p).unwrap();
}

#[test]
fn spending_within_budget_runs_and_overspending_reverts() {
    let (mut env, alice, _bob) = budget_dao(1_000, 500);
    let host = Keypair::new().pubkey();
    let sol = 1_000_000_000;
    let (_, ok) = env.conv_propose(&alice, vec![env.pay_sol(&host, sol)], vec![Env::sol_budget(sol)]).unwrap();
    let (_, over) = env.conv_propose(&alice, vec![env.pay_sol(&host, 2 * sol)], vec![Env::sol_budget(sol)]).unwrap();
    let (_, none) = env.conv_propose(&alice, vec![env.pay_sol(&host, 1)], vec![]).unwrap();
    assert_eq!(env.conv_proposal(ok).required_conviction, 400, "200 + 1000 x 1/5 of the SOL");
    assert_eq!(env.conv_proposal(none).required_conviction, 200);
    assert_eq!(env.core(ok).instructions.len(), 3, "record, pay, check");

    back_and_queue(&mut env, &alice, ok, None);
    back_and_queue(&mut env, &alice, over, Some(ok));
    back_and_queue(&mut env, &alice, none, Some(over));
    env.warp(START + 150);
    env.execute(CONV, ok).unwrap();
    assert_eq!(env.lamports(&host), sol);
    assert_eq!(code(env.execute(CONV, over)), err(GovError::BudgetExceeded), "2 SOL against a 1 SOL budget");
    assert_eq!(code(env.execute(CONV, none)), err(GovError::BudgetExceeded), "no budget, no spending");
    assert_eq!(env.lamports(&host), sol, "nothing moved");
    assert_eq!(env.lamports(&env.treasury), 4 * sol);
}

#[test]
fn token_budgets_and_delegates_are_checked() {
    let (mut env, alice, bob) = budget_dao(1_000, 500);
    let treasury_ata = env.treasury_ata();
    let pay = pay_tokens(&mut env, &bob.ata, 30);
    let (_, p_pay) = env.conv_propose(&alice, vec![pay], vec![tokens(&env, 30)]).unwrap();
    assert_eq!(env.conv_proposal(p_pay).required_conviction, 350, "200 + 500 x 30/100");
    // Spends nothing today, but would let Bob drain the account later.
    let approve = stored(spl_token_interface::instruction::approve(&env.token_program, &treasury_ata, &bob.kp.pubkey(), &env.treasury, &[], 100).unwrap());
    let (_, p_approve) = env.conv_propose(&alice, vec![approve], vec![]).unwrap();

    back_and_queue(&mut env, &alice, p_pay, None);
    back_and_queue(&mut env, &alice, p_approve, Some(p_pay));
    env.warp(env.now() + 50);
    env.execute(CONV, p_pay).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 630);
    assert_eq!(env.token_balance(&treasury_ata), 70);
    assert_eq!(code(env.execute(CONV, p_approve)), err(GovError::WatchedAccountChanged));
}

#[test]
fn the_bar_follows_the_budget_and_rule_changes_cost_everything() {
    let (mut env, alice, _bob) = budget_dao(1_000, 500);
    let me = alice.kp.pubkey();
    let bar = |env: &mut Env, ixs: Vec<StoredInstruction>, budget: Vec<AssetAmount>| {
        let (_, p) = env.conv_propose(&alice, ixs, budget).unwrap();
        env.conv_proposal(p).required_conviction
    };
    let pay = env.pay_sol(&me, 1);
    let (t30, t1000) = (tokens(&env, 30), tokens(&env, 1_000));
    assert_eq!(bar(&mut env, vec![pay.clone()], vec![Env::sol_budget(5_000_000_000)]), 1_200, "all the SOL");
    assert_eq!(bar(&mut env, vec![pay.clone()], vec![Env::sol_budget(1_000_000_000), t30]), 550);
    assert_eq!(bar(&mut env, vec![pay.clone()], vec![t1000]), 700, "capped at all 100 held");

    // Anything that could loosen the rules faces the bar of spending everything.
    let loosen = env.conv_update_config_ix(conv_config());
    assert_eq!(bar(&mut env, vec![loosen], vec![]), 1_700, "200 + 1000 + 500");
    let switch = stored(env.ix_propose_switch(TW));
    assert_eq!(bar(&mut env, vec![switch], vec![]), 1_700);
    // Protecting another asset doesn't.
    let creator = env.creator.insecure_clone();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let add = env.conv_add_asset_ix(usdc, 300);
    assert_eq!(bar(&mut env, vec![add], vec![]), 200);

    // Bad budgets and reserved instructions.
    let unlisted = AssetAmount { mint: usdc, amount: 1 };
    assert_eq!(code(env.conv_propose(&alice, vec![pay.clone()], vec![unlisted])), err(GovError::AssetNotListed));
    assert_eq!(code(env.conv_propose(&alice, vec![pay.clone()], vec![Env::sol_budget(1), Env::sol_budget(2)])), err(GovError::DuplicateBudgetAsset));
    assert_eq!(code(env.conv_propose(&alice, vec![pay.clone()], vec![Env::sol_budget(0)])), err(GovError::InvalidAmount));
    let mut sneaky = env.conv_admin_ix(conv::instruction::RecordBalances {}.data());
    sneaky.accounts.truncate(1);
    assert_eq!(code(env.conv_propose(&alice, vec![sneaky], vec![])), err(GovError::ReservedInstruction));
    assert_eq!(code(env.conv_propose(&alice, vec![pay.clone(); 7], vec![])), err(GovError::TooManyInstructions), "6 plus the 2 budget steps");
    env.conv_propose(&alice, vec![pay; 6], vec![]).unwrap();
}

#[test]
fn the_asset_list_changes_only_by_proposal_and_cuts_wait() {
    let (mut env, alice, _bob) = budget_dao(10, 10);
    let creator = env.creator.insecure_clone();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let direct = Instruction {
        program_id: CONV,
        accounts: conv::accounts::AddAsset { treasury: alice.kp.pubkey(), governance: env.governance(CONV), mint: usdc }.to_account_metas(None),
        data: conv::instruction::AddAsset { weight: 1 }.data(),
    };
    assert_eq!(code(env.send(&[direct], &[&alice.kp])), err(GovError::Unauthorized));

    // Made before USDC is listed, so it can't watch it.
    let (_, early) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![Env::sol_budget(1)]).unwrap();
    let add = env.conv_add_asset_ix(usdc, 30);
    let (_, list) = env.conv_propose(&alice, vec![add], vec![]).unwrap();
    back_and_queue(&mut env, &alice, early, None);
    back_and_queue(&mut env, &alice, list, Some(early));
    env.warp(env.now() + 50);
    env.execute(CONV, list).unwrap();
    assert_eq!(env.conv_governance().assets.len(), 3);
    assert_eq!(code(env.execute(CONV, early)), err(GovError::AssetListChanged));

    // Cut SOL's weight, raise USDC's, drop the DAO's token: the raise is
    // immediate, the rest wait a week.
    let mint = env.mint;
    let changes = vec![
        env.conv_admin_ix(conv::instruction::SetAssetWeight { mint: SOL, weight: 5 }.data()),
        env.conv_admin_ix(conv::instruction::SetAssetWeight { mint: usdc, weight: 50 }.data()),
        env.conv_admin_ix(conv::instruction::RemoveAsset { mint }.data()),
    ];
    let (_, p) = env.conv_propose(&alice, changes, vec![]).unwrap();
    assert_eq!(env.conv_proposal(p).required_conviction, 250, "200 + every weight: 10 + 10 + 30");
    let (_, no_sol) = env.conv_propose(&alice, vec![env.conv_admin_ix(conv::instruction::RemoveAsset { mint: SOL }.data())], vec![]).unwrap();
    back_and_queue(&mut env, &alice, p, Some(list));
    back_and_queue(&mut env, &alice, no_sol, Some(p));
    env.warp(env.now() + 50);
    env.execute(CONV, p).unwrap();
    assert_eq!(code(env.execute(CONV, no_sol)), err(GovError::CantRemoveSol));
    let weights = |env: &Env| env.conv_governance().assets.iter().map(|a| (a.mint, a.weight)).collect::<Vec<_>>();
    assert_eq!(weights(&env), vec![(SOL, 10), (mint, 10), (usdc, 50)]);
    assert_eq!(code(env.apply_asset_change(SOL)), err(GovError::AssetChangeNotDue));
    assert_eq!(code(env.apply_asset_change(usdc)), err(GovError::NoPendingAssetChange));
    let t = env.now();
    env.warp(t + conv::WEIGHT_CUT_DELAY);
    env.apply_asset_change(SOL).unwrap();
    env.apply_asset_change(mint).unwrap();
    assert_eq!(weights(&env), vec![(SOL, 5), (usdc, 50)]);
}
