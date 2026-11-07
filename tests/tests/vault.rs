//! Prop-firm vaults: deposits and redemptions priced at windows, fees split
//! between traders and the treasury, the drawdown pause, moves only by
//! proposal and never near a window, the value read from Phoenix's own
//! accounts (a mainnet copy of its PerpAssetMap), and a prop firm's launch:
//! the raise pays into the vault and its first shares go to the Distributor.

use phoenix_rise_accounts::perp_asset_map::PerpAssetMap;
use phoenix_rise_accounts::trader::Trader;
use vault::phoenix_layout as L;
use vortex_tests::*;

const TOKEN: Pubkey = litesvm_token::TOKEN_ID;
const DAY: i64 = 24 * 60 * 60;
const MONTH: i64 = 30 * DAY;
const USDC: u64 = 1_000_000;
/// Shares have 9 decimals against USDC's 6: one share per dollar is 1,000 units per micro-dollar.
const RATE: u64 = 1_000;
/// The slot of the fixture's mark prices.
const FIXTURE_SLOT: u64 = 428_272_683;

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, mint, &TOKEN)
}

fn funded(env: &mut Env) -> Keypair {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 2_000_000_000).unwrap();
    kp
}

fn vconfig() -> vault::VaultConfig {
    vault::VaultConfig {
        interval: MONTH,
        freeze_before: DAY,
        settle_delay: 60 * 60,
        management_bps: 0,
        performance_bps: 2_000,
        max_drawdown_bps: 2_000,
        open_to_all: true,
        max_price_age_slots: 150,
    }
}

struct V {
    env: Env,
    alice: Member,
    usdc: Pubkey,
    shares: Pubkey,
    vault: Pubkey,
    trader: Keypair,
}

impl V {
    fn assets(&self) -> Pubkey {
        vault::assets_address(&self.vault)
    }
    fn escrow(&self) -> Pubkey {
        vault::escrow_address(&self.vault)
    }
    fn state(&self) -> vault::Vault {
        self.env.account(&self.vault)
    }
    fn epoch(&self) -> u64 {
        self.state().epoch
    }
    fn supply(&self) -> u64 {
        let m = self.env.svm.get_account(&self.shares).unwrap();
        u64::from_le_bytes(m.data[36..44].try_into().unwrap())
    }
}

