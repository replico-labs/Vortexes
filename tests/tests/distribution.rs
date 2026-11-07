//! The Distributor and raises: a pro-rata raise past its hard cap, paid
//! only into the DAO's treasury; refunds; claims by the owner and by the
//! bot's operator for a chat account; vesting; grants by proposal; welcome
//! grants approved by a Delegate council member.

use vortex_tests::*;

const TOKEN: Pubkey = litesvm_token::TOKEN_ID;
const DAY: i64 = 24 * 60 * 60;

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, mint, &TOKEN)
}

fn distributor_pda(dao: &Pubkey, mint: &Pubkey, operator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::DISTRIBUTOR_SEED, dao.as_ref(), mint.as_ref(), operator.as_ref()], &DIST).0
}
fn distributor_vault(d: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::DISTRIBUTOR_VAULT_SEED, d.as_ref()], &DIST).0
}
fn allocation_pda(d: &Pubkey, id: u64) -> Pubkey {
    Pubkey::find_program_address(&[dist::ALLOCATION_SEED, d.as_ref(), &id.to_le_bytes()], &DIST).0
}
fn raise_pda(dao: &Pubkey, key: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::RAISE_SEED, dao.as_ref(), key.as_ref()], &DIST).0
}
fn raise_vault(r: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::RAISE_VAULT_SEED, r.as_ref()], &DIST).0
}
fn contribution_pda(r: &Pubkey, who: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::CONTRIBUTION_SEED, r.as_ref(), who.as_ref()], &DIST).0
}

struct Setup {
    env: Env,
    operator: Keypair,
    distributor: Pubkey,
    usdc: Pubkey,
}

fn funded(env: &mut Env) -> Keypair {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 2_000_000_000).unwrap();
    kp
}

/// A DAO with its Distributor holding `sale` DAO tokens, and a USDC mint
/// the treasury has an account for.
fn setup(env: Env, sale: u64) -> Setup {
    let mut env = env;
    let operator = funded(&mut env);
    let creator = env.creator.insecure_clone();
    let distributor = distributor_pda(&env.dao, &env.mint, &operator.pubkey());
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::CreateDistributor {
            payer: creator.pubkey(),
            hub_dao: env.dao,
            mint: env.mint,
            distributor,
            vault: distributor_vault(&distributor),
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::CreateDistributor { operator: operator.pubkey(), claim_deadline: START + 400 * DAY }.data(),
    };
    env.send(&[ix], &[&creator]).unwrap();
    MintTo::new(&mut env.svm, &creator, &env.mint, &distributor_vault(&distributor), sale).send().unwrap();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &usdc).owner(&env.treasury).send().unwrap();
    Setup { env, operator, distributor, usdc }
}

fn terms(sale: u64) -> dist::RaiseTerms {
    dist::RaiseTerms {
        soft_cap: 10_000_000,
        hard_cap: 50_000_000,
        start: START,
        end: START + 7 * DAY,
        min_contribution: 0,
        max_contribution: 0,
        sale_supply: sale,
        vest_cliff: 0,
        vest_duration: 0,
        decision_window: 7 * DAY,
    }
}

fn create_raise(s: &mut Setup, decider: Pubkey, t: dist::RaiseTerms) -> Pubkey {
    let creator = s.env.creator.insecure_clone();
    let key = Keypair::new();
    let raise = raise_pda(&s.env.dao, &key.pubkey());
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::CreateRaise {
            payer: creator.pubkey(),
            create_key: key.pubkey(),
            hub_dao: s.env.dao,
            distributor: s.distributor,
            currency: s.usdc,
            raise,
            vault: raise_vault(&raise),
            prop_firm_vault: None,
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::CreateRaise { terms: t, decider }.data(),
    };
    s.env.send(&[ix], &[&creator, &key]).unwrap();
    raise
}

/// A contributor with `usdc` in their wallet.
fn contributor(s: &mut Setup, usdc: u64) -> Keypair {
    let kp = funded(&mut s.env);
    let creator = s.env.creator.insecure_clone();
    let a = CreateAssociatedTokenAccount::new(&mut s.env.svm, &creator, &s.usdc).owner(&kp.pubkey()).send().unwrap();
    MintTo::new(&mut s.env.svm, &creator, &s.usdc, &a, usdc).send().unwrap();
    kp
}

fn contribute(s: &mut Setup, raise: Pubkey, who: &Keypair, amount: u64, account: [u8; 32]) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::Contribute {
            contributor: who.pubkey(),
            raise,
            contribution: contribution_pda(&raise, &who.pubkey()),
            from: ata(&who.pubkey(), &s.usdc),
            vault: raise_vault(&raise),
            currency: s.usdc,
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::Contribute { amount, account }.data(),
    };
    s.env.send(&[ix], &[who])
}

