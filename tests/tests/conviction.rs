//! Conviction governance through the hub: support builds conviction over
//! time; a proposal runs once it reaches its bar. Build first with
//! `anchor build`.

use vortex_tests::*;

/// A DAO run by conviction: Alice 600 and Bob 400 deposited, both with 400
/// more in their wallets; the treasury holds 5 SOL. Bar: 20% of 1000 = 200.
fn conviction_dao() -> (Env, Member, Member) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Patient", CONV, |env| env.ix_init_conviction(&creator, &creator, conv_config()));
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(CONV, &alice, 600).unwrap();
    env.deposit(CONV, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, alice, bob)
}

#[test]
fn sustained_support_passes_a_proposal() {
    let (mut env, alice, _bob) = conviction_dao();
    let host = Keypair::new().pubkey();
    let (_, p) = env.conv_propose(&alice, vec![env.pay_sol(&host, 1_000_000_000)], vec![Env::sol_budget(1_000_000_000)]).unwrap();
    assert_eq!(env.conv_proposal(p).required_conviction, 200);
    assert_eq!(env.conv_state(p), ConvictionState::Active);
    assert_eq!(code(env.queue(CONV, p)), err(GovError::ConvictionNotReached));

    env.support(&alice, p, None).unwrap();
    env.warp(START + 10);
    assert_eq!(env.conviction(p), 100, "10 per second");
    assert_eq!(code(env.queue(CONV, p)), err(GovError::ConvictionNotReached));
    env.warp(START + 20);
    assert_eq!(env.conviction(p), 200);
    env.queue(CONV, p).unwrap();
    assert_eq!(env.conv_state(p), ConvictionState::Queued);
    assert_eq!(code(env.queue(CONV, p)), err(GovError::ProposalAlreadyQueued));
    assert_eq!(code(env.execute(CONV, p)), err(GovError::ProposalNotExecutable), "timelock not over");
    env.warp(START + 70);
    env.execute(CONV, p).unwrap();
    assert_eq!(env.lamports(&host), 1_000_000_000);
    assert_eq!(env.conv_state(p), ConvictionState::Executed);
    assert_eq!(code(env.support(&alice, p, None)), err(GovError::ProposalAlreadyExecuted));
}

#[test]
fn a_spike_of_support_isnt_enough_and_conviction_falls_back() {
    let (mut env, alice, bob) = conviction_dao();
    let (_, p) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    env.support(&bob, p, None).unwrap();
    env.warp(START + 15);
    assert_eq!(env.conviction(p), 150);
    env.withdraw_support(&bob, p).unwrap();
    env.warp(START + 25);
    assert_eq!(env.conviction(p), 50, "falls 10 per second once support leaves");
    env.warp(START + 40);
    assert_eq!(env.conviction(p), 0);
    assert_eq!(code(env.queue(CONV, p)), err(GovError::ConvictionNotReached));

    // Support capped below the bar never gets there, however long it lasts.
    let (_, p2) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    let carol = env.member(150);
    env.deposit(CONV, &carol, 150).unwrap();
    env.support(&carol, p2, None).unwrap();
    env.warp(START + 10_000);
    assert_eq!(env.conviction(p2), 150);
    assert_eq!(code(env.queue(CONV, p2)), err(GovError::ConvictionNotReached));
}

#[test]
fn members_back_one_proposal_at_a_time() {
    let (mut env, alice, _bob) = conviction_dao();
    let (_, p1) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    let (_, p2) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 2)], vec![]).unwrap();
    env.support(&alice, p1, None).unwrap();
    assert_eq!(code(env.support(&alice, p1, None)), err(GovError::AlreadySupporting));
    // Moving support needs the proposal it's leaving.
    assert_eq!(code(env.support(&alice, p2, None)), err(GovError::NotSupporting));
    assert_eq!(code(env.withdraw_support(&alice, p2)), err(GovError::NotSupporting));
    env.warp(START + 10);
    env.support(&alice, p2, Some(p1)).unwrap();
    assert_eq!(env.conv_proposal(p1).total_support, 0);
    assert_eq!(env.conv_proposal(p2).total_support, 600);
    assert_eq!(env.conv_voter(&alice.kp.pubkey()).supporting, p2);
    assert_eq!(env.conviction(p1), 100, "kept what it had built, now falling");
    env.warp(START + 20);
    assert_eq!(env.conviction(p1), 0);
    assert_eq!(env.conviction(p2), 100);
}