fn create_vault_ix(env: &Env, creator: &Pubkey, shares: Pubkey, usdc: Pubkey, config: vault::VaultConfig, splits: Vec<vault::FeeSplitTerms>, seeder: Pubkey) -> Instruction {
    let v = vault::vault_address(&env.dao);
    Instruction {
        program_id: VAULT,
        accounts: vault::accounts::CreateVault {
            payer: env.creator.pubkey(),
            creator: *creator,
            hub_dao: env.dao,
            vault: v,
            share_mint: shares,
            asset_mint: usdc,
            assets: vault::assets_address(&v),
            escrow: vault::escrow_address(&v),
            asset_token_program: TOKEN,
            share_token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: vault::instruction::CreateVault { config, fee_splits: splits, seeder }.data(),
    }
}

/// A DAO with a vault over USDC whose shares are a fresh 9-decimal token,
/// fees split half to a trader and half to the treasury.
fn setup_with(config: vault::VaultConfig, seeder: impl FnOnce(&Env) -> Pubkey) -> V {
    let (mut env, alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let shares = CreateMint::new(&mut env.svm, &creator).decimals(9).send().unwrap();
    let trader = funded(&mut env);
    let splits = vec![
        vault::FeeSplitTerms { recipient: trader.pubkey(), bps: 5_000 },
        vault::FeeSplitTerms { recipient: env.treasury, bps: 5_000 },
    ];
    let seeder = seeder(&env);
    let ix = create_vault_ix(&env, &creator.pubkey(), shares, usdc, config, splits, seeder);
    env.send(&[ix], &[&creator]).unwrap();
    let vault = vault::vault_address(&env.dao);
    V { env, alice, usdc, shares, vault, trader }
}

fn setup() -> V {
    setup_with(vconfig(), |_| Pubkey::default())
}

/// A depositor with `usdc` dollars and empty share account.
fn depositor(v: &mut V, usdc: u64) -> Keypair {
    let kp = funded(&mut v.env);
    let creator = v.env.creator.insecure_clone();
    let a = CreateAssociatedTokenAccount::new(&mut v.env.svm, &creator, &v.usdc).owner(&kp.pubkey()).send().unwrap();
    MintTo::new(&mut v.env.svm, &creator, &v.usdc, &a, usdc * USDC).send().unwrap();
    CreateAssociatedTokenAccount::new(&mut v.env.svm, &creator, &v.shares).owner(&kp.pubkey()).send().unwrap();
    kp
}

fn deposit(v: &mut V, who: &Keypair, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let e = v.epoch();
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::RequestDeposit {
            user: who.pubkey(),
            vault: v.vault,
            epoch: vault::epoch_address(&v.vault, e),
            request: vault::request_address(&v.vault, e, &who.pubkey()),
            allowed: Some(vault::allowed_address(&v.vault, &who.pubkey())).filter(|a| v.env.svm.get_account(a).is_some()),
            from: ata(&who.pubkey(), &v.usdc),
            assets: v.assets(),
            asset_mint: v.usdc,
            asset_token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: vault::instruction::RequestDeposit { amount }.data(),
    };
    v.env.send(&[ix], &[who])
}

fn redeem(v: &mut V, who: &Keypair, amount: u64) -> Result<(), FailedTransactionMetadata> {
    let e = v.epoch();
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::RequestRedeem {
            user: who.pubkey(),
            vault: v.vault,
            epoch: vault::epoch_address(&v.vault, e),
            request: vault::request_address(&v.vault, e, &who.pubkey()),
            from: ata(&who.pubkey(), &v.shares),
            escrow: v.escrow(),
            share_mint: v.shares,
            share_token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: vault::instruction::RequestRedeem { amount }.data(),
    };
    v.env.send(&[ix], &[who])
}

fn cancel_deposit(v: &mut V, who: &Keypair) -> Result<(), FailedTransactionMetadata> {
    let e = v.epoch();
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::CancelDeposit {
            user: who.pubkey(),
            vault: v.vault,
            epoch: vault::epoch_address(&v.vault, e),
            request: vault::request_address(&v.vault, e, &who.pubkey()),
            to: ata(&who.pubkey(), &v.usdc),
            assets: v.assets(),
            asset_mint: v.usdc,
            asset_token_program: TOKEN,
        }
        .to_account_metas(None),
        data: vault::instruction::CancelDeposit {}.data(),
    };
    v.env.send(&[ix], &[who])
}

fn process_ix(v: &V, venues: &[AccountMeta]) -> Instruction {
    let e = v.epoch();
    let payer = v.env.creator.pubkey();
    let mut accounts = vault::accounts::ProcessWindow {
        payer,
        vault: v.vault,
        epoch: vault::epoch_address(&v.vault, e),
        assets: v.assets(),
        escrow: v.escrow(),
        share_mint: v.shares,
        share_token_program: TOKEN,
        system_program: SYSTEM,
    }
    .to_account_metas(None);
    accounts.extend_from_slice(venues);
    Instruction { program_id: VAULT, accounts, data: vault::instruction::ProcessWindow {}.data() }
}

fn process(v: &mut V, venues: &[AccountMeta]) -> Result<(), FailedTransactionMetadata> {
    let ix = process_ix(v, venues);
    let creator = v.env.creator.insecure_clone();
    v.env.send(&[ix], &[&creator])
}

/// Processes with a compute budget and returns the compute units it took.
fn process_cu(v: &mut V, venues: &[AccountMeta]) -> Result<u64, FailedTransactionMetadata> {
    let ix = process_ix(v, venues);
    let creator = v.env.creator.insecure_clone();
    let tx = solana_transaction::Transaction::new(&[&creator], solana_message::Message::new(&[ix], Some(&creator.pubkey())), v.env.svm.latest_blockhash());
    let res = v.env.svm.send_transaction(tx);
    v.env.svm.expire_blockhash();
    res.map(|m| m.compute_units_consumed)
}

fn claim_shares(v: &mut V, who: &Keypair, e: u64) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::ClaimShares {
            vault: v.vault,
            epoch: vault::epoch_address(&v.vault, e),
            request: vault::request_address(&v.vault, e, &who.pubkey()),
            to: ata(&who.pubkey(), &v.shares),
            escrow: v.escrow(),
            share_mint: v.shares,
            share_token_program: TOKEN,
        }
        .to_account_metas(None),
        data: vault::instruction::ClaimShares {}.data(),
    };
    v.env.send(&[ix], &[who])
}

fn claim_redemption(v: &mut V, who: &Keypair, e: u64) -> Result<(), FailedTransactionMetadata> {
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::ClaimRedemption {
            vault: v.vault,
            epoch: vault::epoch_address(&v.vault, e),
            request: vault::request_address(&v.vault, e, &who.pubkey()),
            to: ata(&who.pubkey(), &v.usdc),
            assets: v.assets(),
            asset_mint: v.usdc,
            asset_token_program: TOKEN,
        }
        .to_account_metas(None),
        data: vault::instruction::ClaimRedemption {}.data(),
    };
    v.env.send(&[ix], &[who])
}

fn claim_fees(v: &mut V, index: u8, to: Pubkey) -> Result<(), FailedTransactionMetadata> {
    let creator = v.env.creator.insecure_clone();
    let ix = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::ClaimFees { vault: v.vault, to, escrow: v.escrow(), share_mint: v.shares, share_token_program: TOKEN }
            .to_account_metas(None),
        data: vault::instruction::ClaimFees { index }.data(),
    };
    v.env.send(&[ix], &[&creator])
}

fn admin_ix(v: &V, data: Vec<u8>) -> Instruction {
    Instruction {
        program_id: VAULT,
        accounts: vault::accounts::Admin { treasury: v.env.treasury, vault: v.vault }.to_account_metas(None),
        data,
    }
}

