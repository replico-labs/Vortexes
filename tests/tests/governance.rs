//! Token-weighted and quadratic through the hub, the hub's own checks, and
//! switching between models. Build first with `anchor build`.

use vortex_tests::*;

// ---------------------------------------------------------------
// Voting and executing through the hub
// ---------------------------------------------------------------

#[test]
fn full_lifecycle_pays_out_from_the_hub_treasury() {
    let (mut env, alice, bob) = dao_with_members();
    let d = env.hub_dao();
    assert_eq!((d.governance_program, d.governance, d.epoch), (TW, env.governance(TW), 1));
    assert_eq!(env.governance_account(TW).total_deposited, 1_000);
    assert_eq!(env.token_balance(&env.vault(TW)), 1_000);

    // The treasury also holds tokens; the proposal pays SOL and tokens.
    let host = Keypair::new();
    let creator = env.creator.insecure_clone();
    let treasury_ata = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &env.mint).owner(&env.treasury).send().unwrap();
    MintTo::new(&mut env.svm, &creator, &env.mint, &treasury_ata, 500).send().unwrap();
    let host_ata = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &env.mint).owner(&host.pubkey()).send().unwrap();
    let pay_tokens = stored(
        spl_token_interface::instruction::transfer_checked(&litesvm_token::TOKEN_ID, &treasury_ata, &env.mint, &host_ata, &env.treasury, &[], 250, 6).unwrap(),
    );
    let (id, p) = env.propose(TW, &alice, vec![env.pay_sol(&host.pubkey(), 1_000_000_000), pay_tokens]).unwrap();
    assert_eq!(id, 1);
    let prop = env.proposal_account(p);
    assert_eq!((prop.core.hub_dao, prop.core.epoch, prop.quorum_base), (env.dao, 1, 1_000));
    assert_eq!(env.state(TW, p), ProposalState::Pending);

    assert_eq!(code(env.vote(TW, &alice, p, VoteChoice::For)), err(GovError::ProposalNotActive), "voting not open yet");
    env.warp(START + 10);
    env.vote(TW, &alice, p, VoteChoice::For).unwrap();
    env.vote(TW, &bob, p, VoteChoice::Against).unwrap();
    assert_eq!(code(env.queue(TW, p)), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(env.state(TW, p), ProposalState::Succeeded, "60% for meets a 60% threshold");
    assert_eq!(code(env.execute(TW, p)), err(GovError::ProposalNotExecutable), "not queued");
    env.queue(TW, p).unwrap();
    assert_eq!(code(env.execute(TW, p)), err(GovError::ProposalNotExecutable), "timelock not over");

    env.warp(START + 160);
    env.execute(TW, p).unwrap();
    assert_eq!(env.lamports(&host.pubkey()), 1_000_000_000);
    assert_eq!(env.token_balance(&host_ata), 250);
    assert_eq!(env.token_balance(&treasury_ata), 250);
    assert_eq!(env.state(TW, p), ProposalState::Executed);
    assert_eq!(code(env.execute(TW, p)), err(GovError::ProposalAlreadyExecuted));
}

#[test]
fn one_vote_each_and_votes_lock_tokens_until_the_end() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(TW, &bob, p, VoteChoice::For).unwrap();
    assert!(env.vote(TW, &bob, p, VoteChoice::Against).is_err(), "second vote refused (vote record exists)");
    assert_eq!(env.proposal_account(p).tally.for_votes, 400);
    assert_eq!(env.voter_account(TW, &bob.kp.pubkey()).locked_until, START + 110);
    assert_eq!(code(env.withdraw(TW, &bob, 400)), err(GovError::TokensLocked));
    env.withdraw(TW, &alice, 100).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 500);
    env.warp(START + 110);
    assert_eq!(code(env.withdraw(TW, &bob, 401)), err(GovError::InsufficientDeposit));
    env.withdraw(TW, &bob, 400).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 1_000);
}

