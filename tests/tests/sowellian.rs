//! Sowellian: approval, the YES/NO market, execution through the hub, and
//! every resolution path - Pyth oracle, unchallenged and adjudicated human
//! resolution - with claims, bonds and voids.

use solana_account::Account;
use vortex_sowellian as sow;
use vortex_sowellian::{Criteria, Outcome, Phase, ResolutionMethod, Side, SowellianConfig, VoteChoice as Choice};
use vortex_tests::*;

const FEED: [u8; 32] = [0xEF; 32];

fn sow_config() -> SowellianConfig {
    SowellianConfig {
        proposal_bond: 50,
        approval_voting_delay: 10,
        approval_voting_period: 100,
        approval_quorum_bps: 1_000,
        approval_threshold_bps: 6_000,
        positions_window: 100,
        execution_timelock: 50,
        execution_period: 200,
        resolution_bond: 20,
        challenge_period: 100,
        challenge_bond: 30,
        adjudication_voting_period: 100,
        adjudication_quorum_bps: 1_000,
        adjudication_threshold_bps: 6_000,
        max_oracle_staleness: 60,
    }
}

fn escrow(env: &Env) -> Pubkey {
    Pubkey::find_program_address(&[sow::ESCROW_SEED, env.governance(SOW).as_ref()], &SOW).0
}

fn pda(seed: &[u8], p: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, p.as_ref(), owner.as_ref()], &SOW).0
}

fn proposal(env: &Env, p: Pubkey) -> sow::Proposal {
    sow::Proposal::try_deserialize(&mut env.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
}

fn governance(env: &Env) -> sow::Governance {
    sow::Governance::try_deserialize(&mut env.svm.get_account(&env.governance(SOW)).unwrap().data.as_slice()).unwrap()
}

/// A Sowellian DAO: alice deposits 600, bob 400; both keep 400 in their wallets for bonds and positions.
fn sowellian_dao() -> (Env, Member, Member) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Sowell", SOW, |env| Instruction {
        program_id: SOW,
        accounts: sow::accounts::InitGovernance {
            authority: creator,
            payer: creator,
            hub_dao: env.dao,
            governance: env.governance(SOW),
            mint: env.mint,
            dao_token: hub::dao_token_address(&env.dao),
            vault: env.vault(SOW),
            escrow: escrow(env),
            token_program: env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: sow::instruction::InitGovernance { config: sow_config() }.data(),
    });
    let alice = env.member(1_000);
    let bob = env.member(800);
    deposit(&mut env, &alice, 600).unwrap();
    deposit(&mut env, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, alice, bob)
}

fn send(env: &mut Env, who: &Keypair, accounts: Vec<AccountMeta>, data: Vec<u8>) -> Result<(), FailedTransactionMetadata> {
    env.send(&[Instruction { program_id: SOW, accounts, data }], &[who])
}

fn deposit(env: &mut Env, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::Deposit {
        owner: m.kp.pubkey(),
        governance: env.governance(SOW),
        voter: env.voter_pda(SOW, &m.kp.pubkey()),
        owner_token_account: m.ata,
        vault: env.vault(SOW),
        mint: env.mint,
        token_program: env.token_program,
        system_program: SYSTEM,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::Deposit { amount }.data())
}

fn withdraw(env: &mut Env, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::Withdraw {
        owner: m.kp.pubkey(),
        governance: env.governance(SOW),
        voter: env.voter_pda(SOW, &m.kp.pubkey()),
        owner_token_account: m.ata,
        vault: env.vault(SOW),
        mint: env.mint,
        token_program: env.token_program,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::Withdraw { amount }.data())
}

fn human() -> Criteria {
    Criteria { method: ResolutionMethod::Human, feed_id: [0; 32], target_value: 0, target_is_minimum: true, measurement_period: 100 }
}

fn oracle(target: i128) -> Criteria {
    Criteria { method: ResolutionMethod::Oracle, feed_id: FEED, target_value: target, target_is_minimum: true, measurement_period: 100 }
}

fn propose(env: &mut Env, m: &Member, criteria: Criteria) -> Result<Pubkey, FailedTransactionMetadata> {
    let id = governance(env).proposal_count + 1;
    let p = env.proposal_pda(SOW, id);
    let a = sow::accounts::Propose {
        proposer: m.kp.pubkey(),
        governance: env.governance(SOW),
        hub_dao: env.dao,
        proposal: p,
        proposer_token_account: m.ata,
        escrow: escrow(env),
        mint: env.mint,
        token_program: env.token_program,
        system_program: SYSTEM,
    };
    let ix = sow::instruction::Propose { id, metadata_uri: "Grow monthly users past 1,000".into(), instructions: vec![env.pay_sol(&m.kp.pubkey(), 1_000)], criteria };
    send(env, &m.kp, a.to_account_metas(None), ix.data()).map(|_| p)
}