fn accept(s: &mut Setup, raise: Pubkey, decider: &Keypair, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let treasury = s.env.treasury;
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::Accept {
            decider: decider.pubkey(),
            raise,
            vault: raise_vault(&raise),
            treasury,
            treasury_token: Some(ata(&treasury, &s.usdc)),
            distributor: s.distributor,
            distributor_vault: distributor_vault(&s.distributor),
            prop_firm_vault: None,
            vault_assets: None,
            share_mint: None,
            share_token_program: None,
            vault_program: None,
            currency: s.usdc,
            token_program: TOKEN,
        }
        .to_account_metas(None),
        data: dist::instruction::Accept { amount }.data(),
    };
    s.env.send(&[ix], &[decider])
}

fn next_allocation(s: &Setup) -> Pubkey {
    let d: dist::Distributor = s.env.account(&s.distributor);
    allocation_pda(&s.distributor, d.allocation_count + 1)
}

fn settle(s: &mut Setup, raise: Pubkey, who: &Pubkey) -> Result<(), FailedTransactionMetadata> {
    let creator = s.env.creator.insecure_clone();
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::Settle {
            payer: creator.pubkey(),
            raise,
            contribution: contribution_pda(&raise, who),
            distributor: s.distributor,
            distributor_vault: distributor_vault(&s.distributor),
            allocation: next_allocation(s),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::Settle {}.data(),
    };
    s.env.send(&[ix], &[&creator])
}

fn refund(s: &mut Setup, raise: Pubkey, who: &Pubkey) -> Result<(), FailedTransactionMetadata> {
    let creator = s.env.creator.insecure_clone();
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::WithdrawRefund {
            raise,
            contribution: contribution_pda(&raise, who),
            vault: raise_vault(&raise),
            to: ata(who, &s.usdc),
            currency: s.usdc,
            token_program: TOKEN,
        }
        .to_account_metas(None),
        data: dist::instruction::WithdrawRefund {}.data(),
    };
    s.env.send(&[ix], &[&creator])
}

fn claim_ix(s: &Setup, signer: &Pubkey, allocation: Pubkey, to: Pubkey, for_account: bool) -> Instruction {
    let accounts = dist::accounts::Claim {
        signer: *signer,
        distributor: s.distributor,
        vault: distributor_vault(&s.distributor),
        allocation,
        to,
        mint: s.env.mint,
        token_program: TOKEN,
    }
    .to_account_metas(None);
    let data = if for_account { dist::instruction::ClaimFor {}.data() } else { dist::instruction::Claim {}.data() };
    Instruction { program_id: DIST, accounts, data }
}

/// A wallet's account for the DAO token (created if needed).
fn dao_token_account(s: &mut Setup, owner: &Pubkey) -> Pubkey {
    let creator = s.env.creator.insecure_clone();
    let a = ata(owner, &s.env.mint);
    if s.env.svm.get_account(&a).is_none() {
        CreateAssociatedTokenAccount::new(&mut s.env.svm, &creator, &s.env.mint).owner(owner).send().unwrap();
    }
    a
}