#[test]
fn proposing_needs_the_threshold_and_valid_instructions() {
    let (mut env, alice, _bob) = dao_with_members();
    let carol = env.member(1_000);
    assert_eq!(code(env.propose(TW, &carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)])), err(GovError::ProposalThresholdNotMet));
    env.deposit(TW, &carol, 99).unwrap();
    assert_eq!(code(env.propose(TW, &carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)])), err(GovError::ProposalThresholdNotMet));
    env.deposit(TW, &carol, 1).unwrap();
    env.propose(TW, &carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)]).unwrap();
    // Only the hub treasury can be asked to sign.
    let mut bad = env.pay_sol(&alice.kp.pubkey(), 1);
    bad.accounts[0].pubkey = alice.kp.pubkey();
    assert_eq!(code(env.propose(TW, &alice, vec![bad])), err(GovError::InvalidProposalSigner));
    assert_eq!(code(env.propose(TW, &alice, vec![])), err(GovError::EmptyProposalActions));
}

#[test]
fn defeated_proposals_cant_be_queued() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(TW, &alice, p, VoteChoice::Against).unwrap();
    env.vote(TW, &bob, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    assert_eq!(env.state(TW, p), ProposalState::Defeated);
    assert_eq!(code(env.queue(TW, p)), err(GovError::ApprovalThresholdNotMet));

    // Quorum: 10% of 1,050 is 105; a 50-token voter alone falls short.
    let (mut env, alice, _bob) = dao_with_members();
    let dave = env.member(1_000);
    env.deposit(TW, &dave, 50).unwrap();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&dave.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(TW, &dave, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    assert_eq!(code(env.queue(TW, p)), err(GovError::QuorumNotReached));
}

#[test]
fn abstain_counts_toward_quorum_only() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(TW, &alice, p, VoteChoice::Abstain).unwrap();
    env.vote(TW, &bob, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    env.queue(TW, p).unwrap();
}

#[test]
fn queued_proposals_expire() {
    let (mut env, alice, bob) = dao_with_members();
    let p = env.pass(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]);
    // queued at 110: executable from 160 through 360.
    env.warp(START + 361);
    assert_eq!(env.state(TW, p), ProposalState::Expired);
    assert_eq!(code(env.execute(TW, p)), err(GovError::ProposalExpired));
}

#[test]
fn only_the_proposer_or_the_dao_can_cancel() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    assert_eq!(code(env.cancel(TW, &bob.kp, p)), err(GovError::Unauthorized));
    env.cancel(TW, &alice.kp, p).unwrap();
    assert_eq!(env.state(TW, p), ProposalState::Cancelled);
    env.warp(START + 10);
    assert_eq!(code(env.vote(TW, &bob, p, VoteChoice::For)), err(GovError::ProposalNotActive));
}

#[test]
fn rules_change_only_through_a_passed_proposal() {
    let (mut env, alice, _bob) = dao_with_members();
    let new_config = VotingConfig { quorum_bps: 2_000, voting_period: 300, ..config() };
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    let direct = Instruction {
        program_id: TW,
        accounts: tw::accounts::UpdateConfig { treasury: impostor.pubkey(), governance: env.governance(TW) }.to_account_metas(None),
        data: tw::instruction::UpdateConfig { config: new_config }.data(),
    };
    assert_eq!(code(env.send(&[direct], &[&impostor])), err(GovError::Unauthorized));

    let bad = env.pass(TW, &alice, vec![env.update_config_ix(TW, VotingConfig { quorum_bps: 0, ..config() })]);
    assert_eq!(code(env.execute(TW, bad)), err(GovError::InvalidQuorum), "invalid rules refused even from the DAO");
    assert!(!env.proposal_account(bad).executed, "a failed execution changes nothing");
    let good = env.pass(TW, &alice, vec![env.update_config_ix(TW, new_config)]);
    env.execute(TW, good).unwrap();
    assert_eq!(env.governance_account(TW).config, new_config);
}

