//! Monthly allowances: a passed proposal grants one; the recipient claims
//! once per 30-day period from the treasury, in SOL or a token; a missed
//! period lapses; proposals change or revoke it.

use vortex_tests::*;

const TOKEN: Pubkey = litesvm_token::TOKEN_ID;
const MONTH: i64 = hub::ALLOWANCE_PERIOD;

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, mint, &TOKEN)
}

fn ix_create(env: &Env, id: u64, recipient: Pubkey, mint: Option<Pubkey>, amount: u64, ends_at: i64) -> Instruction {
    Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::CreateAllowance {
            treasury: env.treasury,
            dao: env.dao,
            allowance: hub::allowance_address(&env.dao, id),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: hub::instruction::CreateAllowance { recipient, mint, amount, ends_at }.data(),
    }
}

fn ix_revoke(env: &Env, id: u64) -> Instruction {
    Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::ChangeAllowance { treasury: env.treasury, dao: env.dao, allowance: hub::allowance_address(&env.dao, id) }
            .to_account_metas(None),
        data: hub::instruction::RevokeAllowance {}.data(),
    }
}

fn ix_update(env: &Env, id: u64, amount: u64, ends_at: i64) -> Instruction {
    Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::ChangeAllowance { treasury: env.treasury, dao: env.dao, allowance: hub::allowance_address(&env.dao, id) }
            .to_account_metas(None),
        data: hub::instruction::UpdateAllowance { amount, ends_at }.data(),
    }
}

fn claim(env: &mut Env, id: u64, who: &Keypair, mint: Option<Pubkey>) -> Result<(), FailedTransactionMetadata> {
    let treasury = env.treasury;
    let accounts = hub::accounts::ClaimAllowance {
        recipient: who.pubkey(),
        dao: env.dao,
        allowance: hub::allowance_address(&env.dao, id),
        treasury,
        mint,
        treasury_token: mint.map(|m| ata(&treasury, &m)),
        recipient_token: mint.map(|m| ata(&who.pubkey(), &m)),
        token_program: mint.map(|_| TOKEN),
        system_program: SYSTEM,
    }
    .to_account_metas(None);
    let ix = Instruction { program_id: hub::ID, accounts, data: hub::instruction::ClaimAllowance {}.data() };
    env.send(&[ix], &[who])
}

fn funded(env: &mut Env) -> Keypair {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 1_000_000_000).unwrap();
    kp
}

#[test]
fn sol_allowance_pays_once_a_period_and_missed_periods_lapse() {
    let (mut env, alice, _bob) = dao_with_members();
    let dev = funded(&mut env);
    let ends = env.now() + 6 * MONTH;
    let p = env.pass(TW, &alice, vec![stored(ix_create(&env, 1, dev.pubkey(), None, 100_000_000, ends))]);
    env.execute(TW, p).unwrap();
    assert_eq!(env.hub_dao().allowance_count, 1);
    let start = env.now();

    let before = env.lamports(&dev.pubkey());
    claim(&mut env, 1, &dev, None).unwrap();
    assert_eq!(env.lamports(&dev.pubkey()) - before, 100_000_000 - 5_000, "paid, less the fee");
    assert_eq!(code(claim(&mut env, 1, &dev, None)), err(GovError::AllowanceAlreadyClaimed));

    // Three periods pass unclaimed; only the current one pays.
    env.warp(start + 3 * MONTH + 10);
    let before = env.lamports(&dev.pubkey());
    claim(&mut env, 1, &dev, None).unwrap();
    assert_eq!(env.lamports(&dev.pubkey()) - before, 100_000_000 - 5_000);
    assert_eq!(code(claim(&mut env, 1, &dev, None)), err(GovError::AllowanceAlreadyClaimed));

    let stranger = funded(&mut env);
    assert_eq!(code(claim(&mut env, 1, &stranger, None)), err(GovError::Unauthorized));

    env.warp(ends);
    assert_eq!(code(claim(&mut env, 1, &dev, None)), err(GovError::AllowanceEnded));
}

#[test]
fn token_allowance_and_many_recipients() {
    let (mut env, alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let mint = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let t = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &mint).owner(&env.treasury).send().unwrap();
    MintTo::new(&mut env.svm, &creator, &mint, &t, 10_000).send().unwrap();
    let (dev, other) = (funded(&mut env), funded(&mut env));
    for who in [&dev, &other] {
        CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &mint).owner(&who.pubkey()).send().unwrap();
    }
    let ends = env.now() + MONTH;
    let p = env.pass(
        TW,
        &alice,
        vec![
            stored(ix_create(&env, 1, dev.pubkey(), Some(mint), 300, ends)),
            stored(ix_create(&env, 2, other.pubkey(), Some(mint), 200, ends)),
            stored(ix_create(&env, 3, dev.pubkey(), None, 1_000_000, ends)),
        ],
    );
    env.execute(TW, p).unwrap();
    claim(&mut env, 1, &dev, Some(mint)).unwrap();
    claim(&mut env, 2, &other, Some(mint)).unwrap();
    claim(&mut env, 3, &dev, None).unwrap();
    assert_eq!(env.token_balance(&ata(&dev.pubkey(), &mint)), 300);
    assert_eq!(env.token_balance(&ata(&other.pubkey(), &mint)), 200);
    assert_eq!(env.token_balance(&t), 9_500);
}

#[test]
fn only_a_proposal_grants_changes_or_revokes() {
    let (mut env, alice, _bob) = dao_with_members();
    let dev = funded(&mut env);
    let ends = env.now() + 2 * MONTH;
    // Signed by anyone but the treasury, the hub refuses.
    let mut ix = ix_create(&env, 1, dev.pubkey(), None, 1, ends);
    ix.accounts[0] = AccountMeta::new(dev.pubkey(), true);
    assert!(env.send(&[ix], &[&dev]).is_err());

    let p = env.pass(TW, &alice, vec![stored(ix_create(&env, 1, dev.pubkey(), None, 1_000_000, ends))]);
    env.execute(TW, p).unwrap();
    claim(&mut env, 1, &dev, None).unwrap();

    // More each month from the next period on; then revoked.
    let p = env.pass(TW, &alice, vec![stored(ix_update(&env, 1, 5_000_000, ends))]);
    env.execute(TW, p).unwrap();
    assert_eq!(code(claim(&mut env, 1, &dev, None)), err(GovError::AllowanceAlreadyClaimed), "the paid period stays paid");
    let start = env.now();
    let p = env.pass(TW, &alice, vec![stored(ix_revoke(&env, 1))]);
    env.execute(TW, p).unwrap();
    env.warp(start + MONTH);
    assert_eq!(code(claim(&mut env, 1, &dev, None)), err(GovError::AllowanceEnded));
}

#[test]
fn terms_are_checked() {
    let (mut env, alice, _bob) = dao_with_members();
    let dev = funded(&mut env);
    let too_long = env.now() + 366 * 24 * 60 * 60;
    let p = env.pass(TW, &alice, vec![stored(ix_create(&env, 1, dev.pubkey(), None, 1, too_long))]);
    assert_eq!(code(env.execute(TW, p)), err(GovError::InvalidAllowance));
}