fn approve_vote(env: &mut Env, m: &Member, p: Pubkey, choice: Choice) -> Result<(), FailedTransactionMetadata> {
    let o = m.kp.pubkey();
    let a = sow::accounts::CastApprovalVote {
        owner: o,
        governance: env.governance(SOW),
        proposal: p,
        voter: env.voter_pda(SOW, &o),
        vote_record: pda(sow::APPROVAL_VOTE_SEED, &p, &o),
        system_program: SYSTEM,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::CastApprovalVote { choice }.data())
}

fn settle(env: &mut Env, p: Pubkey, data: Vec<u8>) -> Result<(), FailedTransactionMetadata> {
    let admin = env.admin.insecure_clone();
    let a = sow::accounts::Settle { governance: env.governance(SOW), proposal: p };
    send(env, &admin, a.to_account_metas(None), data)
}

fn take(env: &mut Env, m: &Member, p: Pubkey, side: Side, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let o = m.kp.pubkey();
    let a = sow::accounts::TakePosition {
        owner: o,
        governance: env.governance(SOW),
        proposal: p,
        position: pda(sow::POSITION_SEED, &p, &o),
        owner_token_account: m.ata,
        escrow: escrow(env),
        mint: env.mint,
        token_program: env.token_program,
        system_program: SYSTEM,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::TakePosition { side, amount }.data())
}

fn bond_action(env: &mut Env, m: &Member, p: Pubkey, data: Vec<u8>) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::BondAction {
        owner: m.kp.pubkey(),
        governance: env.governance(SOW),
        proposal: p,
        owner_token_account: m.ata,
        escrow: escrow(env),
        mint: env.mint,
        token_program: env.token_program,
    };
    send(env, &m.kp, a.to_account_metas(None), data)
}

fn adjudicate(env: &mut Env, m: &Member, p: Pubkey, outcome: Outcome) -> Result<(), FailedTransactionMetadata> {
    let o = m.kp.pubkey();
    let a = sow::accounts::CastAdjudicationVote {
        owner: o,
        governance: env.governance(SOW),
        proposal: p,
        voter: env.voter_pda(SOW, &o),
        vote_record: pda(sow::ADJUDICATION_VOTE_SEED, &p, &o),
        system_program: SYSTEM,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::CastAdjudicationVote { outcome }.data())
}

fn claim_position(env: &mut Env, m: &Member, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let o = m.kp.pubkey();
    let a = sow::accounts::ClaimPosition {
        owner: o,
        governance: env.governance(SOW),
        proposal: p,
        position: pda(sow::POSITION_SEED, &p, &o),
        owner_token_account: m.ata,
        escrow: escrow(env),
        mint: env.mint,
        token_program: env.token_program,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::ClaimPosition {}.data())
}

fn claim_bond(env: &mut Env, m: &Member, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::ClaimBond {
        owner: m.kp.pubkey(),
        governance: env.governance(SOW),
        proposal: p,
        owner_token_account: m.ata,
        escrow: escrow(env),
        mint: env.mint,
        token_program: env.token_program,
    };
    send(env, &m.kp, a.to_account_metas(None), sow::instruction::ClaimBond {}.data())
}

fn treasury_ata(env: &mut Env) -> Pubkey {
    let creator = env.creator.insecure_clone();
    let (mint, treasury) = (env.mint, env.treasury);
    CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &mint).owner(&treasury).send().unwrap()
}

fn sweep(env: &mut Env, to: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::SweepToTreasury { governance: env.governance(SOW), treasury_token_account: to, escrow: escrow(env), mint: env.mint, token_program: env.token_program };
    let admin = env.admin.insecure_clone();
    send(env, &admin, a.to_account_metas(None), sow::instruction::SweepToTreasury {}.data())
}

