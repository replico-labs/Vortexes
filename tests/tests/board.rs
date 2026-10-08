//! Board governance through the hub: signers confirm; M of N queues a
//! proposal; signers and rules change only through proposals. Build first
//! with `anchor build`.

use vortex_tests::*;

/// A DAO run by a 2-of-3 board; the treasury holds 5 SOL.
fn board_dao() -> (Env, [Keypair; 3]) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let signers = [Keypair::new(), Keypair::new(), Keypair::new()];
    for s in &signers {
        env.svm.airdrop(&s.pubkey(), 10_000_000_000).unwrap();
    }
    let keys = signers.iter().map(|s| s.pubkey()).collect::<Vec<_>>();
    let creator = env.creator.pubkey();
    env.create_dao("Council", BOARD, |env| env.ix_init_board(&creator, &creator, keys, board_config(2)));
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, signers)
}

#[test]
fn two_of_three_signers_run_a_proposal() {
    let (mut env, [s1, s2, s3]) = board_dao();
    let host = Keypair::new().pubkey();
    let outsider = Keypair::new();
    env.svm.airdrop(&outsider.pubkey(), 1_000_000_000).unwrap();
    assert_eq!(code(env.board_propose(&outsider, vec![env.pay_sol(&host, 1)])), err(GovError::NotSigner));

    let p = env.board_propose(&s1, vec![env.pay_sol(&host, 1_000_000_000)]).unwrap();
    assert_eq!(env.board_proposal(p).confirmations, vec![s1.pubkey()], "proposing confirms");
    assert_eq!(env.board_state(p), BoardState::Active);
    assert_eq!(code(env.board_confirm(&s1, p)), err(GovError::AlreadyConfirmed));
    assert_eq!(code(env.board_confirm(&outsider, p)), err(GovError::NotSigner));
    env.board_confirm(&s2, p).unwrap();
    assert_eq!(env.board_state(p), BoardState::Queued);
    assert_eq!(code(env.execute(BOARD, p)), err(GovError::ProposalNotExecutable), "timelock not over");
    env.warp(START + 50);
    env.execute(BOARD, p).unwrap();
    assert_eq!(env.lamports(&host), 1_000_000_000);
    assert_eq!(env.board_state(p), BoardState::Executed);
    assert_eq!(code(env.board_confirm(&s3, p)), err(GovError::ProposalAlreadyExecuted));
}

#[test]
fn revoking_below_the_threshold_unqueues() {
    let (mut env, [s1, s2, _s3]) = board_dao();
    let p = env.board_propose(&s1, vec![env.pay_sol(&s1.pubkey(), 1)]).unwrap();
    env.board_confirm(&s2, p).unwrap();
    assert_eq!(env.board_state(p), BoardState::Queued);
    env.board_revoke(&s2, p).unwrap();
    assert_eq!(env.board_state(p), BoardState::Active);
    assert_eq!(code(env.board_revoke(&s2, p)), err(GovError::NotConfirmedBySigner));
    env.warp(START + 50);
    assert_eq!(code(env.execute(BOARD, p)), err(GovError::ThresholdNotMet));
}

#[test]
fn signers_change_only_through_a_proposal() {
    let (mut env, [s1, s2, s3]) = board_dao();
    let s4 = Keypair::new();
    // Nobody adds signers directly, not even a signer.
    let direct = Instruction {
        program_id: BOARD,
        accounts: board::accounts::TreasuryOnly { treasury: s1.pubkey(), governance: env.governance(BOARD) }.to_account_metas(None),
        data: board::instruction::AddSigner { signer: s4.pubkey() }.data(),
    };
    assert_eq!(code(env.send(&[direct], &[&s1])), err(GovError::Unauthorized));

    // One proposal: add s4, then require 3 of 4.
    let add = env.board_admin_ix(board::instruction::AddSigner { signer: s4.pubkey() }.data());
    let raise = env.board_admin_ix(board::instruction::UpdateConfig { config: board_config(3) }.data());
    let p = env.board_propose(&s1, vec![add, raise]).unwrap();
    env.board_confirm(&s2, p).unwrap();
    env.warp(START + 50);
    env.execute(BOARD, p).unwrap();
    let g = env.board_governance();
    assert_eq!(g.signers.len(), 4);
    assert_eq!(g.config.required_approvals, 3);

    // Requiring more approvals than signers is refused.
    let too_many = env.board_admin_ix(board::instruction::UpdateConfig { config: board_config(5) }.data());
    let p = env.board_propose(&s1, vec![too_many]).unwrap();
    env.board_confirm(&s2, p).unwrap();
    env.board_confirm(&s3, p).unwrap();
    env.warp(START + 100);
    assert_eq!(code(env.execute(BOARD, p)), err(GovError::InvalidApprovals));
}

#[test]
fn a_removed_signers_confirmation_stops_counting() {
    let (mut env, [s1, s2, s3]) = board_dao();
    // P: confirmed by s1 and s2, queued.
    let p = env.board_propose(&s1, vec![env.pay_sol(&s1.pubkey(), 5)]).unwrap();
    env.board_confirm(&s2, p).unwrap();
    // R: s1 and s3 remove s2.
    let remove = env.board_admin_ix(board::instruction::RemoveSigner { signer: s2.pubkey() }.data());
    let r = env.board_propose(&s3, vec![remove]).unwrap();
    env.board_confirm(&s1, r).unwrap();
    env.warp(START + 50);
    env.execute(BOARD, r).unwrap();
    assert!(!env.board_governance().signers.contains(&s2.pubkey()));
    // P now has only one current signer's confirmation.
    assert_eq!(code(env.execute(BOARD, p)), err(GovError::ThresholdNotMet));
    env.board_confirm(&s3, p).unwrap();
    env.execute(BOARD, p).unwrap();
}

#[test]
fn only_the_proposer_or_the_dao_cancels() {
    let (mut env, [s1, s2, _s3]) = board_dao();
    let p = env.board_propose(&s1, vec![env.pay_sol(&s1.pubkey(), 1)]).unwrap();
    assert_eq!(code(env.cancel(BOARD, &s2, p)), err(GovError::Unauthorized));
    env.cancel(BOARD, &s1, p).unwrap();
    assert_eq!(env.board_state(p), BoardState::Cancelled);
    assert_eq!(code(env.board_confirm(&s2, p)), err(GovError::ProposalAlreadyCancelled));
}

#[test]
fn bad_boards_are_refused() {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.insecure_clone();
    let a = Keypair::new().pubkey();
    let key = Keypair::new();
    env.dao = Pubkey::find_program_address(&[hub::DAO_SEED, key.pubkey().as_ref()], &hub::ID).0;
    env.treasury = hub::treasury_address(&env.dao);
    let create = env.ix_create_dao("Bad", BOARD, &key.pubkey());
    for (signers, required, why) in [
        (vec![a, a], 1, err(GovError::AlreadySigner)),
        (vec![a], 2, err(GovError::InvalidApprovals)),
        (vec![a], 0, err(GovError::InvalidApprovals)),
        (vec![], 1, err(GovError::InvalidApprovals)),
    ] {
        let init = env.ix_init_board(&creator.pubkey(), &creator.pubkey(), signers, board_config(required));
        assert_eq!(code(env.send(&[create.clone(), init], &[&creator, &key])), why);
    }
}