#[test]
fn a_proposal_cant_execute_itself_twice() {
    let (mut env, alice, _bob) = dao_with_members();
    // Proposal #1's only instruction is "execute proposal #1" on the hub.
    let p1 = env.proposal_pda(TW, 1);
    let reenter = stored(Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::Execute {
            dao: env.dao,
            executor: hub::executor_address(&env.dao),
            governance_program: TW,
            governance: env.governance(TW),
            proposal: p1,
        }
        .to_account_metas(None),
        data: hub::instruction::Execute {}.data(),
    });
    let p = env.pass(TW, &alice, vec![reenter]);
    assert_eq!(p, p1);
    assert_eq!(code(env.execute(TW, p1)), err(GovError::ProposalAlreadyExecuted), "the inner call sees it as executed already");
    assert!(!env.proposal_account(p1).executed);
}

#[test]
fn vote_records_can_be_closed_after_voting() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(TW, &bob, p, VoteChoice::For).unwrap();
    let record = env.vote_pda(TW, &p, &bob.kp.pubkey());
    let close = |who: &Keypair| Instruction {
        program_id: TW,
        accounts: tw::accounts::CloseVoteRecord { owner: who.pubkey(), proposal: p, vote_record: record }.to_account_metas(None),
        data: tw::instruction::CloseVoteRecord {}.data(),
    };
    assert_eq!(code(env.send(&[close(&bob.kp)], &[&bob.kp])), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(code(env.send(&[close(&alice.kp)], &[&alice.kp])), err(GovError::Unauthorized));
    let rent = env.lamports(&record);
    let before = env.lamports(&bob.kp.pubkey());
    env.send(&[close(&bob.kp)], &[&bob.kp]).unwrap();
    assert_eq!(env.lamports(&record), 0);
    assert_eq!(env.lamports(&bob.kp.pubkey()), before + rent - 5_000);
}

#[test]
fn deposits_must_use_the_daos_own_vault_and_mint() {
    let (mut env, alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let other_mint = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let attacker = Keypair::new().pubkey();
    let fake_vault = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &other_mint).owner(&attacker).send().unwrap();
    let alice_other = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &other_mint).owner(&alice.kp.pubkey()).send().unwrap();
    MintTo::new(&mut env.svm, &creator, &other_mint, &alice_other, 1_000).send().unwrap();
    let ix = Instruction {
        program_id: TW,
        accounts: tw::accounts::Deposit {
            owner: alice.kp.pubkey(),
            governance: env.governance(TW),
            voter: env.voter_pda(TW, &alice.kp.pubkey()),
            owner_token_account: alice_other,
            vault: fake_vault,
            mint: other_mint,
            token_program: litesvm_token::TOKEN_ID,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: tw::instruction::Deposit { amount: 1_000 }.data(),
    };
    assert_eq!(code(env.send(&[ix], &[&alice.kp])), err(GovError::WrongDao));
}

#[test]
fn works_with_token_2022_mints() {
    let token_2022: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();
    let mut env = Env::with_token_program(token_2022);
    assert_eq!(env.svm.get_account(&env.vault(TW)).unwrap().owner, token_2022);
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(TW, &alice, 700).unwrap();
    env.deposit(TW, &bob, 300).unwrap();
    env.svm.airdrop(&env.treasury, 1_000_000_000).unwrap();
    let p = env.pass(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 123)]);
    let before = env.lamports(&bob.kp.pubkey());
    env.execute(TW, p).unwrap();
    assert_eq!(env.lamports(&bob.kp.pubkey()), before + 123);
    env.withdraw(TW, &alice, 700).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 1_000);
    assert_eq!(env.token_balance(&env.vault(TW)), 300);
}

// ---------------------------------------------------------------
// The hub itself
// ---------------------------------------------------------------

#[test]
fn only_the_hub_can_confirm_executions() {
    let (mut env, alice, bob) = dao_with_members();
    let p = env.pass(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]);
    // Calling confirm_execution directly (to mark it executed without
    // running it, or to fake a confirmation) is refused.
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    let ix = Instruction {
        program_id: TW,
        accounts: tw::accounts::ConfirmExecution { executor: impostor.pubkey(), governance: env.governance(TW), proposal: p }.to_account_metas(None),
        data: tw::instruction::ConfirmExecution { epoch: 1 }.data(),
    };
    assert_eq!(code(env.send(&[ix], &[&impostor])), err(GovError::Unauthorized));
    assert!(!env.proposal_account(p).executed);
    // The discriminator the hub uses matches the program's own.
    assert_eq!(vortex_core::CONFIRM_EXECUTION_DISCRIMINATOR.to_vec(), tw::instruction::ConfirmExecution { epoch: 0 }.data()[..8].to_vec());
    env.execute(TW, p).unwrap();
}