/// Propose, approve with alice, open positions: alice YES 100, bob NO 300.
fn approved_with_positions(env: &mut Env, alice: &Member, bob: &Member, criteria: Criteria) -> Pubkey {
    let p = propose(env, alice, criteria).unwrap();
    env.warp(START + 10);
    approve_vote(env, alice, p, Choice::For).unwrap();
    env.warp(START + 110);
    settle(env, p, sow::instruction::FinalizeApproval {}.data()).unwrap();
    assert_eq!(proposal(env, p).phase, Phase::PositionsOpen);
    take(env, alice, p, Side::Yes, 100).unwrap();
    take(env, bob, p, Side::No, 300).unwrap();
    p
}

/// Positions close at 210; executable 260..460; measurement ends 100 s after execution.
fn execute(env: &mut Env, p: Pubkey) {
    env.warp(START + 260);
    env.execute(SOW, p).unwrap();
}

fn set_price(env: &mut Env, feed: [u8; 32], price: i64, expo: i32, publish_time: i64, full: bool, owner: Pubkey) -> Pubkey {
    let mut data = sow::PRICE_UPDATE_V2.to_vec();
    data.extend_from_slice(Pubkey::new_unique().as_ref()); // write authority
    if full {
        data.push(1);
    } else {
        data.extend_from_slice(&[0, 5]);
    }
    data.extend_from_slice(&feed);
    data.extend_from_slice(&price.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes()); // conf
    data.extend_from_slice(&expo.to_le_bytes());
    data.extend_from_slice(&publish_time.to_le_bytes());
    data.extend_from_slice(&(publish_time - 1).to_le_bytes());
    data.extend_from_slice(&[0u8; 8 + 8 + 8]); // ema price, ema conf, posted slot
    let address = Pubkey::new_unique();
    env.svm.set_account(address, Account { lamports: 10_000_000, data, owner, executable: false, rent_epoch: 0 }).unwrap();
    address
}

fn resolve_oracle(env: &mut Env, p: Pubkey, price_update: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let a = sow::accounts::ResolveViaOracle { governance: env.governance(SOW), proposal: p, price_update };
    let admin = env.admin.insecure_clone();
    send(env, &admin, a.to_account_metas(None), sow::instruction::ResolveViaOracle {}.data())
}

#[test]
fn human_resolution_unchallenged_pays_the_winners_and_returns_bonds() {
    let (mut env, alice, bob) = sowellian_dao();
    let p = approved_with_positions(&mut env, &alice, &bob, human());
    assert_eq!(env.token_balance(&escrow(&env)), 50 + 400, "bond + positions held in escrow");
    env.warp(START + 210);
    assert_eq!(code(take(&mut env, &bob, p, Side::No, 1)), err(GovError::PositionsClosed));
    assert_eq!(code(env.execute(SOW, p)), err(GovError::ProposalNotExecutable), "timelock after the positions window");
    let before = env.lamports(&alice.kp.pubkey());
    execute(&mut env, p);
    assert_eq!(env.lamports(&alice.kp.pubkey()) - before, 1_000, "paid by the hub treasury");
    assert_eq!(code(bond_action(&mut env, &bob, p, sow::instruction::ProposeResolution { outcome: Outcome::Success }.data())), err(GovError::MeasurementNotOver));
    env.warp(START + 360);
    bond_action(&mut env, &bob, p, sow::instruction::ProposeResolution { outcome: Outcome::Success }.data()).unwrap();
    assert_eq!(code(settle(&mut env, p, sow::instruction::FinalizeUnchallenged {}.data())), err(GovError::ChallengeWindowOpen));
    env.warp(START + 460);
    settle(&mut env, p, sow::instruction::FinalizeUnchallenged {}.data()).unwrap();
    assert_eq!(proposal(&env, p).final_outcome, Outcome::Success);

    let alice_before = env.token_balance(&alice.ata);
    claim_position(&mut env, &alice, p).unwrap();
    assert_eq!(env.token_balance(&alice.ata) - alice_before, 400, "the whole pool to the only YES");
    assert_eq!(code(claim_position(&mut env, &alice, p)), err(GovError::AlreadyClaimed));
    assert_eq!(code(claim_position(&mut env, &bob, p)), err(GovError::NothingToClaim));
    let (a0, b0) = (env.token_balance(&alice.ata), env.token_balance(&bob.ata));
    claim_bond(&mut env, &alice, p).unwrap();
    claim_bond(&mut env, &bob, p).unwrap();
    assert_eq!((env.token_balance(&alice.ata) - a0, env.token_balance(&bob.ata) - b0), (50, 20), "proposal bond, resolution bond");
    assert_eq!(env.token_balance(&escrow(&env)), 0, "escrow empty");
}