#[test]
fn backing_tokens_stay_locked_until_support_is_withdrawn() {
    let (mut env, alice, _bob) = conviction_dao();
    let (_, p) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    env.support(&alice, p, None).unwrap();
    assert_eq!(code(env.withdraw(CONV, &alice, 1)), err(GovError::TokensLocked));
    // Tokens deposited later aren't backing anything, so they can leave.
    env.deposit(CONV, &alice, 100).unwrap();
    env.withdraw(CONV, &alice, 100).unwrap();
    assert_eq!(code(env.withdraw(CONV, &alice, 1)), err(GovError::TokensLocked));
    // Supporting again tops up with what was deposited since.
    env.deposit(CONV, &alice, 100).unwrap();
    env.support(&alice, p, None).unwrap();
    assert_eq!(env.conv_proposal(p).total_support, 700);
    assert_eq!(code(env.withdraw(CONV, &alice, 1)), err(GovError::TokensLocked));
    env.withdraw_support(&alice, p).unwrap();
    assert_eq!(env.conv_proposal(p).total_support, 0);
    env.withdraw(CONV, &alice, 700).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 1_000);
    assert_eq!(code(env.support(&alice, p, None)), err(GovError::NoVotingPower));
}

#[test]
fn the_bar_is_fixed_when_proposed_and_the_threshold_holds() {
    let (mut env, alice, _bob) = conviction_dao();
    let carol = env.member(1_000);
    assert_eq!(code(env.conv_propose(&carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)], vec![])), err(GovError::ProposalThresholdNotMet));
    let (_, p1) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    env.deposit(CONV, &carol, 1_000).unwrap();
    let (_, p2) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    assert_eq!(env.conv_proposal(p1).required_conviction, 200);
    assert_eq!(env.conv_proposal(p2).required_conviction, 400, "20% of 2000");
}

#[test]
fn rules_change_only_through_a_passed_proposal() {
    let (mut env, alice, bob) = conviction_dao();
    let direct = Instruction {
        program_id: CONV,
        accounts: conv::accounts::TreasuryOnly { treasury: alice.kp.pubkey(), governance: env.governance(CONV) }.to_account_metas(None),
        data: conv::instruction::UpdateConfig { config: conv_config() }.data(),
    };
    assert_eq!(code(env.send(&[direct], &[&alice.kp])), err(GovError::Unauthorized));

    let bad = ConvictionConfig { growth_rate: 0, ..conv_config() };
    let new = ConvictionConfig { growth_rate: 50, ..conv_config() };
    let (_, p_bad) = env.conv_propose(&alice, vec![env.conv_update_config_ix(bad)], vec![]).unwrap();
    let (_, p_new) = env.conv_propose(&alice, vec![env.conv_update_config_ix(new)], vec![]).unwrap();
    env.support(&alice, p_bad, None).unwrap();
    env.support(&bob, p_new, None).unwrap();
    env.warp(START + 20);
    env.queue(CONV, p_bad).unwrap();
    env.queue(CONV, p_new).unwrap();
    env.warp(START + 70);
    assert_eq!(code(env.execute(CONV, p_bad)), err(GovError::InvalidConfig));
    env.execute(CONV, p_new).unwrap();
    assert_eq!(env.conv_governance().config, new);
}

#[test]
fn cancelled_proposals_free_their_supporters() {
    let (mut env, alice, bob) = conviction_dao();
    let (_, p) = env.conv_propose(&alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)], vec![]).unwrap();
    env.support(&bob, p, None).unwrap();
    assert_eq!(code(env.cancel(CONV, &bob.kp, p)), err(GovError::Unauthorized));
    env.cancel(CONV, &alice.kp, p).unwrap();
    assert_eq!(env.conv_state(p), ConvictionState::Cancelled);
    assert_eq!(code(env.support(&alice, p, None)), err(GovError::ProposalAlreadyCancelled));
    assert_eq!(code(env.queue(CONV, p)), err(GovError::ProposalAlreadyCancelled));
    env.withdraw_support(&bob, p).unwrap();
    env.withdraw(CONV, &bob, 400).unwrap();
}

#[test]
fn a_token_weighted_dao_can_switch_to_conviction() {
    let (mut env, alice, bob) = dao_with_members();
    let init = stored(env.ix_init_conviction(&env.treasury, &env.treasury, conv_config()));
    let p = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(CONV)), init]);
    env.execute(TW, p).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(CONV).unwrap();
    assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (CONV, 2));

    env.withdraw(TW, &bob, 400).unwrap();
    env.deposit(CONV, &bob, 400).unwrap();
    let host = Keypair::new().pubkey();
    let (_, p) = env.conv_propose(&bob, vec![env.pay_sol(&host, 3_000_000)], vec![Env::sol_budget(3_000_000)]).unwrap();
    assert_eq!(env.conv_proposal(p).required_conviction, 100, "the 100 floor: 20% of 400 is 80");
    env.support(&bob, p, None).unwrap();
    let t = env.now();
    env.warp(t + 10);
    env.queue(CONV, p).unwrap();
    env.warp(t + 60);
    env.execute(CONV, p).unwrap();
    assert_eq!(env.lamports(&host), 3_000_000);
}
