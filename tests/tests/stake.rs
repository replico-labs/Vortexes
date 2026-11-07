//! The stake pool: a DAO changes its token without changing its governance,
//! open proposals keep the pool they were made with, members move their
//! stake across with a converter, stake survives switches, and nothing is
//! locked without the member signing.

use vortex_tests::*;

const TOKEN: Pubkey = litesvm_token::TOKEN_ID;

fn ata(_env: &Env, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, mint, &TOKEN)
}

/// A new token, its pool opened, and `amounts[i]` minted to each `owners[i]`.
fn new_token(env: &mut Env, owners: &[(Pubkey, u64)]) -> Pubkey {
    let creator = env.creator.insecure_clone();
    let mint = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let open = env.ix_create_pool(mint, TOKEN);
    env.send(&[open], &[&creator]).unwrap();
    for (owner, amount) in owners {
        let a = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &mint).owner(owner).send().unwrap();
        if *amount > 0 {
            MintTo::new(&mut env.svm, &creator, &mint, &a, *amount).send().unwrap();
        }
    }
    mint
}

fn converter_pda(from_pool: &Pubkey, to_pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[stake::CONVERTER_SEED, from_pool.as_ref(), to_pool.as_ref()], &STAKE).0
}

fn converter_vault(converter: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[stake::CONVERTER_VAULT_SEED, converter.as_ref()], &STAKE).0
}