#[test]
fn a_challenge_goes_to_a_token_vote_and_the_wrong_side_loses_its_bond() {
    // The resolver says Success; the vote finds Failure.
    let (mut env, alice, bob) = sowellian_dao();
    let p = approved_with_positions(&mut env, &alice, &bob, human());
    execute(&mut env, p);
    env.warp(START + 360);
    bond_action(&mut env, &alice, p, sow::instruction::ProposeResolution { outcome: Outcome::Success }.data()).unwrap();
    bond_action(&mut env, &bob, p, sow::instruction::ChallengeResolution {}.data()).unwrap();
    assert_eq!(code(bond_action(&mut env, &bob, p, sow::instruction::ChallengeResolution {}.data())), err(GovError::WrongPhase));
    adjudicate(&mut env, &bob, p, Outcome::Failure).unwrap();
    adjudicate(&mut env, &alice, p, Outcome::Success).unwrap();
    assert_eq!(code(withdraw(&mut env, &bob, 1)), err(GovError::TokensLocked), "adjudication votes lock deposits");
    assert_eq!(code(settle(&mut env, p, sow::instruction::FinalizeAdjudication {}.data())), err(GovError::VotingNotEnded));
    env.warp(START + 460);
    settle(&mut env, p, sow::instruction::FinalizeAdjudication {}.data()).unwrap();
    // 600 success of 1000: exactly the 60% threshold, so Success.
    let pr = proposal(&env, p);
    assert_eq!(pr.final_outcome, Outcome::Success);
    assert_eq!((pr.owed_resolver, pr.owed_challenger, governance(&env).owed_treasury), (20, 0, 30), "challenger was wrong");
    let ta = treasury_ata(&mut env);
    sweep(&mut env, ta).unwrap();
    assert_eq!(env.token_balance(&ta), 30);
    assert_eq!(code(sweep(&mut env, ta)), err(GovError::NothingToClaim));

    // Same again, but bob holds more weight: Failure, resolver loses.
    let (mut env, alice, bob) = sowellian_dao();
    let creator = env.creator.insecure_clone();
    let mint = env.mint;
    MintTo::new(&mut env.svm, &creator, &mint, &bob.ata, 300).send().unwrap();
    deposit(&mut env, &bob, 300).unwrap();
    let p = approved_with_positions(&mut env, &alice, &bob, human());
    execute(&mut env, p);
    env.warp(START + 360);
    bond_action(&mut env, &alice, p, sow::instruction::ProposeResolution { outcome: Outcome::Success }.data()).unwrap();
    bond_action(&mut env, &bob, p, sow::instruction::ChallengeResolution {}.data()).unwrap();
    adjudicate(&mut env, &bob, p, Outcome::Failure).unwrap();
    adjudicate(&mut env, &alice, p, Outcome::Success).unwrap();
    env.warp(START + 460);
    settle(&mut env, p, sow::instruction::FinalizeAdjudication {}.data()).unwrap();
    let pr = proposal(&env, p);
    assert_eq!(pr.final_outcome, Outcome::Failure);
    assert_eq!((pr.owed_resolver, pr.owed_challenger, governance(&env).owed_treasury), (0, 30, 20));
    let b0 = env.token_balance(&bob.ata);
    claim_position(&mut env, &bob, p).unwrap();
    claim_bond(&mut env, &bob, p).unwrap();
    assert_eq!(env.token_balance(&bob.ata) - b0, 400 + 30, "NO wins the pool; challenge bond back");
}