/// The vault sends `amount` USDC from its idle money to `to`, through `execute` (a proposal).
fn move_out_ix(v: &V, to: Pubkey, amount: u64) -> Instruction {
    let inner = spl_token_interface::instruction::transfer_checked(&TOKEN, &v.assets(), &v.usdc, &to, &v.vault, &[], amount, 6).unwrap();
    let mut accounts = vault::accounts::Execute { treasury: v.env.treasury, vault: v.vault }.to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(TOKEN, false));
    for m in inner.accounts {
        accounts.push(if m.is_writable { AccountMeta::new(m.pubkey, false) } else { AccountMeta::new_readonly(m.pubkey, false) });
    }
    Instruction { program_id: VAULT, accounts, data: vault::instruction::Execute { data: inner.data }.data() }
}

fn by_proposal(v: &mut V, ixs: Vec<Instruction>) -> Result<(), FailedTransactionMetadata> {
    let alice = Member { kp: v.alice.kp.insecure_clone(), ata: v.alice.ata };
    let p = v.env.pass(TW, &alice, ixs.into_iter().map(stored).collect());
    v.env.execute(TW, p)
}

fn window(v: &mut V) {
    let at = v.state().next_window_at;
    v.env.warp(at);
}

#[test]
fn deposits_and_redemptions_are_priced_at_windows_with_fees_on_gains() {
    let mut v = setup();
    let (a, b) = (depositor(&mut v, 10_000), depositor(&mut v, 10_000));
    deposit(&mut v, &a, 1_000 * USDC).unwrap();
    assert_eq!(v.env.token_balance(&v.assets()), 1_000 * USDC);
    assert_eq!(code(claim_shares(&mut v, &a, 1)), err(GovError::NotProcessed));
    assert_eq!(code(process(&mut v, &[])), err(GovError::WindowNotDue));

    // Window 1: the first deposit buys one share per dollar.
    window(&mut v);
    process(&mut v, &[]).unwrap();
    claim_shares(&mut v, &a, 1).unwrap();
    assert_eq!(v.env.token_balance(&ata(&a.pubkey(), &v.shares)), 1_000 * USDC * RATE);
    assert_eq!(v.state().high_water, vault::ONE / RATE as u128, "a dollar a share");

    // The desk makes 200: the vault is worth 1,200. B asks to deposit 600, A to redeem half.
    let creator = v.env.creator.insecure_clone();
    let assets = v.assets();
    MintTo::new(&mut v.env.svm, &creator, &v.usdc, &assets, 200 * USDC).send().unwrap();
    deposit(&mut v, &b, 600 * USDC).unwrap();
    redeem(&mut v, &a, 500 * USDC * RATE).unwrap();
    window(&mut v);
    process(&mut v, &[]).unwrap();

    // Performance fee: 20% of the 200 gain = 40, as shares worth 40 at the new price.
    // Price after fees = 1,200 / (1,000 + fee shares) = 1.16 a share.
    let st = v.state();
    let fee_shares: u64 = st.fee_splits.iter().map(|f| f.owed).sum();
    let price_after = 1_200 * USDC as u128 * vault::ONE / (1_000 * USDC * RATE + fee_shares) as u128;
    assert!((fee_shares as i128 - (40 * USDC * RATE) as i128 * 1_000 / 1_160).abs() < 1_000, "fee shares {fee_shares}");
    assert_eq!(st.fee_splits[0].owed + st.fee_splits[1].owed, fee_shares);
    assert_eq!(st.high_water, price_after);

    // A's 500 shares redeem at that price; B's 600 buy shares at it.
    let e2: vault::VaultEpoch = v.env.account(&vault::epoch_address(&v.vault, 2));
    let supply = (1_000 * USDC * RATE + fee_shares) as u128;
    let redeemed = 500 * (USDC * RATE) as u128 * (1_200 * USDC) as u128 / supply;
    assert_eq!(e2.redeem_assets as u128, redeemed);
    assert_eq!(e2.deposit_shares as u128, (600 * USDC) as u128 * (supply - 500 * (USDC * RATE) as u128) / ((1_200 * USDC) as u128 - redeemed));
    assert!((redeemed as i128 - 580 * USDC as i128).abs() < 10, "500 shares at $1.16: {redeemed}");
    claim_redemption(&mut v, &a, 2).unwrap();
    assert_eq!(v.env.token_balance(&ata(&a.pubkey(), &v.usdc)), 9_000 * USDC + e2.redeem_assets);
    claim_shares(&mut v, &b, 2).unwrap();
    assert_eq!(v.env.token_balance(&ata(&b.pubkey(), &v.shares)), e2.deposit_shares);
    assert_eq!(code(claim_redemption(&mut v, &a, 2)), err(GovError::NothingToClaim));

    // Each fee recipient claims its own, into its own account only.
    let trader_shares = CreateAssociatedTokenAccount::new(&mut v.env.svm, &creator, &v.shares).owner(&v.trader.pubkey()).send().unwrap();
    let wrong = ata(&b.pubkey(), &v.shares);
    assert_eq!(code(claim_fees(&mut v, 0, wrong)), err(GovError::Unauthorized));
    claim_fees(&mut v, 0, trader_shares).unwrap();
    assert_eq!(v.env.token_balance(&trader_shares), st.fee_splits[0].owed);
    assert_eq!(v.state().fee_splits[0].owed, 0);

    // Cancelling before the window returns the money.
    deposit(&mut v, &b, 100 * USDC).unwrap();
    let before = v.env.token_balance(&ata(&b.pubkey(), &v.usdc));
    cancel_deposit(&mut v, &b).unwrap();
    assert_eq!(v.env.token_balance(&ata(&b.pubkey(), &v.usdc)), before + 100 * USDC);
    assert_eq!(v.state().pending_deposit_assets, 0);
}