/// open_converter, funded with `fund` new tokens from the treasury (0: none).
fn ix_open_converter(env: &Env, from_pool: Pubkey, to_mint: Pubkey, num: u64, den: u64, fund: u64) -> Instruction {
    let to_pool = env.pool_for(to_mint);
    let converter = converter_pda(&from_pool, &to_pool);
    Instruction {
        program_id: STAKE,
        accounts: stake::accounts::OpenConverter {
            treasury: env.treasury,
            hub_dao: env.dao,
            dao_token: hub::dao_token_address(&env.dao),
            from_pool,
            to_pool,
            mint: to_mint,
            converter,
            converter_vault: converter_vault(&converter),
            treasury_account: (fund > 0).then(|| ata(env, &env.treasury, &to_mint)),
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: stake::instruction::OpenConverter { rate_num: num, rate_den: den, fund }.data(),
    }
}

fn ix_close_converter(env: &Env, from_pool: Pubkey, to_mint: Pubkey) -> Instruction {
    let converter = converter_pda(&from_pool, &env.pool_for(to_mint));
    Instruction {
        program_id: STAKE,
        accounts: stake::accounts::CloseConverter {
            treasury: env.treasury,
            converter,
            converter_vault: converter_vault(&converter),
            treasury_account: ata(env, &env.treasury, &to_mint),
            mint: to_mint,
            token_program: TOKEN,
        }
        .to_account_metas(None),
        data: stake::instruction::CloseConverter {}.data(),
    }
}

fn convert(env: &mut Env, m: &Member, from_mint: Pubkey, to_mint: Pubkey, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let from_pool = env.pool_for(from_mint);
    let to_pool = env.pool_for(to_mint);
    let converter = converter_pda(&from_pool, &to_pool);
    let owner = m.kp.pubkey();
    let ix = Instruction {
        program_id: STAKE,
        accounts: stake::accounts::Convert {
            owner,
            converter,
            converter_vault: converter_vault(&converter),
            from_pool,
            from_stake: Env::stake_pda(&from_pool, &owner),
            from_vault: Env::pool_vault_of(&from_pool),
            from_mint,
            treasury_from_account: ata(env, &env.treasury, &from_mint),
            to_pool,
            to_stake: Env::stake_pda(&to_pool, &owner),
            to_vault: Env::pool_vault_of(&to_pool),
            to_mint,
            from_token_program: TOKEN,
            to_token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: stake::instruction::Convert { amount }.data(),
    };
    env.send(&[ix], &[&m.kp])
}

fn transfer(env: &Env, from_owner: &Pubkey, to: Pubkey, mint: Pubkey, amount: u64) -> StoredInstruction {
    stored(spl_token_interface::instruction::transfer_checked(&TOKEN, &ata(env, from_owner, &mint), &mint, &to, from_owner, &[], amount, 6).unwrap())
}

#[test]
fn a_dao_changes_its_token_and_keeps_its_governance() {
    let (mut env, alice, bob) = dao_with_members();
    let (old_mint, old_pool) = (env.mint, env.pool());
    let governance = env.governance(TW);
    let new_mint = new_token(&mut env, &[(alice.kp.pubkey(), 1_000)]);
    let new_pool = env.pool_for(new_mint);

    // The change passes; another proposal is opened before it runs.
    let (_, change) = env.propose(TW, &alice, vec![stored(env.ix_set_dao_token(&env.treasury, new_mint, TW))]).unwrap();
    let prop = env.proposal_account(change);
    env.warp(prop.voting_starts_at);
    env.vote(TW, &alice, change, VoteChoice::For).unwrap();
    env.warp(prop.voting_ends_at);
    env.queue(TW, change).unwrap();
    let host = Keypair::new().pubkey();
    let (_, open) = env.propose(TW, &bob, vec![env.pay_sol(&host, 1_000_000)]).unwrap();
    env.warp(prop.voting_ends_at + config().timelock as i64);
    env.execute(TW, change).unwrap();

    // Same governance, same epoch; the record points at the new pool.
    let d = env.hub_dao();
    assert_eq!((d.governance_program, d.governance, d.epoch), (TW, governance, 1));
    assert_eq!((env.dao_token().mint, env.dao_token().pool), (new_mint, new_pool));

    // The open proposal still counts the old pool.
    let o = env.proposal_account(open);
    assert_eq!((o.pool, o.quorum_base), (old_pool, 1_000));
    env.warp(o.voting_starts_at);
    env.vote(TW, &alice, open, VoteChoice::For).unwrap();
    env.vote(TW, &bob, open, VoteChoice::For).unwrap();
    assert_eq!(code(env.withdraw_from(old_mint, TOKEN, &alice.kp, alice.ata, 1)), err(GovError::TokensLocked), "held until the vote ends");
    env.warp(o.voting_ends_at);
    env.queue(TW, open).unwrap();
    env.warp(o.voting_ends_at + config().timelock as i64);
    env.execute(TW, open).unwrap();
    assert_eq!(env.lamports(&host), 1_000_000);

    // New proposals need stake in the new pool, and the old one takes no more.
    env.mint = new_mint;
    assert_eq!(code(env.propose(TW, &alice, vec![env.pay_sol(&host, 1)])), err(GovError::ProposalThresholdNotMet));
    assert_eq!(code(env.stake_into(old_mint, TOKEN, &bob.kp, bob.ata, 100)), err(GovError::PoolNotCurrent));
    let alice_new = ata(&env, &alice.kp.pubkey(), &new_mint);
    env.stake_into(new_mint, TOKEN, &alice.kp, alice_new, 500).unwrap();
    let (_, p) = env.propose(TW, &alice, vec![env.pay_sol(&host, 1)]).unwrap();
    let p = env.proposal_account(p);
    assert_eq!((p.pool, p.quorum_base), (new_pool, 500));

    // The old stake is still the members' own to take out.
    env.withdraw_from(old_mint, TOKEN, &alice.kp, alice.ata, 600).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 1_000);
    assert_eq!(env.pool_account(&old_pool).total, 400);
}

#[test]
fn members_convert_their_stake_into_the_new_token() {
    let (mut env, alice, bob) = dao_with_members();
    let (old_mint, old_pool) = (env.mint, env.pool());
    let treasury = env.treasury;
    let new_mint = new_token(&mut env, &[(treasury, 1_300)]);
    let new_pool = env.pool_for(new_mint);
    let creator = env.creator.insecure_clone();
    CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &old_mint).owner(&treasury).send().unwrap();

    // Opening one before the token changes fails: it only pays into the current pool.
    let early = env.pass(TW, &alice, vec![stored(ix_open_converter(&env, old_pool, new_mint, 2, 1, 0))]);
    assert_eq!(code(env.execute(TW, early)), err(GovError::PoolNotCurrent));

    // One proposal: change the token, open a 2-for-1 converter funded with 1,200.
    let converter = converter_pda(&old_pool, &new_pool);
    let p = env.pass(
        TW,
        &alice,
        vec![stored(env.ix_set_dao_token(&env.treasury, new_mint, TW)), stored(ix_open_converter(&env, old_pool, new_mint, 2, 1, 1_200))],
    );
    env.execute(TW, p).unwrap();
    assert_eq!(env.token_balance(&converter_vault(&converter)), 1_200);
    assert_eq!(env.token_balance(&ata(&env, &treasury, &new_mint)), 100);

    convert(&mut env, &alice, old_mint, new_mint, 600).unwrap();
    assert_eq!(env.stake_in(&new_pool, &alice.kp.pubkey()).amount, 1_200);
    assert_eq!(env.stake_in(&old_pool, &alice.kp.pubkey()).amount, 0);
    assert_eq!(env.token_balance(&ata(&env, &treasury, &old_mint)), 600, "the old tokens go to the treasury");
    assert_eq!((env.pool_account(&old_pool).total, env.pool_account(&new_pool).total), (400, 1_200));
    assert_eq!(code(convert(&mut env, &bob, old_mint, new_mint, 25)), err(GovError::ConverterUnderfunded), "spent");

    // Alice governs with her converted stake; topping the converter up is a plain transfer into its vault.
    env.mint = new_mint;
    let top_up = env.pass(TW, &alice, vec![transfer(&env, &treasury, converter_vault(&converter), new_mint, 100)]);
    env.execute(TW, top_up).unwrap();
    assert_eq!(code(convert(&mut env, &bob, old_mint, new_mint, 400)), err(GovError::ConverterUnderfunded), "800 owed, 100 left");
    convert(&mut env, &bob, old_mint, new_mint, 25).unwrap();

    // The DAO closes the converter.
    let close = env.pass(TW, &alice, vec![stored(ix_close_converter(&env, old_pool, new_mint))]);
    assert_eq!(env.proposal_account(close).quorum_base, 1_250);
    env.execute(TW, close).unwrap();
    assert_eq!(env.token_balance(&converter_vault(&converter)), 0);
    assert_eq!(env.token_balance(&ata(&env, &treasury, &new_mint)), 50, "what was left goes back");
    assert_eq!(code(convert(&mut env, &bob, old_mint, new_mint, 10)), err(GovError::ConverterClosed));
}

