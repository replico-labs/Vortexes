//! Delegate governance through the hub: an elected council proposes and
//! votes; members elect councils and recall council members. Build first
//! with `anchor build`.

use vortex_tests::*;
use del::VoteChoice as V;

struct Dao {
    env: Env,
    /// The first council.
    council: [Member; 3],
    /// Token holders: Alice 600, Bob 400 deposited.
    alice: Member,
    bob: Member,
}

/// A DAO run by a 3-seat council, with `config`; the treasury holds 5 SOL.
fn delegate_dao(config: DelegateConfig) -> Dao {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let council = [env.member(0), env.member(0), env.member(0)];
    let keys = council.iter().map(|m| m.kp.pubkey()).collect::<Vec<_>>();
    let creator = env.creator.pubkey();
    env.create_dao("Senate", DEL, |env| env.ix_init_delegate(&creator, &creator, keys, config));
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(DEL, &alice, 600).unwrap();
    env.deposit(DEL, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    Dao { env, council, alice, bob }
}

#[test]
fn the_council_runs_a_proposal() {
    let Dao { mut env, council: [a, b, c], alice, .. } = delegate_dao(del_config());
    let host = Keypair::new().pubkey();
    assert_eq!(code(env.del_propose(&alice.kp, vec![env.pay_sol(&host, 1)])), err(GovError::NotCouncilMember));

    let p = env.del_propose(&a.kp, vec![env.pay_sol(&host, 1_000_000_000)]).unwrap();
    assert_eq!(env.del_state(p), ProposalState::Pending);
    assert_eq!(code(env.council_vote(&a.kp, p, V::For)), err(GovError::ProposalNotActive), "voting delay");
    env.warp(START + 10);
    env.council_vote(&a.kp, p, V::For).unwrap();
    assert_eq!(code(env.council_vote(&a.kp, p, V::For)), err(GovError::AlreadyVoted));
    assert_eq!(code(env.council_vote(&alice.kp, p, V::For)), err(GovError::NotCouncilMember));
    env.council_vote(&b.kp, p, V::For).unwrap();
    env.council_vote(&c.kp, p, V::Against).unwrap();
    assert_eq!(code(env.queue(DEL, p)), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(env.del_state(p), ProposalState::Succeeded, "2 of 3 for: 67% >= 60%");
    assert_eq!(code(env.council_vote(&c.kp, p, V::For)), err(GovError::ProposalNotActive));
    env.queue(DEL, p).unwrap();
    assert_eq!(code(env.execute(DEL, p)), err(GovError::ProposalNotExecutable), "timelock not over");
    env.warp(START + 160);
    env.execute(DEL, p).unwrap();
    assert_eq!(env.lamports(&host), 1_000_000_000);
    assert_eq!(env.del_state(p), ProposalState::Executed);
}

#[test]
fn council_votes_need_quorum_and_approval() {
    let Dao { mut env, council: [a, b, _c], .. } = delegate_dao(del_config());
    let pay = |env: &Env| vec![env.pay_sol(&env.treasury, 1)];
    let lone = env.del_propose(&a.kp, pay(&env)).unwrap();
    let split = env.del_propose(&a.kp, pay(&env)).unwrap();
    let abstain = env.del_propose(&a.kp, pay(&env)).unwrap();
    env.warp(START + 10);
    env.council_vote(&a.kp, lone, V::For).unwrap();
    env.council_vote(&a.kp, split, V::For).unwrap();
    env.council_vote(&b.kp, split, V::Against).unwrap();
    env.council_vote(&a.kp, abstain, V::For).unwrap();
    env.council_vote(&b.kp, abstain, V::Abstain).unwrap();
    env.warp(START + 110);
    assert_eq!(code(env.queue(DEL, lone)), err(GovError::QuorumNotReached), "1 vote, 2 needed");
    assert_eq!(code(env.queue(DEL, split)), err(GovError::ApprovalThresholdNotMet), "50% < 60%");
    assert_eq!(env.del_state(split), ProposalState::Defeated);
    env.queue(DEL, abstain).unwrap();
}

#[test]
fn members_elect_a_new_council() {
    let Dao { mut env, council: [a, _b, _c], alice, bob } = delegate_dao(del_config());
    let carol = env.member(1_000);
    let dave = env.member(1_000);
    let eve = env.member(1_000);
    env.deposit(DEL, &carol, 50).unwrap();
    env.deposit(DEL, &dave, 300).unwrap();
    env.deposit(DEL, &eve, 200).unwrap();

    assert_eq!(code(env.start_election()), err(GovError::TermNotOver));
    env.warp(START + 1_000);
    let e = env.start_election().unwrap();
    assert_eq!(code(env.start_election()), err(GovError::ElectionInProgress));

    // Alice runs for seat 0, bob for seat 1, dave and eve for seat 2.
    for (m, seat) in [(&alice, 0), (&bob, 1), (&dave, 2), (&eve, 2)] {
        env.declare(m, e, seat).unwrap();
    }
    assert_eq!(code(env.declare(&carol, e, 0)), err(GovError::ProposalThresholdNotMet), "50 < 100");
    assert_eq!(code(env.declare(&alice, e, 1)), err(GovError::AlreadyCandidate));
    let [ka, kb, kd, ke] = [&alice, &bob, &dave, &eve].map(|m| m.kp.pubkey());
    assert_eq!(code(env.elect(&alice, e, vec![ka])), err(GovError::ElectionVotingNotOpen), "still declaring");

    env.warp(START + 1_050);
    assert_eq!(code(env.declare(&carol, e, 0)), err(GovError::CandidacyClosed));
    assert_eq!(code(env.elect(&alice, e, vec![])), err(GovError::InvalidBallot));
    assert_eq!(code(env.elect(&alice, e, vec![ka, kb, kd, ke])), err(GovError::InvalidBallot), "3 seats");
    assert_eq!(code(env.elect(&alice, e, vec![ka, ka])), err(GovError::DuplicateCandidate));
    assert_eq!(code(env.elect(&alice, e, vec![carol.kp.pubkey()])), err(GovError::NotCandidate));
    assert_eq!(code(env.elect(&alice, e, vec![kd, ke])), err(GovError::OneCandidatePerSeat), "both run for seat 2");
    env.elect(&alice, e, vec![ka, kb]).unwrap(); // 600 each
    env.elect(&bob, e, vec![kb, kd]).unwrap(); // 400 each
    env.elect(&carol, e, vec![kd]).unwrap(); // 50
    env.elect(&eve, e, vec![ke]).unwrap(); // 200
    assert!(env.elect(&alice, e, vec![ka]).is_err(), "one ballot each");
    assert_eq!(code(env.withdraw(DEL, &alice, 1)), err(GovError::TokensLocked));
    assert_eq!(code(env.withdraw(DEL, &dave, 1)), err(GovError::TokensLocked), "candidates' deposits too");

    assert_eq!(code(env.finalize_election(e)), err(GovError::VotingNotEnded));
    env.warp(START + 1_150);
    env.finalize_election(e).unwrap();
    assert_eq!(code(env.finalize_election(e)), err(GovError::AlreadyFinalized));
    let g = env.del_governance();
    assert_eq!(g.council, vec![ka, kb, kd], "seat by seat: alice 600, bob 1000, dave 450 beats eve 200");
    assert_eq!((g.council_term, g.term_ends_at), (2, START + 2_150));
    env.withdraw(DEL, &alice, 600).unwrap();

    // The old council is out; the new one is in.
    assert_eq!(code(env.del_propose(&a.kp, vec![env.pay_sol(&ka, 1)])), err(GovError::NotCouncilMember));
    env.del_propose(&bob.kp, vec![env.pay_sol(&kb, 1)]).unwrap();
}

#[test]
fn an_election_nobody_votes_in_keeps_the_council() {
    let Dao { mut env, council, alice, .. } = delegate_dao(del_config());
    env.warp(START + 1_000);
    let e = env.start_election().unwrap();
    env.declare(&alice, e, 0).unwrap();
    env.warp(START + 1_150);
    env.finalize_election(e).unwrap();
    let g = env.del_governance();
    assert_eq!(g.council, council.iter().map(|m| m.kp.pubkey()).collect::<Vec<_>>());
    assert_eq!(g.council_term, 1);
    // The term is still over, so another election can start straight away.
    env.start_election().unwrap();
}

#[test]
fn a_new_council_ends_the_old_councils_proposals() {
    let Dao { mut env, council: [a, b, _c], alice, .. } = delegate_dao(DelegateConfig { term_length: 100, ..del_config() });
    let p = env.del_propose(&a.kp, vec![env.pay_sol(&a.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.council_vote(&a.kp, p, V::For).unwrap();
    env.council_vote(&b.kp, p, V::For).unwrap();
    env.warp(START + 100);
    let e = env.start_election().unwrap();
    env.declare(&alice, e, 0).unwrap();
    env.warp(START + 110);
    env.queue(DEL, p).unwrap();
    env.warp(START + 150);
    env.elect(&alice, e, vec![alice.kp.pubkey()]).unwrap();
    env.warp(START + 250);
    env.finalize_election(e).unwrap();
    assert_eq!(env.del_governance().council, vec![alice.kp.pubkey(), b.kp.pubkey(), _c.kp.pubkey()], "alice takes seat 0; nobody ran for the others, so they keep their holders");
    assert_eq!(code(env.execute(DEL, p)), err(GovError::CouncilChanged));
    assert_eq!(env.del_state(p), ProposalState::Expired);
}

#[test]
fn members_recall_a_council_member() {
    let Dao { mut env, council: [a, b, c], alice, bob } = delegate_dao(del_config());
    let carol = env.member(1_000);
    env.deposit(DEL, &carol, 50).unwrap();
    assert_eq!(code(env.initiate_recall(&carol, b.kp.pubkey())), err(GovError::ProposalThresholdNotMet));
    assert_eq!(code(env.initiate_recall(&alice, carol.kp.pubkey())), err(GovError::NotCouncilMember));

    // A proposal that passes 2-0 with b's vote.
    let p = env.del_propose(&a.kp, vec![env.pay_sol(&a.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.council_vote(&a.kp, p, V::For).unwrap();
    env.council_vote(&b.kp, p, V::For).unwrap();

    // Recall b: 600 for, 400 against = 60%; 1000 of 1000 voted.
    let r = env.initiate_recall(&alice, b.kp.pubkey()).unwrap();
    env.vote_recall(&alice, r, V::For).unwrap();
    env.vote_recall(&bob, r, V::Against).unwrap();
    assert!(env.vote_recall(&alice, r, V::For).is_err(), "one vote each");
    assert_eq!(code(env.withdraw(DEL, &bob, 1)), err(GovError::TokensLocked));
    assert_eq!(code(env.finalize_recall(r)), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(code(env.vote_recall(&carol, r, V::For)), err(GovError::ProposalNotActive));
    env.queue(DEL, p).unwrap();
    env.finalize_recall(r).unwrap();
    assert!(env.recall(r).removed);
    assert_eq!(env.del_governance().council, vec![a.kp.pubkey(), Pubkey::default(), c.kp.pubkey()], "b's seat sits vacant");
    assert_eq!(code(env.finalize_recall(r)), err(GovError::AlreadyFinalized));
    assert_eq!(code(env.del_propose(&b.kp, vec![env.pay_sol(&b.kp.pubkey(), 1)])), err(GovError::NotCouncilMember));

    // b's vote on the queued proposal no longer counts: 1 vote, 2 needed.
    env.warp(START + 160);
    assert_eq!(code(env.execute(DEL, p)), err(GovError::QuorumNotReached));

    // A recall that fails leaves the member seated: 400 for, 600 against.
    let r2 = env.initiate_recall(&bob, c.kp.pubkey()).unwrap();
    env.vote_recall(&bob, r2, V::For).unwrap();
    env.vote_recall(&alice, r2, V::Against).unwrap();
    env.warp(START + 260);
    env.finalize_recall(r2).unwrap();
    assert!(!env.recall(r2).removed);
    assert!(env.del_governance().council.contains(&c.kp.pubkey()));
}

#[test]
fn rules_change_only_through_a_council_proposal() {
    let Dao { mut env, council: [a, b, _c], .. } = delegate_dao(del_config());
    let direct = Instruction {
        program_id: DEL,
        accounts: del::accounts::UpdateConfig { treasury: a.kp.pubkey(), governance: env.governance(DEL) }.to_account_metas(None),
        data: del::instruction::UpdateConfig { config: del_config() }.data(),
    };
    assert_eq!(code(env.send(&[direct], &[&a.kp])), err(GovError::Unauthorized));

    let bad = DelegateConfig { council_quorum: 4, ..del_config() };
    let new = DelegateConfig { term_length: 5_000, ..del_config() };
    let p_bad = env.del_propose(&a.kp, vec![env.del_update_config_ix(bad)]).unwrap();
    let p_new = env.del_propose(&a.kp, vec![env.del_update_config_ix(new)]).unwrap();
    env.warp(START + 10);
    for p in [p_bad, p_new] {
        env.council_vote(&a.kp, p, V::For).unwrap();
        env.council_vote(&b.kp, p, V::For).unwrap();
    }
    env.warp(START + 110);
    env.queue(DEL, p_bad).unwrap();
    env.queue(DEL, p_new).unwrap();
    env.warp(START + 160);
    assert_eq!(code(env.execute(DEL, p_bad)), err(GovError::InvalidQuorum), "quorum above council size");
    env.execute(DEL, p_new).unwrap();
    assert_eq!(env.del_governance().config, new);
}

#[test]
fn bad_councils_are_refused() {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.insecure_clone();
    let (x, y, z) = (Keypair::new().pubkey(), Keypair::new().pubkey(), Keypair::new().pubkey());
    let key = Keypair::new();
    env.dao = Pubkey::find_program_address(&[hub::DAO_SEED, key.pubkey().as_ref()], &hub::ID).0;
    env.treasury = hub::treasury_address(&env.dao);
    let create = env.ix_create_dao("Bad", DEL, &key.pubkey());
    for (council, config, why) in [
        (vec![x, y], del_config(), err(GovError::InvalidConfig)),
        (vec![x, y, x], del_config(), err(GovError::DuplicateCandidate)),
        (vec![x, y, z], DelegateConfig { council_quorum: 0, ..del_config() }, err(GovError::InvalidQuorum)),
        (vec![x, y, z], DelegateConfig { term_length: 0, ..del_config() }, err(GovError::InvalidConfig)),
        (vec![x, y, z], DelegateConfig { recall_quorum_bps: 0, ..del_config() }, err(GovError::InvalidQuorum)),
    ] {
        let record = env.ix_set_dao_token(&creator.pubkey(), env.mint, DEL);
        let init = env.ix_init_delegate(&creator.pubkey(), &creator.pubkey(), council, config);
        assert_eq!(code(env.send(&[create.clone(), record, init], &[&creator, &key])), why);
    }
}

#[test]
fn a_token_weighted_dao_can_switch_to_delegate() {
    let (mut env, alice, _bob) = dao_with_members();
    let council = [Keypair::new(), Keypair::new()];
    for m in &council {
        env.svm.airdrop(&m.pubkey(), 10_000_000_000).unwrap();
    }
    let config = DelegateConfig { council_size: 2, council_quorum: 2, ..del_config() };
    let init = stored(env.ix_init_delegate(&env.treasury, &env.treasury, council.iter().map(|m| m.pubkey()).collect(), config));
    let p = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(DEL)), init]);
    env.execute(TW, p).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(DEL).unwrap();
    assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (DEL, 2));

    let host = Keypair::new().pubkey();
    let p = env.del_propose(&council[0], vec![env.pay_sol(&host, 4_000_000)]).unwrap();
    let t = env.now();
    env.warp(t + 10);
    env.council_vote(&council[0], p, V::For).unwrap();
    env.council_vote(&council[1], p, V::For).unwrap();
    env.warp(t + 110);
    env.queue(DEL, p).unwrap();
    env.warp(t + 160);
    env.execute(DEL, p).unwrap();
    assert_eq!(env.lamports(&host), 4_000_000);
}

// ---- seats: ties drawn by ORAO ----

fn tie_randomness(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[del::ORAO_REQUEST_SEED, seed], &del::ORAO_VRF).0
}

/// ORAO's RandomnessV2 account: pending, or fulfilled with `randomness` (as in sortition.rs).
fn set_orao(env: &mut Env, seed: &[u8; 32], randomness: Option<[u8; 64]>) {
    let mut data = del::ORAO_RANDOMNESS_V2.to_vec();
    let client = Pubkey::new_unique();
    match randomness {
        None => {
            data.push(0);
            data.extend_from_slice(client.as_ref());
            data.extend_from_slice(seed);
            data.extend_from_slice(&0u32.to_le_bytes());
        }
        Some(r) => {
            data.push(1);
            data.extend_from_slice(client.as_ref());
            data.extend_from_slice(seed);
            data.extend_from_slice(&r);
        }
    }
    let account = solana_account::Account { lamports: 10_000_000, data, owner: del::ORAO_VRF, executable: false, rent_epoch: 0 };
    env.svm.set_account(tie_randomness(seed), account).unwrap();
}

fn finalize_with(env: &mut Env, e: Pubkey, randomness: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: DEL,
        accounts: del::accounts::FinalizeElection { governance: env.governance(DEL), election: e, randomness: Some(randomness) }.to_account_metas(None),
        data: del::instruction::FinalizeElection {}.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

fn resolve(env: &mut Env, e: Pubkey, randomness: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: DEL,
        accounts: del::accounts::ResolveTies { governance: env.governance(DEL), election: e, randomness }.to_account_metas(None),
        data: del::instruction::ResolveTies {}.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

/// Alice and bob tie for seat 1 (600 each); dave has seat 0 alone.
fn tied_election() -> (Env, Pubkey, [Pubkey; 3]) {
    let Dao { mut env, alice, bob, .. } = delegate_dao(del_config());
    let dave = env.member(1_000);
    env.deposit(DEL, &dave, 300).unwrap();
    env.warp(START + 1_000);
    let e = env.start_election().unwrap();
    env.declare(&alice, e, 1).unwrap();
    env.declare(&bob, e, 1).unwrap();
    env.declare(&dave, e, 0).unwrap();
    env.warp(START + 1_050);
    let [ka, kb, kd] = [&alice, &bob, &dave].map(|m| m.kp.pubkey());
    env.elect(&alice, e, vec![ka, kd]).unwrap(); // 600 to alice
    env.elect(&bob, e, vec![kb]).unwrap(); // 400 to bob
    env.elect(&dave, e, vec![kb]).unwrap(); // 300: bob 700
    (env, e, [ka, kb, kd])
}

#[test]
fn a_tied_seat_is_drawn_by_orao() {
    let Dao { mut env, alice, bob, .. } = delegate_dao(del_config());
    let carol = env.member(1_000);
    env.deposit(DEL, &carol, 600).unwrap();
    env.warp(START + 1_000);
    let e = env.start_election().unwrap();
    env.declare(&alice, e, 2).unwrap();
    env.declare(&carol, e, 2).unwrap();
    env.warp(START + 1_050);
    let [ka, kc] = [&alice, &carol].map(|m| m.kp.pubkey());
    env.elect(&alice, e, vec![ka]).unwrap(); // 600
    env.elect(&carol, e, vec![kc]).unwrap(); // 600: a tie
    let _ = &bob;
    env.warp(START + 1_150);

    let seed = del::tie_seed(&env.governance(DEL), env.election(e).id);
    let randomness = tie_randomness(&seed);
    assert_eq!(code(env.finalize_election(e)), err(GovError::WrongRandomnessAccount), "a tie needs ORAO's request");
    set_orao(&mut env, &seed, None);
    finalize_with(&mut env, e, randomness).unwrap();
    assert!(env.election(e).awaiting_draw);
    assert_eq!(code(resolve(&mut env, e, randomness)), err(GovError::RandomnessNotReady));

    // Randomness that picks the second tied candidate (carol).
    let mut r = [0u8; 64];
    loop {
        let h = solana_sha256_hasher::hashv(&[&r, &[2u8]]).to_bytes();
        if u64::from_le_bytes(h[..8].try_into().unwrap()) % 2 == 1 {
            break;
        }
        r[0] += 1;
    }
    set_orao(&mut env, &seed, Some(r));
    resolve(&mut env, e, randomness).unwrap();
    let g = env.del_governance();
    assert_eq!(g.council[2], kc);
    assert_eq!(g.council_term, 2);
}

#[test]
fn a_tie_orao_never_draws_goes_to_the_first_to_declare() {
    let Dao { mut env, alice, .. } = delegate_dao(del_config());
    let carol = env.member(1_000);
    env.deposit(DEL, &carol, 600).unwrap();
    env.warp(START + 1_000);
    let e = env.start_election().unwrap();
    env.declare(&alice, e, 0).unwrap();
    env.declare(&carol, e, 0).unwrap();
    env.warp(START + 1_050);
    let [ka, kc] = [&alice, &carol].map(|m| m.kp.pubkey());
    env.elect(&alice, e, vec![ka]).unwrap();
    env.elect(&carol, e, vec![kc]).unwrap();
    env.warp(START + 1_150);
    let seed = del::tie_seed(&env.governance(DEL), env.election(e).id);
    set_orao(&mut env, &seed, None);
    finalize_with(&mut env, e, tie_randomness(&seed)).unwrap();
    env.warp(START + 1_150 + del::TIE_DRAW_TIMEOUT);
    resolve(&mut env, e, tie_randomness(&seed)).unwrap();
    assert_eq!(env.del_governance().council[0], ka);
}

#[test]
fn no_tie_needs_no_draw() {
    let (mut env, e, [_ka, kb, kd]) = tied_election();
    env.warp(START + 1_150);
    env.finalize_election(e).unwrap();
    let g = env.del_governance();
    assert_eq!((g.council[0], g.council[1]), (kd, kb), "bob 700 beats alice 600 for seat 1");

}

#[test]
fn seat_labels_need_a_proposal() {
    let Dao { mut env, council: [a, b, _c], .. } = delegate_dao(del_config());
    let governance = env.governance(DEL);
    let label = |signer: Pubkey| Instruction {
        program_id: DEL,
        accounts: del::accounts::UpdateConfig { treasury: signer, governance }.to_account_metas(None),
        data: del::instruction::SetSeatLabel { seat: 0, label: "Treasurer".into() }.data(),
    };
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    assert_eq!(code(env.send(&[label(stranger.pubkey())], &[&stranger])), err(GovError::Unauthorized));

    let p = env.del_propose(&a.kp, vec![stored(label(env.treasury))]).unwrap();
    env.warp(START + 10);
    env.council_vote(&a.kp, p, V::For).unwrap();
    env.council_vote(&b.kp, p, V::For).unwrap();
    env.warp(START + 110);
    env.queue(DEL, p).unwrap();
    env.warp(START + 160);
    env.execute(DEL, p).unwrap();
    assert_eq!(env.del_governance().seat_labels[0], "Treasurer");
}