#[test]
fn money_moves_only_by_proposal_never_near_a_window_and_a_drawdown_pauses_trading() {
    let mut v = setup();
    let a = depositor(&mut v, 10_000);
    deposit(&mut v, &a, 1_000 * USDC).unwrap();
    window(&mut v);
    process(&mut v, &[]).unwrap();
    let creator = v.env.creator.insecure_clone();
    let desk = CreateAssociatedTokenAccount::new(&mut v.env.svm, &creator, &v.usdc).owner(&v.trader.pubkey()).send().unwrap();

    // Nobody but the treasury can move the vault's money.
    let mut ix = move_out_ix(&v, desk, 100 * USDC);
    ix.accounts[0] = AccountMeta::new_readonly(v.trader.pubkey(), true);
    let trader = v.trader.insecure_clone();
    assert!(v.env.send(&[ix], &[&trader]).is_err());

    // Never the share mint: the vault is its only minter.
    let mint = spl_token_interface::instruction::mint_to(&TOKEN, &v.shares, &desk, &v.vault, &[], 1).unwrap();
    let mut mint_ix = move_out_ix(&v, desk, 1);
    mint_ix.accounts.truncate(2);
    mint_ix.accounts.push(AccountMeta::new_readonly(TOKEN, false));
    mint_ix.accounts.extend(mint.accounts.iter().map(|m| if m.is_writable { AccountMeta::new(m.pubkey, false) } else { AccountMeta::new_readonly(m.pubkey, false) }));
    mint_ix.data = vault::instruction::Execute { data: mint.data }.data();
    assert!(by_proposal(&mut v, vec![mint_ix]).is_err());

    // A proposal moves 300 out (the desk loses it): the vault is worth 700, 30% below its high.
    { let ix = move_out_ix(&v, desk, 300 * USDC); by_proposal(&mut v, vec![ix]) }.unwrap();
    assert_eq!(v.env.token_balance(&desk), 300 * USDC);
    assert!(v.state().last_move_at > 0);

    // The keeper's check pauses trading; only a proposal resumes it.
    let check = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::CheckDrawdown { vault: v.vault, assets: v.assets(), share_mint: v.shares }.to_account_metas(None),
        data: vault::instruction::CheckDrawdown {}.data(),
    };
    v.env.send(&[check], &[&creator]).unwrap();
    assert!(v.state().paused);
    { let ix = admin_ix(&v, vault::instruction::Resume {}.data()); by_proposal(&mut v, vec![ix]) }.unwrap();
    assert!(!v.state().paused);

    // In the day before a window, moves are frozen.
    let at = v.state().next_window_at;
    v.env.warp(at - DAY / 2);
    let p = {
        let alice = Member { kp: v.alice.kp.insecure_clone(), ata: v.alice.ata };
        v.env.pass(TW, &alice, vec![stored(move_out_ix(&v, desk, USDC))])
    };
    assert_eq!(code(v.env.execute(TW, p)), err(GovError::MovesFrozen));

    // A window right after a move waits for the money to settle.
    let mut cfg = vconfig();
    cfg.settle_delay = 2 * DAY;
    let mut v2 = setup_with(cfg, |_| Pubkey::default());
    let a2 = depositor(&mut v2, 1_000);
    deposit(&mut v2, &a2, 100 * USDC).unwrap();
    let creator2 = creator_of(&v2);
    let desk2 = CreateAssociatedTokenAccount::new(&mut v2.env.svm, &creator2, &v2.usdc).owner(&v2.trader.pubkey()).send().unwrap();
    let at = v2.state().next_window_at;
    v2.env.warp(at - DAY - 1_000);
    { let ix = move_out_ix(&v2, desk2, 0); by_proposal(&mut v2, vec![ix]) }.unwrap();
    let moved = v2.state().last_move_at;
    v2.env.warp(at);
    assert_eq!(code(process(&mut v2, &[])), err(GovError::Settling));
    v2.env.warp(moved + 2 * DAY);
    process(&mut v2, &[]).unwrap();
}

fn creator_of(v: &V) -> Keypair {
    v.env.creator.insecure_clone()
}