#[test]
fn stake_carries_over_every_switch() {
    let (mut env, alice, bob) = dao_with_members();
    let s1 = Keypair::new();
    env.svm.airdrop(&s1.pubkey(), 10_000_000_000).unwrap();

    // token-weighted -> board: nobody's stake moves.
    let init_board = stored(env.ix_init_board(&env.treasury, &env.treasury, vec![s1.pubkey()], board_config(1)));
    let s = env.pass(TW, &alice, vec![stored(env.ix_propose_switch(BOARD)), init_board]);
    env.execute(TW, s).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(BOARD).unwrap();
    assert_eq!(env.voter_account(BOARD, &alice.kp.pubkey()).amount, 600);

    // board -> quadratic: members vote with what they already staked.
    let init_qv = stored(env.ix_init_governance(QV, &env.treasury, &env.treasury, config()));
    let s = env.board_propose(&s1, vec![stored(env.ix_propose_switch(QV)), init_qv]).unwrap();
    let t = env.now();
    env.warp(t + 50);
    env.execute(BOARD, s).unwrap();
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.apply_switch(QV).unwrap();
    assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (QV, 3));
    let host = Keypair::new().pubkey();
    let (_, p) = env.propose(QV, &bob, vec![env.pay_sol(&host, 5_000_000)]).unwrap();
    let prop = env.proposal_account(p);
    env.warp(prop.voting_starts_at);
    env.vote(QV, &alice, p, VoteChoice::For).unwrap();
    env.vote(QV, &bob, p, VoteChoice::For).unwrap();
    env.warp(prop.voting_ends_at);
    env.queue(QV, p).unwrap();
    env.warp(prop.voting_ends_at + config().timelock as i64);
    env.execute(QV, p).unwrap();
    assert_eq!(env.lamports(&host), 5_000_000);
    assert_eq!(env.pool_account(&env.pool()).total, 1_000);
}

#[test]
fn nothing_is_locked_without_the_current_governance_and_the_member() {
    let (mut env, alice, _bob) = dao_with_members();
    let pool = env.pool();
    let stake = Env::stake_pda(&pool, &alice.kp.pubkey());
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    let ix = |governance: Pubkey| Instruction {
        program_id: STAKE,
        accounts: stake::accounts::LockUntil { owner: alice.kp.pubkey(), governance, hub_dao: env.dao, pool, stake }.to_account_metas(None),
        data: stake::instruction::LockUntil { until: i64::MAX }.data(),
    };
    // Signed by the member and a key that isn't the DAO's governance.
    assert_eq!(code(env.send(&[ix(impostor.pubkey())], &[&alice.kp, &impostor])), err(GovError::NotActiveGovernance));
    assert_eq!(env.stake_in(&pool, &alice.kp.pubkey()).locked_until, 0);
    env.withdraw(TW, &alice, 600).unwrap();
}
