//! Optimistic governance through the hub: unchallenged proposals pass after
//! the window; challenged ones go to a vote, and the bond goes to the
//! treasury or back to the challenger. Build first with `anchor build`.

use vortex_tests::*;

/// A DAO run by optimistic: Alice 600 and Bob 400 deposited, both with 400
/// more in their wallets; the treasury holds 5 SOL.
fn optimistic_dao() -> (Env, Member, Member) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Swift", OPT, |env| env.ix_init_optimistic(&creator, &creator, opt_config()));
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(OPT, &alice, 600).unwrap();
    env.deposit(OPT, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, alice, bob)
}

#[test]
fn unchallenged_proposals_pass_after_the_window() {
    let (mut env, alice, bob) = optimistic_dao();
    let host = Keypair::new().pubkey();
    let (_, p) = env.propose(OPT, &alice, vec![env.pay_sol(&host, 1_000_000_000)]).unwrap();
    assert_eq!(env.opt_state(p), OptimisticState::ChallengeWindow);
    assert_eq!(code(env.finalize_unchallenged(p)), err(GovError::ChallengeWindowOpen));
    assert_eq!(code(env.vote(OPT, &bob, p, VoteChoice::Against)), err(GovError::ProposalNotActive), "no vote unless challenged");

    env.warp(START + 30);
    assert_eq!(env.opt_state(p), OptimisticState::Succeeded);
    assert_eq!(code(env.challenge(&bob, p)), err(GovError::ChallengeWindowClosed));
    env.finalize_unchallenged(p).unwrap();
    assert_eq!(env.opt_state(p), OptimisticState::Queued);
    assert_eq!(code(env.execute(OPT, p)), err(GovError::ProposalNotExecutable), "timelock not over");
    env.warp(START + 80);
    env.execute(OPT, p).unwrap();
    assert_eq!(env.lamports(&host), 1_000_000_000);
    assert_eq!(env.opt_state(p), OptimisticState::Executed);
}

#[test]
fn a_successful_challenge_gets_its_bond_back() {
    let (mut env, alice, bob) = optimistic_dao();
    let (_, p) = env.propose(OPT, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.challenge(&bob, p).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 550, "bond taken from the wallet");
    assert_eq!(env.token_balance(&env.opt_bond_vault()), 50);
    assert_eq!(code(env.challenge(&alice, p)), err(GovError::AlreadyChallenged));
    assert_eq!(code(env.finalize_unchallenged(p)), err(GovError::AlreadyChallenged));
    assert_eq!(env.opt_state(p), OptimisticState::Active);

    // Only Bob votes, against: approval 0%.
    env.vote(OPT, &bob, p, VoteChoice::Against).unwrap();
    let treasury_ata = env.treasury_ata();
    assert_eq!(code(env.finalize_challenge(p, None, Some(bob.ata))), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(env.opt_state(p), OptimisticState::Defeated);
    // The bond must go back to the challenger, not anywhere else.
    assert_eq!(code(env.finalize_challenge(p, Some(treasury_ata), None)), err(GovError::Unauthorized));
    assert_eq!(code(env.finalize_challenge(p, None, Some(alice.ata))), err(GovError::Unauthorized));
    env.finalize_challenge(p, None, Some(bob.ata)).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 600);
    assert_eq!(code(env.finalize_challenge(p, None, Some(bob.ata))), err(GovError::BondAlreadyResolved));
    assert_eq!(code(env.execute(OPT, p)), err(GovError::ProposalNotExecutable));
}

#[test]
fn a_failed_challenge_forfeits_the_bond_to_the_treasury() {
    let (mut env, alice, bob) = optimistic_dao();
    let host = Keypair::new().pubkey();
    let (_, p) = env.propose(OPT, &alice, vec![env.pay_sol(&host, 7_000_000)]).unwrap();
    env.challenge(&bob, p).unwrap();
    env.vote(OPT, &alice, p, VoteChoice::For).unwrap();
    env.vote(OPT, &bob, p, VoteChoice::Against).unwrap();
    env.warp(START + 100);
    assert_eq!(env.opt_state(p), OptimisticState::Succeeded, "60% for meets 60%");
    let treasury_ata = env.treasury_ata();
    env.finalize_challenge(p, Some(treasury_ata), None).unwrap();
    assert_eq!(env.token_balance(&treasury_ata), 50);
    assert_eq!(env.token_balance(&bob.ata), 550);
    assert_eq!(env.opt_state(p), OptimisticState::Queued);
    env.warp(START + 150);
    env.execute(OPT, p).unwrap();
    assert_eq!(env.lamports(&host), 7_000_000);
}

#[test]
fn a_cancelled_proposals_bond_can_be_reclaimed() {
    let (mut env, alice, bob) = optimistic_dao();
    let (_, p) = env.propose(OPT, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)]).unwrap();
    env.challenge(&bob, p).unwrap();
    assert_eq!(code(env.reclaim_bond(p, bob.ata)), err(GovError::ProposalNotActive), "only once cancelled");
    env.cancel(OPT, &alice.kp, p).unwrap();
    assert_eq!(code(env.reclaim_bond(p, alice.ata)), err(GovError::Unauthorized), "only to the challenger");
    env.reclaim_bond(p, bob.ata).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 600);
    assert_eq!(code(env.reclaim_bond(p, bob.ata)), err(GovError::BondAlreadyResolved));
}

#[test]
fn optimistic_rules_and_threshold_hold() {
    let (mut env, alice, _bob) = optimistic_dao();
    let carol = env.member(1_000);
    assert_eq!(code(env.propose(OPT, &carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)])), err(GovError::ProposalThresholdNotMet));
    // Rules change only through a passed proposal; a zero challenge window is refused.
    let bad = OptimisticConfig { challenge_period: 0, ..opt_config() };
    let new = OptimisticConfig { challenge_bond: 0, ..opt_config() };
    let ix = |env: &Env, config: OptimisticConfig| {
        stored(Instruction {
            program_id: OPT,
            accounts: opt::accounts::UpdateConfig { treasury: env.treasury, governance: env.governance(OPT) }.to_account_metas(None),
            data: opt::instruction::UpdateConfig { config }.data(),
        })
    };
    let (_, p_bad) = env.propose(OPT, &alice, vec![ix(&env, bad)]).unwrap();
    let (_, p_new) = env.propose(OPT, &alice, vec![ix(&env, new)]).unwrap();
    env.warp(START + 30);
    env.finalize_unchallenged(p_bad).unwrap();
    env.finalize_unchallenged(p_new).unwrap();
    env.warp(START + 80);
    assert_eq!(code(env.execute(OPT, p_bad)), err(GovError::InvalidChallengePeriod));
    env.execute(OPT, p_new).unwrap();
    assert_eq!(env.opt_governance().config, new);
}