#[test]
fn oracle_resolution_needs_a_fresh_fully_verified_price_for_the_feed() {
    let (mut env, alice, bob) = sowellian_dao();
    // Target 100.0, success at or above.
    let p = approved_with_positions(&mut env, &alice, &bob, oracle(100 * 10i128.pow(18)));
    execute(&mut env, p);
    let deadline = proposal(&env, p).measurement_deadline;
    let good = set_price(&mut env, FEED, 12_000_000_000, -8, deadline + 5, true, sow::PYTH_RECEIVER);
    assert_eq!(code(resolve_oracle(&mut env, p, good)), err(GovError::MeasurementNotOver));
    env.warp(deadline + 10);
    assert_eq!(code(bond_action(&mut env, &bob, p, sow::instruction::ProposeResolution { outcome: Outcome::Failure }.data())), err(GovError::WrongResolutionMethod));
    let wrong_feed = set_price(&mut env, [1; 32], 12_000_000_000, -8, deadline + 5, true, sow::PYTH_RECEIVER);
    let partial = set_price(&mut env, FEED, 12_000_000_000, -8, deadline + 5, false, sow::PYTH_RECEIVER);
    let forged = set_price(&mut env, FEED, 12_000_000_000, -8, deadline + 5, true, Pubkey::new_unique());
    let early = set_price(&mut env, FEED, 12_000_000_000, -8, deadline - 1, true, sow::PYTH_RECEIVER);
    for bad in [wrong_feed, partial, forged] {
        assert_eq!(code(resolve_oracle(&mut env, p, bad)), err(GovError::WrongOracleAccount));
    }
    assert_eq!(code(resolve_oracle(&mut env, p, early)), err(GovError::StaleOracleData), "measured before the period ended");
    env.warp(deadline + 100);
    assert_eq!(code(resolve_oracle(&mut env, p, good)), err(GovError::StaleOracleData), "older than 60 s");
    let fresh = set_price(&mut env, FEED, 12_000_000_000, -8, deadline + 90, true, sow::PYTH_RECEIVER);
    resolve_oracle(&mut env, p, fresh).unwrap();
    let pr = proposal(&env, p);
    assert_eq!((pr.phase, pr.final_outcome), (Phase::Finalized, Outcome::Success), "120 >= 100");
    let a0 = env.token_balance(&alice.ata);
    claim_position(&mut env, &alice, p).unwrap();
    claim_bond(&mut env, &alice, p).unwrap();
    assert_eq!(env.token_balance(&alice.ata) - a0, 400 + 50);
}

#[test]
fn rejection_sends_the_bond_to_the_treasury() {
    let (mut env, alice, bob) = sowellian_dao();
    let p = propose(&mut env, &alice, human()).unwrap();
    env.warp(START + 10);
    approve_vote(&mut env, &bob, p, Choice::Against).unwrap();
    env.warp(START + 110);
    settle(&mut env, p, sow::instruction::FinalizeApproval {}.data()).unwrap();
    assert_eq!(proposal(&env, p).phase, Phase::Rejected);
    assert_eq!(governance(&env).owed_treasury, 50);
    assert_eq!(code(take(&mut env, &bob, p, Side::No, 10)), err(GovError::WrongPhase));
    assert_eq!(code(env.execute(SOW, p)), err(GovError::ProposalNotExecutable));
    assert_eq!(code(claim_bond(&mut env, &alice, p)), err(GovError::NothingToClaim));
}

#[test]
fn an_approved_proposal_nobody_ran_is_voided_and_refunded() {
    let (mut env, alice, bob) = sowellian_dao();
    let p = approved_with_positions(&mut env, &alice, &bob, human());
    env.warp(START + 459);
    assert_eq!(code(settle(&mut env, p, sow::instruction::VoidExpired {}.data())), err(GovError::ProposalNotExecutable));
    env.warp(START + 460);
    assert_eq!(code(env.execute(SOW, p)), err(GovError::ProposalExpired));
    settle(&mut env, p, sow::instruction::VoidExpired {}.data()).unwrap();
    let (a0, b0) = (env.token_balance(&alice.ata), env.token_balance(&bob.ata));
    claim_position(&mut env, &alice, p).unwrap();
    claim_position(&mut env, &bob, p).unwrap();
    claim_bond(&mut env, &alice, p).unwrap();
    assert_eq!((env.token_balance(&alice.ata) - a0, env.token_balance(&bob.ata) - b0), (100 + 50, 300), "stakes and bond back");
}

#[test]
fn the_proposer_can_cancel_during_the_approval_vote() {
    let (mut env, alice, bob) = sowellian_dao();
    let p = propose(&mut env, &alice, human()).unwrap();
    let cancel = |env: &Env, who: &Keypair| Instruction {
        program_id: SOW,
        accounts: sow::accounts::Cancel { authority: who.pubkey(), governance: env.governance(SOW), proposal: p }.to_account_metas(None),
        data: sow::instruction::Cancel {}.data(),
    };
    assert_eq!(code(env.send(&[cancel(&env, &bob.kp)], &[&bob.kp])), err(GovError::Unauthorized));
    env.send(&[cancel(&env, &alice.kp)], &[&alice.kp]).unwrap();
    let a0 = env.token_balance(&alice.ata);
    claim_bond(&mut env, &alice, p).unwrap();
    assert_eq!(env.token_balance(&alice.ata) - a0, 50);
}
