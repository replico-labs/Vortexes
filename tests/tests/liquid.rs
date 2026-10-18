//! Liquid governance: delegation counted at once, overrides, one hop,
//! and the locks that stop the same tokens counting twice.

use vortex_liquid as liq;
use vortex_liquid::VoteChoice as Choice;
use vortex_tests::*;

const OVERRIDE_SEED: &[u8] = b"override";

/// A liquid DAO with four members: alice 400, bob 300, carol 200, dave 100.
fn liquid_dao() -> (Env, [Member; 4]) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Liquid", LIQ, |env| env.ix_init_governance(LIQ, &creator, &creator, config()));
    let members = [400, 300, 200, 100].map(|n| {
        let m = env.member(1_000);
        deposit(&mut env, &m, n, None).unwrap();
        m
    });
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, members)
}

fn voter(env: &Env, owner: &Pubkey) -> liq::Voter {
    let data = env.svm.get_account(&env.voter_pda(LIQ, owner)).unwrap().data;
    liq::Voter::try_deserialize(&mut &data[..]).unwrap()
}

fn proposal(env: &Env, p: Pubkey) -> liq::Proposal {
    let data = env.svm.get_account(&p).unwrap().data;
    liq::Proposal::try_deserialize(&mut &data[..]).unwrap()
}

fn tally(env: &Env, p: Pubkey) -> (u64, u64, u64) {
    let t = proposal(env, p).tally;
    (t.for_votes, t.against_votes, t.abstain_votes)
}

