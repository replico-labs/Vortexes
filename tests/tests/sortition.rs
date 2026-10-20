//! Sortition: the opt-in pool, ORAO-randomness draws, restarts, and the
//! drawn council governing through the hub.

use solana_account::Account;
use vortex_sortition as sort;
use vortex_sortition::{SortitionConfig, VoteChoice as Choice};
use vortex_tests::*;

fn sort_config(seats: u8) -> SortitionConfig {
    SortitionConfig {
        council_size: seats,
        term_length: 1_000,
        eligibility_threshold: 100,
        council_quorum: 2.min(seats),
        council_approval_bps: 6_000,
        voting_delay: 10,
        voting_period: 100,
        timelock: 50,
        execution_period: 200,
        randomness_timeout: 60,
    }
}

fn pool_pda(env: &Env) -> Pubkey {
    Pubkey::find_program_address(&[sort::POOL_SEED, env.governance(SORT).as_ref()], &SORT).0
}

fn governance(env: &Env) -> sort::Governance {
    sort::Governance::try_deserialize(&mut env.svm.get_account(&env.governance(SORT)).unwrap().data.as_slice()).unwrap()
}

fn pool(env: &Env) -> Vec<Pubkey> {
    sort::Pool::try_deserialize(&mut env.svm.get_account(&pool_pda(env)).unwrap().data.as_slice()).unwrap().members
}

/// A sortition DAO (council: the creator) and `n` members with 100 deposited each.
fn sortition_dao(seats: u8, n: usize) -> (Env, Vec<Member>) {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    let config = SortitionConfig { council_size: 1, council_quorum: 1, ..sort_config(seats) };
    env.create_dao("Lots", SORT, |env| Instruction {
        program_id: SORT,
        accounts: sort::accounts::InitGovernance {
            authority: creator,
            payer: creator,
            hub_dao: env.dao,
            governance: env.governance(SORT),
            pool: pool_pda(env),
            mint: env.mint,
            dao_token: hub::dao_token_address(&env.dao),
            vault: env.vault(SORT),
            token_program: env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: sort::instruction::InitGovernance { council: vec![creator], config }.data(),
    });
    // Seats for later draws come from a config update; for these tests the
    // DAO starts with `seats` straight away.
    let mut g = governance(&env);
    g.config = sort_config(seats);
    let address = env.governance(SORT);
    write(&mut env, address, &g);
    let members = (0..n)
        .map(|_| {
            let m = env.member(1_000);
            deposit(&mut env, &m, 100).unwrap();
            m
        })
        .collect();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, members)
}

fn write<T: anchor_lang::AccountSerialize>(env: &mut Env, address: Pubkey, value: &T) {
    let mut account = env.svm.get_account(&address).unwrap();
    let mut data = Vec::new();
    value.try_serialize(&mut data).unwrap();
    account.data[..data.len()].copy_from_slice(&data);
    env.svm.set_account(address, account).unwrap();
}