#[test]
fn a_closed_vault_takes_deposits_only_from_allowed_addresses() {
    let mut cfg = vconfig();
    cfg.open_to_all = false;
    let mut v = setup_with(cfg, |_| Pubkey::default());
    let a = depositor(&mut v, 1_000);
    assert_eq!(code(deposit(&mut v, &a, USDC)), err(GovError::NotAllowed));
    let allow = Instruction {
        program_id: VAULT,
        accounts: vault::accounts::SetAllowed {
            treasury: v.env.treasury,
            vault: v.vault,
            allowed: vault::allowed_address(&v.vault, &a.pubkey()),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: vault::instruction::SetAllowed { who: a.pubkey(), ok: true }.data(),
    };
    by_proposal(&mut v, vec![allow]).unwrap();
    deposit(&mut v, &a, USDC).unwrap();
}

#[test]
fn only_the_creator_or_treasury_opens_a_vault_and_takes_the_mint() {
    let (mut env, _alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let stranger = funded(&mut env);
    let shares = CreateMint::new(&mut env.svm, &stranger).decimals(9).send().unwrap();
    let mut ix = create_vault_ix(&env, &stranger.pubkey(), shares, usdc, vconfig(), vec![], Pubkey::default());
    ix.accounts[0] = AccountMeta::new(stranger.pubkey(), true);
    assert_eq!(code(env.send(&[ix], &[&stranger])), err(GovError::Unauthorized));

    // A token that already has holders can't become the shares.
    let used = CreateMint::new(&mut env.svm, &creator).decimals(9).send().unwrap();
    let holder = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &used).owner(&creator.pubkey()).send().unwrap();
    MintTo::new(&mut env.svm, &creator, &used, &holder, 1).send().unwrap();
    let ix = create_vault_ix(&env, &creator.pubkey(), used, usdc, vconfig(), vec![], Pubkey::default());
    assert_eq!(code(env.send(&[ix], &[&creator])), err(GovError::InvalidConfig));

    let fresh = CreateMint::new(&mut env.svm, &creator).decimals(9).send().unwrap();
    let ix = create_vault_ix(&env, &creator.pubkey(), fresh, usdc, vconfig(), vec![], Pubkey::default());
    env.send(&[ix], &[&creator]).unwrap();
    let m = env.svm.get_account(&fresh).unwrap();
    assert_eq!(&m.data[4..36], vault::vault_address(&env.dao).as_ref(), "the vault is the only minter now");
}

// ---------------------------------------------------------------
// PHOENIX
// ---------------------------------------------------------------

/// The mainnet PerpAssetMap, padded back to its full size.
fn phoenix_map() -> Vec<u8> {
    let mut d = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/phoenix-perp-asset-map-mainnet.bin")).unwrap();
    d.resize(L::MAP_LEN, 0);
    d
}

/// A Phoenix trader account: `authority`, collateral (micro-USD), and positions (asset id, base lots, virtual quote lots, funding snapshot).
fn phoenix_trader(authority: &Pubkey, collateral: i64, positions: &[(u32, i64, i64, i64)], withdraw_queue: u32) -> Vec<u8> {
    let capacity = 4usize;
    let mut d = vec![0u8; L::TRADER_ENTRIES + capacity * L::POSITION_ENTRY_LEN];
    d[..8].copy_from_slice(&L::TRADER_DISCRIMINANT);
    d[L::TRADER_AUTHORITY..L::TRADER_AUTHORITY + 32].copy_from_slice(authority.as_ref());
    d[L::TRADER_COLLATERAL..L::TRADER_COLLATERAL + 8].copy_from_slice(&collateral.to_le_bytes());
    d[L::TRADER_WITHDRAW_QUEUE_NODE..L::TRADER_WITHDRAW_QUEUE_NODE + 4].copy_from_slice(&withdraw_queue.to_le_bytes());
    // max_positions, so the header reads as a real one
    d[112..116].copy_from_slice(&(capacity as u32).to_le_bytes());
    d[L::TRADER_POSITIONS_LEN..L::TRADER_POSITIONS_LEN + 8].copy_from_slice(&(positions.len() as u64).to_le_bytes());
    d[L::TRADER_POSITIONS_CAPACITY..L::TRADER_POSITIONS_CAPACITY + 8].copy_from_slice(&(capacity as u64).to_le_bytes());
    for (i, (asset, base, vq, snap)) in positions.iter().enumerate() {
        let at = L::TRADER_ENTRIES + i * L::POSITION_ENTRY_LEN;
        d[at..at + 8].copy_from_slice(&(*asset as u64).to_le_bytes());
        d[at + L::POS_BASE_LOTS..at + L::POS_BASE_LOTS + 8].copy_from_slice(&base.to_le_bytes());
        d[at + L::POS_VIRTUAL_QUOTE_LOTS..at + L::POS_VIRTUAL_QUOTE_LOTS + 8].copy_from_slice(&vq.to_le_bytes());
        d[at + L::POS_FUNDING_SNAPSHOT..at + L::POS_FUNDING_SNAPSHOT + 8].copy_from_slice(&snap.to_le_bytes());
    }
    d
}

/// 8-byte aligned copy, as account data is on-chain.
fn aligned(d: &[u8]) -> (Vec<u64>, usize) {
    let mut words = vec![0u64; d.len().div_ceil(8)];
    unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), words.as_mut_ptr() as *mut u8, d.len()) };
    (words, d.len())
}