#[test]
fn the_hub_only_trusts_the_daos_own_governance_program() {
    let (mut env, alice, bob) = dao_with_members();
    let p = env.pass(TW, &alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]);
    // Point execute at quadratic (approved, but not this DAO's program).
    let mut accounts = hub::accounts::Execute {
        dao: env.dao,
        executor: hub::executor_address(&env.dao),
        governance_program: QV,
        governance: env.governance(TW),
        proposal: p,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new(env.treasury, false));
    accounts.push(AccountMeta::new(bob.kp.pubkey(), false));
    accounts.push(AccountMeta::new_readonly(SYSTEM, false));
    let ix = Instruction { program_id: hub::ID, accounts, data: hub::instruction::Execute {}.data() };
    let admin = env.admin.insecure_clone();
    assert_eq!(code(env.send(&[ix], &[&admin])), err(GovError::NotActiveGovernance));
}

#[test]
fn only_approved_models_and_only_the_admin_approves() {
    let mut env = Env::new();
    // A random program isn't approved: no model record exists for it.
    let rogue = Keypair::new().pubkey();
    let key = Keypair::new();
    let creator = env.creator.insecure_clone();
    let ix = env.ix_create_dao("Rogue", rogue, &key.pubkey());
    assert!(env.send(&[ix], &[&creator, &key]).is_err());
    // Only the admin edits the list.
    let not_admin = Keypair::new();
    env.svm.airdrop(&not_admin.pubkey(), 1_000_000_000).unwrap();
    let mut ix = env.ix_set_model(rogue, "rogue", true);
    ix.accounts[0].pubkey = not_admin.pubkey();
    assert_eq!(code(env.send(&[ix], &[&not_admin])), err(GovError::Unauthorized));
    // A disabled model can't take new DAOs.
    let admin = env.admin.insecure_clone();
    env.send(&[env.ix_set_model(QV, "quadratic", false)], &[&admin]).unwrap();
    let key = Keypair::new();
    let ix = env.ix_create_dao("Quad", QV, &key.pubkey());
    assert_eq!(code(env.send(&[ix], &[&creator, &key])), err(GovError::ModelNotApproved));
}

#[test]
fn only_the_creator_sets_up_a_new_daos_governance() {
    let mut env = Env::new();
    let key = Keypair::new();
    let creator = env.creator.insecure_clone();
    let create = env.ix_create_dao("Second", TW, &key.pubkey());
    env.send(&[create], &[&creator, &key]).unwrap();
    env.dao = Pubkey::find_program_address(&[hub::DAO_SEED, key.pubkey().as_ref()], &hub::ID).0;
    let squatter = Keypair::new();
    env.svm.airdrop(&squatter.pubkey(), 1_000_000_000).unwrap();
    let ix = env.ix_init_governance(TW, &squatter.pubkey(), &squatter.pubkey(), config());
    assert_eq!(code(env.send(&[ix], &[&squatter])), err(GovError::Unauthorized));
    // Nor can anyone set it up in a program that isn't running the DAO.
    let ix = env.ix_init_governance(QV, &creator.pubkey(), &creator.pubkey(), config());
    assert_eq!(code(env.send(&[ix], &[&creator])), err(GovError::NotActiveGovernance));
}

// ---------------------------------------------------------------
// Switching governance model
// ---------------------------------------------------------------