#[test]
fn a_raise_past_its_hard_cap_pro_rata_into_the_treasury() {
    let (env, _alice, _bob) = dao_with_members();
    let mut s = setup(env, 300_000);
    let founder = funded(&mut s.env);
    let raise = create_raise(&mut s, founder.pubkey(), terms(300_000));
    let (a, b, c) = (contributor(&mut s, 100_000_000), contributor(&mut s, 100_000_000), contributor(&mut s, 100_000_000));
    let account = [7u8; 32];
    contribute(&mut s, raise, &a, 30_000_000, account).unwrap();
    contribute(&mut s, raise, &b, 20_000_000, [0u8; 32]).unwrap();
    contribute(&mut s, raise, &c, 10_000_000, [0u8; 32]).unwrap();
    assert_eq!(code(contribute(&mut s, raise, &a, 1, [9u8; 32])), err(GovError::Unauthorized), "one account per wallet");

    assert_eq!(code(accept(&mut s, raise, &founder, 55_000_000)), err(GovError::RaiseNotEnded));
    s.env.warp(START + 7 * DAY);
    assert_eq!(code(contribute(&mut s, raise, &a, 1, account)), err(GovError::RaiseNotOpen));
    assert_eq!(code(accept(&mut s, raise, &a, 55_000_000)), err(GovError::Unauthorized));
    assert_eq!(code(accept(&mut s, raise, &founder, 61_000_000)), err(GovError::AmountOutOfRange));
    accept(&mut s, raise, &founder, 55_000_000).unwrap(); // above the 50 hard cap
    let treasury = s.env.treasury;
    assert_eq!(s.env.token_balance(&ata(&treasury, &s.usdc)), 55_000_000, "into the DAO's treasury");
    assert!(s.env.svm.get_account(&ata(&founder.pubkey(), &s.usdc)).is_none(), "and none of it to the founder");

    for who in [&a, &b, &c] {
        settle(&mut s, raise, &who.pubkey()).unwrap();
    }
    assert_eq!(code(settle(&mut s, raise, &a.pubkey())), err(GovError::AlreadyFinalized));
    let al: dist::Allocation = s.env.account(&allocation_pda(&s.distributor, 1));
    assert_eq!((al.total, al.account, al.owner), (150_000, account, a.pubkey()), "half the sale");

    // Refunds: each their share of the 5 not accepted.
    refund(&mut s, raise, &a.pubkey()).unwrap();
    assert_eq!(s.env.token_balance(&ata(&a.pubkey(), &s.usdc)), 70_000_000 + 2_500_000);
    assert_eq!(code(refund(&mut s, raise, &a.pubkey())), err(GovError::NothingToClaim));

    // The bot claims a's tokens into their Protean wallet; b claims for themselves.
    let wallet = Keypair::new().pubkey();
    let to = dao_token_account(&mut s, &wallet);
    let op = s.operator.insecure_clone();
    let stranger = funded(&mut s.env);
    assert_eq!(code(s.env.send(&[claim_ix(&s, &stranger.pubkey(), allocation_pda(&s.distributor, 1), to, true)], &[&stranger])), err(GovError::Unauthorized));
    s.env.send(&[claim_ix(&s, &op.pubkey(), allocation_pda(&s.distributor, 1), to, true)], &[&op]).unwrap();
    assert_eq!(s.env.token_balance(&to), 150_000);
    let bto = dao_token_account(&mut s, &b.pubkey());
    assert_eq!(code(s.env.send(&[claim_ix(&s, &op.pubkey(), allocation_pda(&s.distributor, 2), bto, true)], &[&op])), err(GovError::Unauthorized), "b has no chat account");
    s.env.send(&[claim_ix(&s, &b.pubkey(), allocation_pda(&s.distributor, 2), bto, false)], &[&b]).unwrap();
    assert_eq!(s.env.token_balance(&bto), 100_000);
}

#[test]
fn below_the_soft_cap_or_without_a_decision_everyone_is_refunded() {
    let (env, _alice, _bob) = dao_with_members();
    let mut s = setup(env, 300_000);
    let founder = funded(&mut s.env);
    let low = create_raise(&mut s, founder.pubkey(), terms(100_000));
    let undecided = create_raise(&mut s, founder.pubkey(), terms(100_000));
    let a = contributor(&mut s, 100_000_000);
    contribute(&mut s, low, &a, 5_000_000, [0u8; 32]).unwrap();
    contribute(&mut s, undecided, &a, 20_000_000, [0u8; 32]).unwrap();
    s.env.warp(START + 7 * DAY);
    assert_eq!(code(accept(&mut s, low, &founder, 5_000_000)), err(GovError::AmountOutOfRange));
    refund(&mut s, low, &a.pubkey()).unwrap();
    assert_eq!(code(refund(&mut s, undecided, &a.pubkey())), err(GovError::NothingToClaim), "still in its decision window");
    s.env.warp(START + 14 * DAY);
    assert_eq!(code(accept(&mut s, undecided, &founder, 20_000_000)), err(GovError::DecisionClosed));
    refund(&mut s, undecided, &a.pubkey()).unwrap();
    assert_eq!(s.env.token_balance(&ata(&a.pubkey(), &s.usdc)), 100_000_000);
}

#[test]
fn a_raise_needs_its_tokens_in_the_distributor() {
    let (env, _alice, _bob) = dao_with_members();
    let mut s = setup(env, 1_000);
    let founder = funded(&mut s.env);
    let raise = create_raise(&mut s, founder.pubkey(), terms(5_000));
    let a = contributor(&mut s, 100_000_000);
    contribute(&mut s, raise, &a, 20_000_000, [0u8; 32]).unwrap();
    s.env.warp(START + 7 * DAY);
    assert_eq!(code(accept(&mut s, raise, &founder, 20_000_000)), err(GovError::NotFunded));
}