#[test]
fn phoenix_offsets_match_phoenixs_own_crate() {
    // Markets: every active perp market reads the same through both.
    let map = phoenix_map();
    let (w, n) = aligned(&map);
    let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, n) };
    let view = PerpAssetMap::try_from_account_bytes(bytes).unwrap();
    let mut checked = 0;
    for entry in view.iter() {
        let entry = entry.unwrap();
        let m = entry.metadata;
        let id = m.static_market_params().asset_id();
        let ours = vault::phoenix_market(&map, id).unwrap().expect("found by asset id");
        assert_eq!(ours.tick_size, m.static_market_params().tick_size.as_inner(), "{id}");
        assert_eq!(ours.mark_ticks, m.oracle_price().mark_price.price.ticks.as_inner(), "{id}");
        assert_eq!(ours.mark_slot, m.oracle_price().mark_price.price.slot, "{id}");
        assert_eq!(ours.cumulative_funding, m.funding_accumulator().cumulative_funding_rate.as_inner(), "{id}");
        checked += 1;
    }
    assert_eq!(checked, 44, "every perp market on the mainnet copy");
    let sol = vault::phoenix_market(&map, 0).unwrap().unwrap();
    assert_eq!((sol.tick_size, sol.mark_ticks, sol.mark_slot), (100, 7191, FIXTURE_SLOT), "SOL at $71.91");

    // Traders: what we write at our offsets, Phoenix's view reads back.
    let authority = Pubkey::new_unique();
    let t = phoenix_trader(&authority, 123_456_789, &[(0, 250, -1_797_000, 42), (2, -3, 9_000_000, -7)], 9);
    let (w, n) = aligned(&t);
    let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, n) };
    let view = Trader::try_from_account_bytes(bytes).unwrap();
    assert_eq!(view.header.authority, authority.to_bytes());
    assert_eq!(view.header.trader_state.quote_lot_collateral.as_inner(), 123_456_789);
    assert_eq!(view.header.withdraw_queue_node(), Some(9));
    let ps: Vec<_> = view.positions().map(|(id, p)| (id, p.to_position())).collect();
    assert_eq!(ps.len(), 2);
    assert_eq!((ps[0].0, ps[0].1.base_lot_position.as_inner(), ps[0].1.virtual_quote_lot_position.as_inner(), ps[0].1.cumulative_funding_snapshot.as_inner()), (0, 250, -1_797_000, 42));
    assert_eq!((ps[1].0, ps[1].1.base_lot_position.as_inner(), ps[1].1.virtual_quote_lot_position.as_inner(), ps[1].1.cumulative_funding_snapshot.as_inner()), (2, -3, 9_000_000, -7));
}

fn set_phoenix_account(env: &mut Env, key: Pubkey, data: Vec<u8>) {
    let lamports = env.svm.minimum_balance_for_rent_exemption(data.len());
    env.svm
        .set_account(key, solana_account::Account { lamports, data, owner: vault::PHOENIX_PROGRAM_ID, executable: false, rent_epoch: 0 })
        .unwrap();
}

fn set_slot(env: &mut Env, slot: u64) {
    let mut clock: solana_clock::Clock = env.svm.get_sysvar();
    clock.slot = slot;
    env.svm.set_sysvar(&clock);
}