#[test]
fn switching_to_quadratic_keeps_the_treasury() {
    let (mut env, alice, bob) = dao_with_members();
    let treasury_before = env.treasury;
    // Something left queued under token-weighted, to check it can't run later.
    let leftover = env.pass(TW, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 7)]);

    // Only a passed proposal can start a switch.
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    let mut direct = env.ix_propose_switch(QV);
    direct.accounts[0] = AccountMeta::new_readonly(impostor.pubkey(), true);
    assert!(env.send(&[direct], &[&impostor]).is_err());

    // One proposal: start the switch, and set the DAO up in quadratic (the treasury pays the rent).
    let init_quadratic = stored(env.ix_init_governance(QV, &env.treasury, &env.treasury, config()));
    let switch = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(QV)), init_quadratic]);
    env.execute(TW, switch).unwrap();
    let d = env.hub_dao();
    let pending = d.pending_switch.clone().expect("switch pending");
    assert_eq!(pending.program, QV);
    assert_eq!(d.governance_program, TW, "token-weighted still in charge during the delay");
    assert_eq!(env.governance_account(QV).hub_dao, env.dao);

    assert_eq!(code(env.apply_switch(QV)), err(GovError::SwitchNotReady));
    env.warp(pending.ready_at);
    env.apply_switch(QV).unwrap();
    let d = env.hub_dao();
    assert_eq!((d.governance_program, d.governance, d.epoch, d.pending_switch), (QV, env.governance(QV), 2, None));
    assert_eq!(env.treasury, treasury_before);
    assert_eq!(hub::treasury_address(&env.dao), treasury_before, "same treasury address");

    // Token-weighted can't run anything any more, including the leftover.
    assert_eq!(code(env.execute(TW, leftover)), err(GovError::NotActiveGovernance));
    assert_eq!(code(env.propose(TW, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 1)])), err(GovError::NotActiveGovernance));
    // Members take their tokens back from the old vault.
    env.withdraw(TW, &alice, 600).unwrap();
    env.withdraw(TW, &bob, 400).unwrap();

    // Quadratic: 900 tokens -> 30 votes, 100 tokens -> 10 votes.
    env.deposit(QV, &alice, 900).unwrap();
    env.deposit(QV, &bob, 100).unwrap();
    let carol = Keypair::new().pubkey();
    let (_, p) = env.propose(QV, &alice, vec![env.pay_sol(&carol, 2_000_000)]).unwrap();
    assert_eq!(env.proposal_account(p).core.epoch, 2);
    assert_eq!(env.proposal_account(p).quorum_base, 31, "sqrt(1000)");
    let prop = env.proposal_account(p);
    env.warp(prop.voting_starts_at);
    env.vote(QV, &alice, p, VoteChoice::For).unwrap();
    env.vote(QV, &bob, p, VoteChoice::Against).unwrap();
    let t = env.proposal_account(p).tally;
    assert_eq!((t.for_votes, t.against_votes), (30, 10));
    env.warp(prop.voting_ends_at);
    env.queue(QV, p).unwrap();
    env.warp(prop.voting_ends_at + config().timelock as i64);
    env.execute(QV, p).unwrap();
    assert_eq!(env.lamports(&carol), 2_000_000, "paid from the same treasury");
}

#[test]
fn a_pending_switch_can_be_cancelled_and_needs_an_approved_model() {
    let (mut env, alice, _bob) = dao_with_members();
    let admin = env.admin.insecure_clone();
    // Can't switch to a disabled model, or to the one already running.
    env.send(&[env.ix_set_model(QV, "quadratic", false)], &[&admin]).unwrap();
    let p = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(QV))]);
    assert_eq!(code(env.execute(TW, p)), err(GovError::ModelNotApproved));
    let p = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(TW))]);
    assert_eq!(code(env.execute(TW, p)), err(GovError::AlreadyActive));

    env.send(&[env.ix_set_model(QV, "quadratic", true)], &[&admin]).unwrap();
    let start = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(QV))]);
    env.execute(TW, start).unwrap();
    assert!(env.hub_dao().pending_switch.is_some());
    // The members change their minds during the delay.
    let stop = env.pass(TW, &alice, vec![env.ix_cancel_switch()].into_iter().map(stored).collect());
    env.execute(TW, stop).unwrap();
    assert!(env.hub_dao().pending_switch.is_none());
    assert_eq!(code(env.apply_switch(QV)), err(GovError::NoSwitchPending));

    // A switch can't be applied before the new program is set up.
    let again = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(QV))]);
    env.execute(TW, again).unwrap();
    // ...and the creator can't set it up on the DAO's behalf: after the
    // first setup, only the DAO itself (its treasury) can.
    let creator = env.creator.insecure_clone();
    let ix = env.ix_init_governance(QV, &creator.pubkey(), &creator.pubkey(), config());
    assert_eq!(code(env.send(&[ix], &[&creator])), err(GovError::Unauthorized));
    let ready = env.hub_dao().pending_switch.unwrap().ready_at;
    env.warp(ready);
    assert_eq!(code(env.apply_switch(QV)), err(GovError::GovernanceNotInitialized));
}