fn deposit(env: &mut Env, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::Deposit {
            owner: m.kp.pubkey(),
            governance: env.governance(SORT),
            voter: env.voter_pda(SORT, &m.kp.pubkey()),
            owner_token_account: m.ata,
            vault: env.vault(SORT),
            mint: env.mint,
            token_program: env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: sort::instruction::Deposit { amount }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn withdraw(env: &mut Env, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::Withdraw {
            owner: m.kp.pubkey(),
            governance: env.governance(SORT),
            voter: env.voter_pda(SORT, &m.kp.pubkey()),
            owner_token_account: m.ata,
            vault: env.vault(SORT),
            mint: env.mint,
            token_program: env.token_program,
        }
        .to_account_metas(None),
        data: sort::instruction::Withdraw { amount }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn pool_action(env: &mut Env, m: &Member, join: bool) -> Result<(), FailedTransactionMetadata> {
    let accounts = sort::accounts::PoolAction {
        owner: m.kp.pubkey(),
        governance: env.governance(SORT),
        pool: pool_pda(env),
        voter: env.voter_pda(SORT, &m.kp.pubkey()),
    }
    .to_account_metas(None);
    let data = if join { sort::instruction::JoinPool {}.data() } else { sort::instruction::LeavePool {}.data() };
    env.send(&[Instruction { program_id: SORT, accounts, data }], &[&m.kp])
}

fn randomness_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[sort::ORAO_REQUEST_SEED, seed], &sort::ORAO_VRF).0
}

/// ORAO's RandomnessV2 account, as ORAO would leave it: pending, or fulfilled with `randomness`.
fn set_orao(env: &mut Env, seed: &[u8; 32], randomness: Option<[u8; 64]>, owner: Pubkey) {
    let mut data = sort::ORAO_RANDOMNESS_V2.to_vec();
    let client = Pubkey::new_unique();
    match randomness {
        None => {
            data.push(0);
            data.extend_from_slice(client.as_ref());
            data.extend_from_slice(seed);
            data.extend_from_slice(&0u32.to_le_bytes()); // no responses yet
        }
        Some(r) => {
            data.push(1);
            data.extend_from_slice(client.as_ref());
            data.extend_from_slice(seed);
            data.extend_from_slice(&r);
        }
    }
    let account = Account { lamports: 10_000_000, data, owner, executable: false, rent_epoch: 0 };
    env.svm.set_account(randomness_address(seed), account).unwrap();
}

fn start_draw(env: &mut Env, nonce: [u8; 32], previous: Option<[u8; 32]>) -> Result<[u8; 32], FailedTransactionMetadata> {
    let g = governance(env);
    let number = if g.draw_open { g.draw_count } else { g.draw_count + 1 };
    let seed = sort::draw_seed(&env.governance(SORT), number, &nonce);
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::StartDraw {
            governance: env.governance(SORT),
            pool: pool_pda(env),
            randomness: randomness_address(&seed),
            previous_randomness: randomness_address(&previous.unwrap_or(seed)),
        }
        .to_account_metas(None),
        data: sort::instruction::StartDraw { nonce }.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin]).map(|_| seed)
}

fn finalize(env: &mut Env, seed: &[u8; 32]) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::FinalizeDraw { governance: env.governance(SORT), pool: pool_pda(env), randomness: randomness_address(seed) }.to_account_metas(None),
        data: sort::instruction::FinalizeDraw {}.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

/// The draw the program should make: the same partial Fisher-Yates.
fn expected_council(mut members: Vec<Pubkey>, randomness: &[u8; 64], seats: usize) -> Vec<Pubkey> {
    let n = members.len();
    for i in 0..seats.min(n) {
        let h = solana_sha256_hasher::hashv(&[randomness, &(i as u64).to_le_bytes()]).to_bytes();
        let r = u64::from_le_bytes(h[..8].try_into().unwrap());
        members.swap(i, i + (r % (n - i) as u64) as usize);
    }
    members.truncate(seats.min(n));
    members
}

fn propose(env: &mut Env, who: &Keypair) -> Result<Pubkey, FailedTransactionMetadata> {
    let id = governance(env).proposal_count + 1;
    let p = env.proposal_pda(SORT, id);
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::Propose { proposer: who.pubkey(), governance: env.governance(SORT), hub_dao: env.dao, proposal: p, system_program: SYSTEM }
            .to_account_metas(None),
        data: sort::instruction::Propose { id, metadata_uri: "Pay the host".into(), instructions: vec![env.pay_sol(&who.pubkey(), 1_000)] }.data(),
    };
    env.send(&[ix], &[who]).map(|_| p)
}

fn council_vote(env: &mut Env, who: &Keypair, p: Pubkey, choice: Choice) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::CouncilAction { member: who.pubkey(), governance: env.governance(SORT), proposal: p }.to_account_metas(None),
        data: sort::instruction::CastVote { choice }.data(),
    };
    env.send(&[ix], &[who])
}