#[test]
fn the_vault_values_its_phoenix_accounts_itself() {
    let mut v = setup();
    let a = depositor(&mut v, 10_000);
    deposit(&mut v, &a, 1_000 * USDC).unwrap();
    window(&mut v);
    process(&mut v, &[]).unwrap();

    // The DAO registers its Phoenix accounts: the mainnet PerpAssetMap, a
    // cross account with 500 collateral, and a trader's subaccount that
    // holds 100 collateral and is long 2.5 SOL bought at $71.88.
    let map = Pubkey::new_unique();
    set_phoenix_account(&mut v.env, map, phoenix_map());
    let (cross, sub) = (Pubkey::new_unique(), Pubkey::new_unique());
    let sol = vault::phoenix_market(&phoenix_map(), 0).unwrap().unwrap();
    let base = 250i64; // SOL base lots have 2 decimals
    let entry_cost = -(base * 7188 * sol.tick_size as i64);
    let snapshot = sol.cumulative_funding - 3; // 3 micro-dollars a lot of funding to pay since
    set_phoenix_account(&mut v.env, cross, phoenix_trader(&v.vault, 500 * USDC as i64, &[], 0));
    set_phoenix_account(&mut v.env, sub, phoenix_trader(&v.vault, 100 * USDC as i64, &[(0, base, entry_cost, snapshot)], 0));
    let set = admin_ix(&v, vault::instruction::SetVenues { perp_asset_map: map, phoenix_traders: vec![cross, sub], extra_tokens: vec![] }.data());
    by_proposal(&mut v, vec![set]).unwrap();
    // The desk took 600 of the idle 1,000 to Phoenix (here: out of the vault).
    let creator = creator_of(&v);
    let desk = CreateAssociatedTokenAccount::new(&mut v.env.svm, &creator, &v.usdc).owner(&creator.pubkey()).send().unwrap();
    { let ix = move_out_ix(&v, desk, 600 * USDC); by_proposal(&mut v, vec![ix]) }.unwrap();

    // Value = 400 idle + 500 + 100 + 2.5 SOL x (71.91 - 71.88) - 250 lots x 3 micro-dollars of funding.
    let pnl = base as i128 * 7191 * sol.tick_size as i128 + entry_cost as i128;
    let funding = -(sol.cumulative_funding as i128 - snapshot as i128) * base as i128;
    let value = (1_000 * USDC) as i128 + pnl + funding;
    assert_eq!(pnl, 75_000, "3 cents on 2.5 SOL: 7.5 cents");
    assert_eq!(funding, -750);

    let b = depositor(&mut v, 10_000);
    deposit(&mut v, &b, 1_000 * USDC).unwrap();
    window(&mut v);
    set_slot(&mut v.env, FIXTURE_SLOT + 10);
    let venues = [AccountMeta::new_readonly(map, false), AccountMeta::new_readonly(cross, false), AccountMeta::new_readonly(sub, false)];

    // Every account, in order, or no window.
    assert_eq!(code(process(&mut v, &venues[..2])), err(GovError::VenueMismatch));
    assert_eq!(code(process(&mut v, &[venues[0].clone(), venues[2].clone(), venues[1].clone()])), err(GovError::VenueMismatch));
    // A stale mark price makes the window wait.
    set_slot(&mut v.env, FIXTURE_SLOT + 151);
    assert_eq!(code(process(&mut v, &venues)), err(GovError::StalePrice));
    // So does money in Phoenix's withdraw queue.
    set_slot(&mut v.env, FIXTURE_SLOT + 10);
    set_phoenix_account(&mut v.env, cross, phoenix_trader(&v.vault, 500 * USDC as i64, &[], 4));
    assert_eq!(code(process(&mut v, &venues)), err(GovError::Settling));
    // Someone else's account is refused.
    set_phoenix_account(&mut v.env, cross, phoenix_trader(&Pubkey::new_unique(), 500 * USDC as i64, &[], 0));
    assert_eq!(code(process(&mut v, &venues)), err(GovError::VenueMismatch));
    // Spot collateral (here SOL) can't be valued.
    let mut spot = phoenix_trader(&v.vault, 500 * USDC as i64, &[(0xFFFF_FFFF, 0, 0, 0)], 0);
    spot[L::TRADER_ENTRIES + 8] = 1;
    set_phoenix_account(&mut v.env, cross, spot);
    assert_eq!(code(process(&mut v, &venues)), err(GovError::UnsupportedCollateral));

    set_phoenix_account(&mut v.env, cross, phoenix_trader(&v.vault, 500 * USDC as i64, &[], 0));
    let cu = process_cu(&mut v, &venues).unwrap();
    println!("process_window with two Phoenix accounts and the 44-market map: {cu} compute units");
    assert!(cu < 200_000, "fits the default budget: {cu}");

    // B's 1,000 bought shares at that value (after the performance fee on the gain).
    let st = v.state();
    let fee_shares: u64 = st.fee_splits.iter().map(|f| f.owed).sum();
    let supply_before = 1_000 * USDC * RATE + fee_shares;
    let e2: vault::VaultEpoch = v.env.account(&vault::epoch_address(&v.vault, 2));
    assert_eq!(e2.deposit_shares as u128, (1_000 * USDC) as u128 * supply_before as u128 / value as u128);
    assert_eq!(v.supply(), supply_before + e2.deposit_shares);
}

// ---------------------------------------------------------------
// A PROP FIRM'S LAUNCH
// ---------------------------------------------------------------

fn distributor_pda(dao: &Pubkey, mint: &Pubkey, operator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::DISTRIBUTOR_SEED, dao.as_ref(), mint.as_ref(), operator.as_ref()], &DIST).0
}
fn distributor_vault(d: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dist::DISTRIBUTOR_VAULT_SEED, d.as_ref()], &DIST).0
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
fn allocation_pda(d: &Pubkey, id: u64) -> Pubkey {
    Pubkey::find_program_address(&[dist::ALLOCATION_SEED, d.as_ref(), &id.to_le_bytes()], &DIST).0
}