#[test]
fn switching_back_leaves_old_proposals_dead() {
    let (mut env, alice, _bob) = dao_with_members();
    let leftover = env.pass(TW, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 7)]);

    // token-weighted -> quadratic
    let init_quadratic = stored(env.ix_init_governance(QV, &env.treasury, &env.treasury, config()));
    let s = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(QV)), init_quadratic]);
    env.execute(TW, s).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(QV).unwrap();

    // quadratic -> back to token-weighted (already set up there, deposits intact).
    env.deposit(QV, &alice, 400).unwrap();
    let back = env.pass(QV, &alice, vec![stored(env.ix_propose_switch(TW))]);
    env.execute(QV, back).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(TW).unwrap();
    assert_eq!(env.hub_dao().epoch, 3);

    // The token-weighted proposal from epoch 1 stays dead.
    assert_eq!(env.proposal_account(leftover).core.epoch, 1);
    assert_eq!(code(env.execute(TW, leftover)), err(GovError::StaleProposal));
    // New ones work, with alice's token-weighted deposit still there.
    assert_eq!(env.voter_account(TW, &alice.kp.pubkey()).amount, 600);
    let p = env.pass(TW, &alice, vec![env.pay_sol(&alice.kp.pubkey(), 9)]);
    assert_eq!(env.proposal_account(p).core.epoch, 3);
    env.execute(TW, p).unwrap();
}

#[test]
fn one_dao_through_three_models_keeps_one_treasury() {
    let (mut env, alice, _bob) = dao_with_members();
    let treasury = env.treasury;
    let s1 = Keypair::new();
    let s2 = Keypair::new();
    for s in [&s1, &s2] {
        env.svm.airdrop(&s.pubkey(), 10_000_000_000).unwrap();
    }

    // token-weighted -> board (2 of 2), set up by the treasury in the same proposal.
    let init_board = stored(env.ix_init_board(&env.treasury, &env.treasury, vec![s1.pubkey(), s2.pubkey()], board_config(2)));
    let to_board = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(BOARD)), init_board]);
    env.execute(TW, to_board).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(BOARD).unwrap();
    assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (BOARD, 2));

    // The board pays someone, then moves the DAO to optimistic.
    let paid = Keypair::new().pubkey();
    let pay = env.board_propose(&s1, vec![env.pay_sol(&paid, 1_000_000)]).unwrap();
    env.board_confirm(&s2, pay).unwrap();
    let init_opt = stored(env.ix_init_optimistic(&env.treasury, &env.treasury, opt_config()));
    let to_opt = env.board_propose(&s1, vec![stored(env.ix_propose_switch(OPT)), init_opt]).unwrap();
    env.board_confirm(&s2, to_opt).unwrap();
    let t = env.now();
    env.warp(t + 50);
    env.execute(BOARD, pay).unwrap();
    env.execute(BOARD, to_opt).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(OPT).unwrap();
    assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (OPT, 3));

    // Optimistic pays from the same treasury.
    env.withdraw(TW, &alice, 600).unwrap();
    env.deposit(OPT, &alice, 600).unwrap();
    let (_, p) = env.propose(OPT, &alice, vec![env.pay_sol(&paid, 2_000_000)]).unwrap();
    let t = env.now();
    env.warp(t + 30);
    env.finalize_unchallenged(p).unwrap();
    env.warp(t + 80);
    env.execute(OPT, p).unwrap();
    assert_eq!(env.lamports(&paid), 3_000_000);
    assert_eq!(env.treasury, treasury);
    assert_eq!(hub::treasury_address(&env.dao), treasury);
}