fn queue(env: &mut Env, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: SORT,
        accounts: sort::accounts::Queue { governance: env.governance(SORT), proposal: p }.to_account_metas(None),
        data: sort::instruction::Queue {}.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

#[test]
fn the_pool_needs_a_deposit_and_locks_it() {
    let (mut env, ms) = sortition_dao(3, 2);
    let small = env.member(1_000);
    deposit(&mut env, &small, 50).unwrap();
    assert_eq!(code(pool_action(&mut env, &small, true)), err(GovError::ProposalThresholdNotMet));
    pool_action(&mut env, &ms[0], true).unwrap();
    assert_eq!(code(pool_action(&mut env, &ms[0], true)), err(GovError::AlreadyEligible));
    assert_eq!(code(withdraw(&mut env, &ms[0], 10)), err(GovError::LeavePoolFirst));
    pool_action(&mut env, &ms[0], false).unwrap();
    withdraw(&mut env, &ms[0], 100).unwrap();
    assert_eq!(code(pool_action(&mut env, &ms[1], false)), err(GovError::NotEligible));
}

#[test]
fn a_draw_seats_the_council_orao_randomness_picks_and_it_governs() {
    let (mut env, ms) = sortition_dao(3, 6);
    for m in &ms {
        pool_action(&mut env, m, true).unwrap();
    }
    // The first council's proposal, made before the draw.
    let creator = env.creator.insecure_clone();
    let old = propose(&mut env, &creator).unwrap();

    let nonce = [7u8; 32];
    let seed = sort::draw_seed(&env.governance(SORT), 1, &nonce);
    set_orao(&mut env, &seed, None, sort::ORAO_VRF);
    assert_eq!(code(start_draw(&mut env, nonce, None)), err(GovError::TermNotOver));
    env.warp(START + 1_000);
    start_draw(&mut env, nonce, None).unwrap();
    assert_eq!(code(pool_action(&mut env, &ms[0], false)), err(GovError::DrawInProgress), "pool frozen");
    let late = env.member(1_000);
    deposit(&mut env, &late, 100).unwrap();
    assert_eq!(code(pool_action(&mut env, &late, true)), err(GovError::DrawInProgress));
    assert_eq!(code(finalize(&mut env, &seed)), err(GovError::RandomnessNotReady));

    let randomness = [42u8; 64];
    let pool_before = pool(&env);
    set_orao(&mut env, &seed, Some(randomness), sort::ORAO_VRF);
    finalize(&mut env, &seed).unwrap();
    let g = governance(&env);
    assert_eq!(g.council, expected_council(pool_before, &randomness, 3), "exactly the draw the randomness gives");
    assert_eq!((g.council.len(), g.council_term, g.draw_open), (3, 2, false));
    assert_eq!(g.term_ends_at, START + 1_000 + 1_000);
    assert_eq!(code(finalize(&mut env, &seed)), err(GovError::NoDrawInProgress));

    // The old council's proposal died with it; the new council governs.
    env.warp(START + 1_020);
    assert_eq!(code(council_vote(&mut env, &creator, old, Choice::For)), err(GovError::NotCouncilMember));
    let seated: Vec<&Member> = ms.iter().filter(|m| g.council.contains(&m.kp.pubkey())).collect();
    let outsider = ms.iter().find(|m| !g.council.contains(&m.kp.pubkey())).unwrap();
    assert_eq!(code(propose(&mut env, &outsider.kp)), err(GovError::NotCouncilMember));
    let p = propose(&mut env, &seated[0].kp).unwrap();
    env.warp(START + 1_030);
    council_vote(&mut env, &seated[0].kp, p, Choice::For).unwrap();
    assert_eq!(code(council_vote(&mut env, &seated[0].kp, p, Choice::For)), err(GovError::AlreadyVoted));
    env.warp(START + 1_130);
    assert_eq!(code(queue(&mut env, p)), err(GovError::QuorumNotReached), "quorum is 2 of 3");
    let p2 = propose(&mut env, &seated[1].kp).unwrap();
    env.warp(START + 1_140);
    council_vote(&mut env, &seated[1].kp, p2, Choice::For).unwrap();
    council_vote(&mut env, &seated[2].kp, p2, Choice::For).unwrap();
    env.warp(START + 1_240);
    queue(&mut env, p2).unwrap();
    env.warp(START + 1_290);
    let who = seated[1].kp.pubkey();
    let before = env.lamports(&who);
    env.execute(SORT, p2).unwrap();
    assert_eq!(env.lamports(&who) - before, 1_000);
}

#[test]
fn only_an_unanswered_orao_request_can_start_a_draw() {
    let (mut env, ms) = sortition_dao(2, 3);
    for m in &ms {
        pool_action(&mut env, m, true).unwrap();
    }
    env.warp(START + 1_000);
    let nonce = [1u8; 32];
    let seed = sort::draw_seed(&env.governance(SORT), 1, &nonce);
    assert!(start_draw(&mut env, nonce, None).is_err(), "no request at all");
    set_orao(&mut env, &seed, None, Pubkey::new_unique());
    assert_eq!(code(start_draw(&mut env, nonce, None)), err(GovError::WrongRandomnessAccount), "not owned by ORAO");
    set_orao(&mut env, &seed, Some([9u8; 64]), sort::ORAO_VRF);
    assert_eq!(code(start_draw(&mut env, nonce, None)), err(GovError::RandomnessAlreadyFulfilled), "a result already known can't be used");
}

#[test]
fn a_stuck_draw_restarts_only_while_its_request_is_unanswered() {
    let (mut env, ms) = sortition_dao(2, 3);
    for m in &ms {
        pool_action(&mut env, m, true).unwrap();
    }
    env.warp(START + 1_000);
    let first = sort::draw_seed(&env.governance(SORT), 1, &[1u8; 32]);
    set_orao(&mut env, &first, None, sort::ORAO_VRF);
    start_draw(&mut env, [1u8; 32], None).unwrap();

    let second = sort::draw_seed(&env.governance(SORT), 1, &[2u8; 32]);
    set_orao(&mut env, &second, None, sort::ORAO_VRF);
    assert_eq!(code(start_draw(&mut env, [2u8; 32], Some(first))), err(GovError::DrawInProgress), "before the timeout");
    env.warp(START + 1_060);
    // If the first request got answered meanwhile, it must be used.
    set_orao(&mut env, &first, Some([3u8; 64]), sort::ORAO_VRF);
    assert_eq!(code(start_draw(&mut env, [2u8; 32], Some(first))), err(GovError::RandomnessAlreadyFulfilled));
    set_orao(&mut env, &first, None, sort::ORAO_VRF);
    start_draw(&mut env, [2u8; 32], Some(first)).unwrap();
    let g = governance(&env);
    assert_eq!((g.draw_count, g.draw_seed), (1, second), "same draw, new request");
    // Only the new request's answer counts.
    set_orao(&mut env, &first, Some([3u8; 64]), sort::ORAO_VRF);
    assert_eq!(code(finalize(&mut env, &first)), err(GovError::WrongRandomnessAccount));
    set_orao(&mut env, &second, Some([5u8; 64]), sort::ORAO_VRF);
    finalize(&mut env, &second).unwrap();
}

#[test]
fn a_pool_smaller_than_the_seats_seats_everyone() {
    let (mut env, ms) = sortition_dao(5, 2);
    for m in &ms {
        pool_action(&mut env, m, true).unwrap();
    }
    env.warp(START + 1_000);
    let seed = sort::draw_seed(&env.governance(SORT), 1, &[0u8; 32]);
    set_orao(&mut env, &seed, None, sort::ORAO_VRF);
    start_draw(&mut env, [0u8; 32], None).unwrap();
    set_orao(&mut env, &seed, Some([1u8; 64]), sort::ORAO_VRF);
    finalize(&mut env, &seed).unwrap();
    let g = governance(&env);
    assert_eq!(g.council.len(), 2);
    assert_eq!(g.quorum(), 2, "quorum capped at the council's size");
}