fn deposit(env: &mut Env, m: &Member, amount: u64, delegate: Option<&Pubkey>) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Deposit {
            owner: m.kp.pubkey(),
            governance: env.governance(LIQ),
            voter: env.voter_pda(LIQ, &m.kp.pubkey()),
            delegate_voter: delegate.map(|d| env.voter_pda(LIQ, d)),
            owner_token_account: m.ata,
            vault: env.vault(LIQ),
            mint: env.mint,
            token_program: env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: liq::instruction::Deposit { amount }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn withdraw(env: &mut Env, m: &Member, amount: u64, delegate: Option<&Pubkey>) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Withdraw {
            owner: m.kp.pubkey(),
            governance: env.governance(LIQ),
            voter: env.voter_pda(LIQ, &m.kp.pubkey()),
            delegate_voter: delegate.map(|d| env.voter_pda(LIQ, d)),
            owner_token_account: m.ata,
            vault: env.vault(LIQ),
            mint: env.mint,
            token_program: env.token_program,
        }
        .to_account_metas(None),
        data: liq::instruction::Withdraw { amount }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn delegate(env: &mut Env, m: &Member, to: &Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Delegate {
            owner: m.kp.pubkey(),
            governance: env.governance(LIQ),
            voter: env.voter_pda(LIQ, &m.kp.pubkey()),
            delegate_voter: env.voter_pda(LIQ, to),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: liq::instruction::Delegate { to: *to }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn undelegate(env: &mut Env, m: &Member, to: &Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Undelegate {
            owner: m.kp.pubkey(),
            governance: env.governance(LIQ),
            voter: env.voter_pda(LIQ, &m.kp.pubkey()),
            delegate_voter: env.voter_pda(LIQ, to),
        }
        .to_account_metas(None),
        data: liq::instruction::Undelegate {}.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn override_pda(p: &Pubkey, delegate: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[OVERRIDE_SEED, p.as_ref(), delegate.as_ref()], &LIQ).0
}

fn vote(env: &mut Env, m: &Member, p: Pubkey, choice: Choice) -> Result<(), FailedTransactionMetadata> {
    let owner = m.kp.pubkey();
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::CastVote {
            owner,
            governance: env.governance(LIQ),
            proposal: p,
            voter: env.voter_pda(LIQ, &owner),
            overrides: override_pda(&p, &owner),
            vote_record: env.vote_pda(LIQ, &p, &owner),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: liq::instruction::CastVote { choice }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn override_vote(env: &mut Env, m: &Member, delegate: &Pubkey, p: Pubkey, choice: Choice) -> Result<(), FailedTransactionMetadata> {
    let owner = m.kp.pubkey();
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::OverrideVote {
            owner,
            governance: env.governance(LIQ),
            proposal: p,
            voter: env.voter_pda(LIQ, &owner),
            delegate_voter: env.voter_pda(LIQ, delegate),
            delegate_record: env.vote_pda(LIQ, &p, delegate),
            overrides: override_pda(&p, delegate),
            vote_record: env.vote_pda(LIQ, &p, &owner),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: liq::instruction::OverrideVote { choice }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn propose(env: &mut Env, m: &Member) -> Result<Pubkey, FailedTransactionMetadata> {
    let g = env.governance(LIQ);
    let data = env.svm.get_account(&g).unwrap().data;
    let id = liq::Governance::try_deserialize(&mut &data[..]).unwrap().proposal_count + 1;
    let p = env.proposal_pda(LIQ, id);
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Propose {
            proposer: m.kp.pubkey(),
            governance: g,
            hub_dao: env.dao,
            voter: Some(env.voter_pda(LIQ, &m.kp.pubkey())),
            proposal: p,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: liq::instruction::Propose { id, metadata_uri: "Pay the host".into(), instructions: vec![env.pay_sol(&m.kp.pubkey(), 1_000)] }.data(),
    };
    env.send(&[ix], &[&m.kp]).map(|_| p)
}

fn queue(env: &mut Env, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: LIQ,
        accounts: liq::accounts::Queue { governance: env.governance(LIQ), proposal: p }.to_account_metas(None),
        data: liq::instruction::Queue {}.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

#[test]
fn a_delegates_vote_counts_delegated_tokens_at_once_and_runs_through_the_hub() {
    let (mut env, [alice, bob, carol, dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    delegate(&mut env, &bob, &a).unwrap();
    delegate(&mut env, &carol, &a).unwrap();
    let av = voter(&env, &a);
    assert_eq!((av.delegated_power, av.delegators), (500, 2));

    let p = propose(&mut env, &alice).unwrap();
    env.warp(START + 10);
    vote(&mut env, &alice, p, Choice::For).unwrap();
    assert_eq!(tally(&env, p), (900, 0, 0), "alice's 400 plus 500 delegated, in one vote");
    vote(&mut env, &dave, p, Choice::Against).unwrap();
    env.warp(START + 110);
    queue(&mut env, p).unwrap();
    env.warp(START + 160);
    let before = env.lamports(&a);
    env.execute(LIQ, p).unwrap();
    assert_eq!(env.lamports(&a) - before, 1_000, "paid from the hub treasury");
}

#[test]
fn overriding_after_the_delegate_voted_moves_the_weight() {
    let (mut env, [alice, bob, carol, _dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    delegate(&mut env, &bob, &a).unwrap();
    delegate(&mut env, &carol, &a).unwrap();
    let p = propose(&mut env, &alice).unwrap();
    env.warp(START + 10);
    vote(&mut env, &alice, p, Choice::For).unwrap();
    override_vote(&mut env, &carol, &a, p, Choice::Against).unwrap();
    assert_eq!(tally(&env, p), (700, 200, 0), "carol's 200 moved from For to Against");
    assert!(override_vote(&mut env, &carol, &a, p, Choice::For).is_err(), "one vote each");
    assert_eq!(code(vote(&mut env, &bob, p, Choice::Against)), err(GovError::AlreadyDelegated), "delegators vote by overriding");

    // Bob's tokens were counted in alice's open vote: locked until it closes.
    assert_eq!(code(undelegate(&mut env, &bob, &a)), err(GovError::DelegationLocked));
    assert_eq!(code(withdraw(&mut env, &bob, 10, Some(&a))), err(GovError::DelegationLocked));
    assert_eq!(code(deposit(&mut env, &bob, 10, Some(&a))), err(GovError::DelegationLocked));
    assert_eq!(code(withdraw(&mut env, &bob, 10, None)), err(GovError::WrongDelegate), "the delegate's account must come along");
    env.warp(START + 110);
    undelegate(&mut env, &bob, &a).unwrap();
    assert_eq!(voter(&env, &a).delegated_power, 200);
    withdraw(&mut env, &bob, 300, None).unwrap();
}

#[test]
fn overriding_before_the_delegate_votes_is_left_out_of_their_vote() {
    let (mut env, [alice, bob, _carol, _dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    delegate(&mut env, &bob, &a).unwrap();
    let p = propose(&mut env, &alice).unwrap();
    env.warp(START + 10);
    override_vote(&mut env, &bob, &a, p, Choice::Against).unwrap();
    vote(&mut env, &alice, p, Choice::For).unwrap();
    assert_eq!(tally(&env, p), (400, 300, 0), "alice's vote leaves out bob's 300");
    // Bob's own vote is still open: he can't undelegate or add tokens yet.
    env.warp(START + 20);
    assert_eq!(code(undelegate(&mut env, &bob, &a)), err(GovError::TokensLocked));
}

#[test]
fn a_delegation_made_after_the_delegate_voted_isnt_taken_from_that_vote() {
    let (mut env, [alice, _bob, _carol, dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    let p = propose(&mut env, &alice).unwrap();
    env.warp(START + 10);
    vote(&mut env, &alice, p, Choice::For).unwrap();
    delegate(&mut env, &dave, &a).unwrap();
    override_vote(&mut env, &dave, &a, p, Choice::Against).unwrap();
    assert_eq!(tally(&env, p), (400, 100, 0), "dave's 100 was never in alice's vote, so nothing moves");
}

#[test]
fn delegation_is_one_hop() {
    let (mut env, [alice, bob, carol, dave]) = liquid_dao();
    let (a, b) = (alice.kp.pubkey(), bob.kp.pubkey());
    // Self-delegation: Anchor already refuses the same account twice (2040, duplicate mutable account).
    assert!(delegate(&mut env, &alice, &a).is_err());
    delegate(&mut env, &bob, &a).unwrap();
    assert_eq!(code(delegate(&mut env, &bob, &carol.kp.pubkey())), err(GovError::AlreadyDelegated));
    assert_eq!(code(delegate(&mut env, &alice, &carol.kp.pubkey())), err(GovError::HasDelegators), "alice holds bob's votes");
    assert_eq!(code(delegate(&mut env, &dave, &b)), err(GovError::DelegateHasDelegated), "bob passed his on");
    // Someone who never deposited can still be a delegate.
    let outsider = Keypair::new();
    delegate(&mut env, &carol, &outsider.pubkey()).unwrap();
    assert_eq!(voter(&env, &outsider.pubkey()).delegated_power, 200);
}

#[test]
fn delegators_cant_vote_directly_nor_override_without_a_delegate() {
    let (mut env, [alice, bob, _carol, dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    let p = propose(&mut env, &alice).unwrap();
    env.warp(START + 10);
    // Dave has no delegate: the delegate accounts' addresses don't match (2006, seeds).
    assert!(override_vote(&mut env, &dave, &a, p, Choice::For).is_err());
    vote(&mut env, &bob, p, Choice::For).unwrap();
    // Bob voted himself on an open proposal: he can't delegate until it closes.
    assert_eq!(code(delegate(&mut env, &bob, &a)), err(GovError::TokensLocked));
}

#[test]
fn delegated_tokens_count_toward_the_proposal_threshold() {
    let (mut env, [alice, _bob, _carol, _dave]) = liquid_dao();
    let small = env.member(1_000);
    deposit(&mut env, &small, 60, None).unwrap();
    assert_eq!(code(propose(&mut env, &small)), err(GovError::ProposalThresholdNotMet), "60 < 100");
    let helper = env.member(1_000);
    deposit(&mut env, &helper, 50, None).unwrap();
    delegate(&mut env, &helper, &small.kp.pubkey()).unwrap();
    propose(&mut env, &small).unwrap();
    // A delegator has given their votes away, so can't propose with them.
    delegate(&mut env, &alice, &small.kp.pubkey()).unwrap();
    assert_eq!(code(propose(&mut env, &alice)), err(GovError::ProposalThresholdNotMet));
}

#[test]
fn deposits_and_withdrawals_keep_the_delegates_total_in_step() {
    let (mut env, [alice, bob, _carol, _dave]) = liquid_dao();
    let a = alice.kp.pubkey();
    delegate(&mut env, &bob, &a).unwrap();
    deposit(&mut env, &bob, 100, Some(&a)).unwrap();
    assert_eq!(voter(&env, &a).delegated_power, 400);
    withdraw(&mut env, &bob, 250, Some(&a)).unwrap();
    assert_eq!(voter(&env, &a).delegated_power, 150);
    assert_eq!(code(deposit(&mut env, &bob, 1, Some(&_carol.kp.pubkey()))), err(GovError::WrongDelegate), "someone else's account as the delegate");
}

#[test]
fn a_token_weighted_dao_can_switch_to_liquid() {
    let (mut env, alice, bob) = dao_with_members();
    let init_liquid = stored(env.ix_init_governance(LIQ, &env.treasury, &env.treasury, config()));
    let switch = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(LIQ)), init_liquid]);
    env.execute(TW, switch).unwrap();
    let ready = env.hub_dao().pending_switch.expect("switch pending").ready_at;
    env.warp(ready);
    env.apply_switch(LIQ).unwrap();
    assert_eq!(env.hub_dao().governance_program, LIQ);

    // Members move their tokens over and delegate; the same treasury pays out.
    env.withdraw(TW, &alice, 600).unwrap();
    env.withdraw(TW, &bob, 400).unwrap();
    deposit(&mut env, &alice, 600, None).unwrap();
    deposit(&mut env, &bob, 400, None).unwrap();
    delegate(&mut env, &bob, &alice.kp.pubkey()).unwrap();
    let p = propose(&mut env, &alice).unwrap();
    let prop = proposal(&env, p);
    env.warp(prop.voting_starts_at);
    vote(&mut env, &alice, p, Choice::For).unwrap();
    assert_eq!(tally(&env, p), (1_000, 0, 0));
    env.warp(prop.voting_ends_at);
    queue(&mut env, p).unwrap();
    env.warp(prop.voting_ends_at + config().timelock as i64);
    env.execute(LIQ, p).unwrap();
}