#[test]
fn grants_by_proposal_vest_and_unclaimed_tokens_go_back() {
    let (env, alice, _bob) = dao_with_members();
    let mut s = setup(env, 1_000);
    let member = Keypair::new().pubkey();
    let treasury = s.env.treasury;
    let grant = Instruction {
        program_id: DIST,
        accounts: dist::accounts::Allocate {
            treasury,
            distributor: s.distributor,
            vault: distributor_vault(&s.distributor),
            allocation: allocation_pda(&s.distributor, 1),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::Allocate { owner: member, account: [0u8; 32], amount: 400, start: START, cliff: 30 * DAY, duration: 120 * DAY }.data(),
    };
    let p = s.env.pass(TW, &alice, vec![stored(grant)]);
    s.env.execute(TW, p).unwrap();
    let al: dist::Allocation = s.env.account(&allocation_pda(&s.distributor, 1));
    assert_eq!(al.vested(START + 29 * DAY), 0);
    assert_eq!(al.vested(START + 30 * DAY), 100, "a quarter at the cliff");
    assert_eq!(al.vested(START + 200 * DAY), 400);

    // What nobody was promised goes back to the treasury.
    let treasury_dao = dao_token_account(&mut s, &treasury);
    let back = Instruction {
        program_id: DIST,
        accounts: dist::accounts::ReturnTokens {
            treasury,
            distributor: s.distributor,
            vault: distributor_vault(&s.distributor),
            treasury_token: treasury_dao,
            mint: s.env.mint,
            token_program: TOKEN,
        }
        .to_account_metas(None),
        data: dist::instruction::ReturnUnpromised {}.data(),
    };
    let p = s.env.pass(TW, &alice, vec![stored(back)]);
    s.env.execute(TW, p).unwrap();
    assert_eq!(s.env.token_balance(&treasury_dao), 600);
    assert_eq!(s.env.token_balance(&distributor_vault(&s.distributor)), 400);
}

#[test]
fn welcome_grants_need_a_delegate_council_member() {
    let mut env = Env::bare(TOKEN);
    let council = [env.member(0), env.member(0), env.member(0)];
    let keys = council.iter().map(|m| m.kp.pubkey()).collect::<Vec<_>>();
    let creator = env.creator.pubkey();
    env.create_dao("Senate", DEL, |env| env.ix_init_delegate(&creator, &creator, keys, del_config()));
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    let mut s = setup(env, 1_000);
    let treasury = s.env.treasury;
    let governance = s.env.governance(DEL);

    // A proposal turns welcome grants on: 100 each, 150 in all.
    let on = Instruction {
        program_id: DIST,
        accounts: dist::accounts::DistributorAdmin { treasury, distributor: s.distributor }.to_account_metas(None),
        data: dist::instruction::SetWelcome { amount: 100, cap: 150 }.data(),
    };
    let p = s.env.del_propose(&council[0].kp, vec![stored(on)]).unwrap();
    s.env.warp(START + 10);
    s.env.council_vote(&council[0].kp, p, del::VoteChoice::For).unwrap();
    s.env.council_vote(&council[1].kp, p, del::VoteChoice::For).unwrap();
    s.env.warp(START + 110);
    s.env.queue(DEL, p).unwrap();
    s.env.warp(START + 160);
    s.env.execute(DEL, p).unwrap();

    let grant = |s: &Setup, approver: &Pubkey, member: Pubkey, account: [u8; 32]| Instruction {
        program_id: DIST,
        accounts: dist::accounts::GrantWelcome {
            approver: *approver,
            hub_dao: s.env.dao,
            governance,
            distributor: s.distributor,
            vault: distributor_vault(&s.distributor),
            welcomed: Pubkey::find_program_address(&[dist::WELCOMED_SEED, s.distributor.as_ref(), &dist::welcome_key(&member, &account)], &DIST).0,
            allocation: next_allocation(s),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::GrantWelcome { member, account }.data(),
    };
    let newcomer = Keypair::new().pubkey();
    let account = [3u8; 32];
    let outsider = funded(&mut s.env);
    assert_eq!(code(s.env.send(&[grant(&s, &outsider.pubkey(), newcomer, account)], &[&outsider])), err(GovError::NotCouncilMember));
    let member = council[2].kp.insecure_clone();
    s.env.svm.airdrop(&member.pubkey(), 1_000_000_000).unwrap();
    s.env.send(&[grant(&s, &member.pubkey(), newcomer, account)], &[&member]).unwrap();
    assert!(s.env.send(&[grant(&s, &member.pubkey(), Keypair::new().pubkey(), account)], &[&member]).is_err(), "one welcome per account");
    assert_eq!(code(s.env.send(&[grant(&s, &member.pubkey(), Keypair::new().pubkey(), [4u8; 32])], &[&member])), err(GovError::WelcomeUnavailable), "the cap");
    let al: dist::Allocation = s.env.account(&allocation_pda(&s.distributor, 1));
    assert_eq!((al.total, al.account), (100, account));
}