#[test]
fn a_prop_firms_raise_pays_into_its_vault_and_the_first_shares_go_to_the_distributor() {
    // The founder picks the raise's address first, so the vault can name it as its seeder.
    let raise_key = Keypair::new();
    let mut v = setup_with(vconfig(), |env| raise_pda(&env.dao, &raise_key.pubkey()));
    let raise = raise_pda(&v.env.dao, &raise_key.pubkey());
    assert_eq!(v.state().seeder, raise);
    let creator = creator_of(&v);
    let operator = funded(&mut v.env);
    let distributor = distributor_pda(&v.env.dao, &v.shares, &operator.pubkey());
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::CreateDistributor {
            payer: creator.pubkey(),
            hub_dao: v.env.dao,
            mint: v.shares,
            distributor,
            vault: distributor_vault(&distributor),
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::CreateDistributor { operator: operator.pubkey(), claim_deadline: START + 400 * DAY }.data(),
    };
    v.env.send(&[ix], &[&creator]).unwrap();

    let terms = dist::RaiseTerms {
        soft_cap: 1_000 * USDC,
        hard_cap: 2_000 * USDC,
        start: START,
        end: START + 7 * DAY,
        min_contribution: 0,
        max_contribution: 0,
        sale_supply: 0,
        vest_cliff: 0,
        vest_duration: 0,
        decision_window: 7 * DAY,
    };
    let ix = Instruction {
        program_id: DIST,
        accounts: dist::accounts::CreateRaise {
            payer: creator.pubkey(),
            create_key: raise_key.pubkey(),
            hub_dao: v.env.dao,
            distributor,
            currency: v.usdc,
            raise,
            vault: raise_vault(&raise),
            prop_firm_vault: Some(v.vault),
            token_program: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dist::instruction::CreateRaise { terms, decider: creator.pubkey() }.data(),
    };
    v.env.send(&[ix], &[&creator, &raise_key]).unwrap();

    // 3,000 contributed past the 2,000 hard cap; the founder accepts 1,500.
    let (a, b) = (depositor(&mut v, 5_000), depositor(&mut v, 5_000));
    for (who, amount) in [(&a, 2_000), (&b, 1_000)] {
        let ix = Instruction {
            program_id: DIST,
            accounts: dist::accounts::Contribute {
                contributor: who.pubkey(),
                raise,
                contribution: contribution_pda(&raise, &who.pubkey()),
                from: ata(&who.pubkey(), &v.usdc),
                vault: raise_vault(&raise),
                currency: v.usdc,
                token_program: TOKEN,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: dist::instruction::Contribute { amount: amount * USDC, account: [0u8; 32] }.data(),
        };
        v.env.send(&[ix], &[who]).unwrap();
    }
    v.env.warp(START + 7 * DAY);
    let accept = |v: &V, with_vault: bool| Instruction {
        program_id: DIST,
        accounts: dist::accounts::Accept {
            decider: creator.pubkey(),
            raise,
            vault: raise_vault(&raise),
            treasury: v.env.treasury,
            treasury_token: None,
            distributor,
            distributor_vault: distributor_vault(&distributor),
            prop_firm_vault: with_vault.then_some(v.vault),
            vault_assets: with_vault.then(|| v.assets()),
            share_mint: with_vault.then_some(v.shares),
            share_token_program: with_vault.then_some(TOKEN),
            vault_program: with_vault.then_some(VAULT),
            currency: v.usdc,
            token_program: TOKEN,
        }
        .to_account_metas(None),
        data: dist::instruction::Accept { amount: 1_500 * USDC }.data(),
    };
    // Without the vault's accounts, nothing goes anywhere.
    assert_eq!(code(v.env.send(&[accept(&v, false)], &[&creator])), err(GovError::WrongPool));
    v.env.send(&[accept(&v, true)], &[&creator]).unwrap();

    // The money is in the vault; the shares it minted are in the Distributor.
    assert_eq!(v.env.token_balance(&v.assets()), 1_500 * USDC);
    assert_eq!(v.env.token_balance(&distributor_vault(&distributor)), 1_500 * USDC * RATE);
    assert_eq!(v.supply(), 1_500 * USDC * RATE);
    let st = v.state();
    assert!(st.seeded && st.seeder == Pubkey::default());
    let r: dist::Raise = v.env.account(&raise);
    assert_eq!(r.terms.sale_supply, 1_500 * USDC * RATE);

    // Contributors' shares are pro rata (2:1), the rest of their money refunded.
    for (i, who) in [&a, &b].into_iter().enumerate() {
        let ix = Instruction {
            program_id: DIST,
            accounts: dist::accounts::Settle {
                payer: creator.pubkey(),
                raise,
                contribution: contribution_pda(&raise, &who.pubkey()),
                distributor,
                distributor_vault: distributor_vault(&distributor),
                allocation: allocation_pda(&distributor, i as u64 + 1),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: dist::instruction::Settle {}.data(),
        };
        v.env.send(&[ix], &[&creator]).unwrap();
    }
    let al: dist::Allocation = v.env.account(&allocation_pda(&distributor, 1));
    let bl: dist::Allocation = v.env.account(&allocation_pda(&distributor, 2));
    assert_eq!((al.total, bl.total), (1_000 * USDC * RATE, 500 * USDC * RATE));
    let c: dist::Contribution = v.env.account(&contribution_pda(&raise, &a.pubkey()));
    assert_eq!(c.refund_owed, 1_000 * USDC);

    // The vault can't be seeded again, and its value is the money: a dollar a share.
    assert_eq!(st.high_water, vault::ONE / RATE as u128);
}
