//! Vortexes prop-firm vault: a prop-firm DAO's fund. Depositors' money is
//! traded through the DAO's desk, and the vault's shares are the DAO's own
//! token: depositing mints it, redeeming burns it. Only DAOs created as prop
//! firms have one, so a DAO never has two tokens.
//!
//! **Windows.** Deposits and redemptions are requested at any time and priced
//! together at the next window (e.g. monthly), at the vault's value then, so
//! traders aren't forced to close positions to pay someone out. Per window
//! (epoch): requests add up; `process_window` values the vault, takes fees,
//! prices redemptions (shares burned, their money owed) and then deposits
//! (shares minted), all at one price. Members claim afterwards.
//!
//! **Value.** Idle money, plus every Phoenix trader account the vault owns
//! (collateral, each position's PnL at Phoenix's mark price, and unsettled
//! funding), plus the vault's other registered token accounts (e.g. Phoenix's
//! collateral token, 1:1). The program reads Phoenix's accounts itself: no
//! trust in the bot. A stale mark price, money in Phoenix's withdraw queue or
//! a move too recent makes the window wait rather than price shares wrong.
//!
//! **Moves.** Money goes to and from Phoenix only through `execute`, which
//! the DAO's treasury signs (a proposal): deposits, withdrawals, delegating a
//! subaccount to a trader's key. Moves stop `freeze_before` ahead of a window
//! and the window waits `settle_delay` after the last move.
//!
//! **Fees and drawdown.** A yearly management fee and a performance fee
//! above the high water mark, taken as shares and split between the
//! trader(s) and the treasury as the DAO configures (each recipient claims
//! theirs). When the share price falls more than `max_drawdown_bps` below the
//! high water mark, `paused` is set - the bot stops signing traders' orders -
//! until a proposal resumes trading.
//!
//! **Launch.** A prop firm's raise pays straight into the vault: the raise
//! (the vault's `seeder`) seeds it, and the first shares go to the
//! Distributor, which shares them among the contributors.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use anchor_spl::token_2022::spl_token_2022::instruction::AuthorityType;
use anchor_spl::token_interface::{self, Burn, Mint, MintTo, SetAuthority, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{GovError, TREASURY_SEED};
use vortex_hub::Dao as HubDao;

declare_id!("GjVM1GkPEzXMRLsrP2FLr9UXJpXDVnwGVsVYbzTvtAU5");

pub const VAULT_SEED: &[u8] = b"vault";
pub const ASSETS_SEED: &[u8] = b"vault_assets";
pub const ESCROW_SEED: &[u8] = b"vault_escrow";
pub const EPOCH_SEED: &[u8] = b"vault_epoch";
pub const REQUEST_SEED: &[u8] = b"vault_request";
pub const ALLOWED_SEED: &[u8] = b"vault_allowed";

pub const MAX_FEE_SPLITS: usize = 4;
pub const MAX_PHOENIX_TRADERS: usize = 8;
pub const MAX_EXTRA_TOKENS: usize = 2;
pub const BPS: u128 = 10_000;
pub const YEAR: u128 = 365 * 24 * 60 * 60;
/// Share prices are asset units per share unit, scaled by this.
pub const ONE: u128 = 1_000_000_000_000_000_000;

/// Phoenix's perps program (the address the bot's desk pins).
pub const PHOENIX_PROGRAM_ID: Pubkey = pubkey!("EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih");

#[program]
pub mod vortex_vault {
    use super::*;

    /// Opens a DAO's vault over `asset_mint`, with the DAO's token as its
    /// shares. Signed by the share mint's authority, which must be the DAO's
    /// creator (at a launch) or its treasury (a proposal); minting passes to
    /// the vault here, so the token only ever comes from deposits. The token
    /// has no supply yet and no freeze authority. `seeder` may seed it once
    /// (a launch's raise); default: nobody.
    pub fn create_vault(ctx: Context<CreateVault>, config: VaultConfig, fee_splits: Vec<FeeSplitTerms>, seeder: Pubkey) -> Result<()> {
        let dao = &ctx.accounts.hub_dao;
        let treasury = Pubkey::create_program_address(&[TREASURY_SEED, dao.key().as_ref(), &[dao.treasury_bump]], &vortex_hub::ID)
            .map_err(|_| error!(GovError::WrongDao))?;
        let creator = ctx.accounts.creator.key();
        require!(creator == dao.creator || creator == treasury, GovError::Unauthorized);
        let shares = &ctx.accounts.share_mint;
        require!(shares.supply == 0 && shares.freeze_authority.is_none(), GovError::InvalidConfig);
        let (sd, ad) = (shares.decimals, ctx.accounts.asset_mint.decimals);
        require!(sd >= ad && sd - ad <= 12, GovError::InvalidConfig);
        check_config(&config)?;

        let now = Clock::get()?.unix_timestamp;
        let v = &mut ctx.accounts.vault;
        v.dao = dao.key();
        v.treasury = treasury;
        v.asset_mint = ctx.accounts.asset_mint.key();
        v.asset_token_program = ctx.accounts.asset_token_program.key();
        v.asset_decimals = ad;
        v.share_mint = shares.key();
        v.share_token_program = ctx.accounts.share_token_program.key();
        v.assets = ctx.accounts.assets.key();
        v.escrow = ctx.accounts.escrow.key();
        v.seeder = seeder;
        v.config = config;
        v.fee_splits = to_splits(&fee_splits)?;
        v.perp_asset_map = Pubkey::default();
        v.phoenix_traders = vec![];
        v.extra_tokens = vec![];
        v.epoch = 1;
        v.next_window_at = now + config.interval;
        v.last_move_at = 0;
        v.last_fee_at = now;
        v.high_water = 0;
        v.paused = false;
        v.seeded = false;
        v.pending_deposit_assets = 0;
        v.owed_assets = 0;
        v.initial_rate = 10u64.pow((sd - ad) as u32);
        v.bump = ctx.bumps.vault;

        token_interface::set_authority(
            CpiContext::new(
                ctx.accounts.share_token_program.key(),
                SetAuthority { current_authority: ctx.accounts.creator.to_account_info(), account_or_mint: ctx.accounts.share_mint.to_account_info() },
            ),
            AuthorityType::MintTokens,
            Some(v.key()),
        )?;
        emit!(VaultCreated { dao: v.dao, vault: v.key(), asset_mint: v.asset_mint, share_mint: v.share_mint, seeder });
        Ok(())
    }

    // ---- the DAO (its treasury signs, through a proposal) ----

    pub fn set_config(ctx: Context<Admin>, config: VaultConfig) -> Result<()> {
        check_config(&config)?;
        ctx.accounts.vault.config = config;
        emit!(ConfigSet { vault: ctx.accounts.vault.key() });
        Ok(())
    }

    /// Replaces who gets the fees, once every recipient has claimed theirs.
    pub fn set_fee_splits(ctx: Context<Admin>, fee_splits: Vec<FeeSplitTerms>) -> Result<()> {
        let v = &mut ctx.accounts.vault;
        require!(v.fee_splits.iter().all(|f| f.owed == 0), GovError::FeesOwed);
        v.fee_splits = to_splits(&fee_splits)?;
        emit!(ConfigSet { vault: v.key() });
        Ok(())
    }

    /// The venue accounts the vault is valued from: Phoenix's PerpAssetMap
    /// and the vault's Phoenix trader accounts (cross and isolated
    /// subaccounts), and other token accounts the vault owns that count 1:1
    /// (e.g. Phoenix's collateral token). The DAO registers each before
    /// moving money into it; a window must see all of them.
    pub fn set_venues(ctx: Context<Admin>, perp_asset_map: Pubkey, phoenix_traders: Vec<Pubkey>, extra_tokens: Vec<Pubkey>) -> Result<()> {
        let v = &mut ctx.accounts.vault;
        require!(phoenix_traders.len() <= MAX_PHOENIX_TRADERS && extra_tokens.len() <= MAX_EXTRA_TOKENS, GovError::InvalidConfig);
        // Phoenix counts in micro-USD, the same unit as a 6-decimal USDC.
        require!(phoenix_traders.is_empty() || (v.asset_decimals == 6 && perp_asset_map != Pubkey::default()), GovError::InvalidConfig);
        for (i, k) in phoenix_traders.iter().chain(extra_tokens.iter()).enumerate() {
            let all: Vec<&Pubkey> = phoenix_traders.iter().chain(extra_tokens.iter()).collect();
            require!(!all[..i].contains(&k) && *k != v.assets && *k != v.escrow, GovError::InvalidConfig);
        }
        v.perp_asset_map = perp_asset_map;
        v.phoenix_traders = phoenix_traders;
        v.extra_tokens = extra_tokens;
        emit!(VenuesSet { vault: v.key(), traders: v.phoenix_traders.len() as u8, extra_tokens: v.extra_tokens.len() as u8 });
        Ok(())
    }

    /// Lets `who` deposit (or stops them) while the vault isn't open to all.
    pub fn set_allowed(ctx: Context<SetAllowed>, who: Pubkey, ok: bool) -> Result<()> {
        let a = &mut ctx.accounts.allowed;
        a.vault = ctx.accounts.vault.key();
        a.who = who;
        a.ok = ok;
        a.bump = ctx.bumps.allowed;
        Ok(())
    }

    /// Pauses trading by proposal, e.g. ahead of a vote on the desk.
    pub fn pause(ctx: Context<Admin>) -> Result<()> {
        let v = &mut ctx.accounts.vault;
        v.paused = true;
        emit!(Paused { vault: v.key(), price: 0, high_water: v.high_water });
        Ok(())
    }

    /// Resumes trading after a drawdown pause.
    pub fn resume(ctx: Context<Admin>) -> Result<()> {
        let v = &mut ctx.accounts.vault;
        v.paused = false;
        emit!(Resumed { vault: v.key() });
        Ok(())
    }

    /// Runs one instruction as the vault: moving money to or from Phoenix,
    /// delegating a subaccount to a trader's key, and so on. The first
    /// remaining account is the program to call, the rest its accounts (the
    /// vault signs where it appears). Never the share mint or the escrow
    /// (members' shares), never this program, and not in the run-up to a
    /// window.
    pub fn execute<'info>(ctx: Context<'info, Execute<'info>>, data: Vec<u8>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let v = &mut ctx.accounts.vault;
        require!(now + v.config.freeze_before < v.next_window_at, GovError::MovesFrozen);
        let (program, accounts) = ctx.remaining_accounts.split_first().ok_or(GovError::InvalidConfig)?;
        require!(program.key() != crate::ID, GovError::Unauthorized);
        let vault_key = v.key();
        let mut metas = Vec::with_capacity(accounts.len());
        for a in accounts {
            require!(a.key() != v.share_mint && a.key() != v.escrow, GovError::Unauthorized);
            let signer = a.is_signer || a.key() == vault_key;
            metas.push(if a.is_writable { AccountMeta::new(a.key(), signer) } else { AccountMeta::new_readonly(a.key(), signer) });
        }
        v.last_move_at = now;
        let ix = Instruction { program_id: program.key(), accounts: metas, data };
        let mut infos = accounts.to_vec();
        infos.push(program.clone());
        infos.push(v.to_account_info());
        let seeds: &[&[u8]] = &[VAULT_SEED, v.dao.as_ref(), &[v.bump]];
        invoke_signed(&ix, &infos, &[seeds])?;
        emit!(Moved { vault: vault_key, program: program.key() });
        Ok(())
    }

    // ---- seeding (a launch) ----

    /// The money already in the vault becomes its first shares, sent to
    /// `receiver` (the Distributor, which shares them among the raise's
    /// contributors). Once, by the seeder, while there are no shares.
    pub fn seed(ctx: Context<Seed>) -> Result<()> {
        let v = &ctx.accounts.vault;
        require!(v.seeder != Pubkey::default() && ctx.accounts.seeder.key() == v.seeder, GovError::Unauthorized);
        require!(!v.seeded && ctx.accounts.share_mint.supply == 0, GovError::AlreadySeeded);
        let assets = ctx.accounts.assets.amount.saturating_sub(v.pending_deposit_assets);
        require!(assets > 0, GovError::InvalidAmount);
        let minted = assets.checked_mul(v.initial_rate).ok_or(GovError::Overflow)?;
        mint_shares(v, &ctx.accounts.share_mint, &ctx.accounts.receiver.to_account_info(), &ctx.accounts.share_token_program, minted)?;
        let v = &mut ctx.accounts.vault;
        v.seeded = true;
        v.seeder = Pubkey::default();
        v.high_water = ONE * assets as u128 / minted as u128;
        emit!(Seeded { vault: v.key(), receiver: ctx.accounts.receiver.key(), assets, shares: minted });
        Ok(())
    }

    // ---- requests ----

    pub fn request_deposit(ctx: Context<RequestDeposit>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let v = &ctx.accounts.vault;
        let user = ctx.accounts.user.key();
        if !v.config.open_to_all {
            let ok = match &ctx.accounts.allowed {
                Some(a) => a.key() == allowed_address(&v.key(), &user) && a.ok,
                None => false,
            };
            require!(ok, GovError::NotAllowed);
        }
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.asset_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.from.to_account_info(),
                    mint: ctx.accounts.asset_mint.to_account_info(),
                    to: ctx.accounts.assets.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.asset_mint.decimals,
        )?;
        let e = v.epoch;
        init_epoch(&mut ctx.accounts.epoch, v.key(), e, ctx.bumps.epoch);
        init_request(&mut ctx.accounts.request, v.key(), e, user, ctx.bumps.request);
        ctx.accounts.request.deposit += amount;
        ctx.accounts.epoch.deposit_assets += amount;
        ctx.accounts.vault.pending_deposit_assets += amount;
        emit!(DepositRequested { vault: ctx.accounts.vault.key(), epoch: e, who: user, assets: amount });
        Ok(())
    }

    pub fn cancel_deposit(ctx: Context<CancelDeposit>) -> Result<()> {
        let amount = std::mem::take(&mut ctx.accounts.request.deposit);
        require!(amount > 0, GovError::NothingToClaim);
        ctx.accounts.epoch.deposit_assets -= amount;
        ctx.accounts.vault.pending_deposit_assets -= amount;
        send_assets(&ctx.accounts.vault, &ctx.accounts.assets, &ctx.accounts.to, &ctx.accounts.asset_mint, &ctx.accounts.asset_token_program, amount)?;
        emit!(DepositCancelled { vault: ctx.accounts.vault.key(), epoch: ctx.accounts.vault.epoch, who: ctx.accounts.user.key(), assets: amount });
        Ok(())
    }

    /// Hands `amount` shares to the vault, to redeem at the next window.
    pub fn request_redeem(ctx: Context<RequestRedeem>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.share_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.from.to_account_info(),
                    mint: ctx.accounts.share_mint.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.share_mint.decimals,
        )?;
        let (vk, e, user) = (ctx.accounts.vault.key(), ctx.accounts.vault.epoch, ctx.accounts.user.key());
        init_epoch(&mut ctx.accounts.epoch, vk, e, ctx.bumps.epoch);
        init_request(&mut ctx.accounts.request, vk, e, user, ctx.bumps.request);
        ctx.accounts.request.redeem += amount;
        ctx.accounts.epoch.redeem_shares += amount;
        emit!(RedeemRequested { vault: vk, epoch: e, who: user, shares: amount });
        Ok(())
    }

    pub fn cancel_redeem(ctx: Context<CancelRedeem>) -> Result<()> {
        let amount = std::mem::take(&mut ctx.accounts.request.redeem);
        require!(amount > 0, GovError::NothingToClaim);
        ctx.accounts.epoch.redeem_shares -= amount;
        send_shares(&ctx.accounts.vault, &ctx.accounts.escrow, &ctx.accounts.to, &ctx.accounts.share_mint, &ctx.accounts.share_token_program, amount)?;
        emit!(RedeemCancelled { vault: ctx.accounts.vault.key(), epoch: ctx.accounts.vault.epoch, who: ctx.accounts.user.key(), shares: amount });
        Ok(())
    }

    // ---- windows ----

    /// Prices the open epoch's requests. Anyone, once the window is due and
    /// nothing has moved for `settle_delay`. Remaining accounts: the venue
    /// accounts in the vault's order (see `set_venues`).
    pub fn process_window<'info>(ctx: Context<'info, ProcessWindow<'info>>) -> Result<()> {
        let clock = Clock::get()?;
        let now = clock.unix_timestamp;
        let v = &ctx.accounts.vault;
        require!(now >= v.next_window_at, GovError::WindowNotDue);
        require!(now >= v.last_move_at + v.config.settle_delay, GovError::Settling);
        let e = v.epoch;
        init_epoch(&mut ctx.accounts.epoch, v.key(), e, ctx.bumps.epoch);

        let mut value = total_value(v, ctx.accounts.assets.amount, ctx.remaining_accounts, clock.slot)? as u128;
        let mut supply = ctx.accounts.share_mint.supply as u128;

        // Fees, as shares in the escrow, owed to each recipient.
        let fee_shares = take_fees(&mut ctx.accounts.vault, value, supply, now)?;
        if fee_shares > 0 {
            let v = &ctx.accounts.vault;
            mint_shares(v, &ctx.accounts.share_mint, &ctx.accounts.escrow.to_account_info(), &ctx.accounts.share_token_program, fee_shares)?;
            supply += fee_shares as u128;
        }

        // Redemptions first, then deposits, at the same price.
        let redeem_shares = ctx.accounts.epoch.redeem_shares;
        let mut redeem_assets = 0u64;
        if redeem_shares > 0 {
            redeem_assets = if supply == 0 { 0 } else { (redeem_shares as u128 * value / supply) as u64 };
            burn_escrow(&ctx.accounts.vault, &ctx.accounts.share_mint, &ctx.accounts.escrow, &ctx.accounts.share_token_program, redeem_shares)?;
            ctx.accounts.vault.owed_assets += redeem_assets;
            value -= redeem_assets as u128;
            supply -= redeem_shares as u128;
            ctx.accounts.epoch.redeem_assets = redeem_assets;
        }

        let deposit_assets = ctx.accounts.epoch.deposit_assets;
        let mut minted = 0u64;
        if deposit_assets > 0 {
            let v = &ctx.accounts.vault;
            minted = if supply == 0 {
                deposit_assets.checked_mul(v.initial_rate).ok_or(GovError::Overflow)?
            } else {
                require!(value > 0, GovError::VaultEmpty);
                u64::try_from(deposit_assets as u128 * supply / value).map_err(|_| GovError::Overflow)?
            };
            mint_shares(v, &ctx.accounts.share_mint, &ctx.accounts.escrow.to_account_info(), &ctx.accounts.share_token_program, minted)?;
            ctx.accounts.vault.pending_deposit_assets -= deposit_assets;
            ctx.accounts.epoch.deposit_shares = minted;
            value += deposit_assets as u128;
            supply += minted as u128;
        }

        let v = &mut ctx.accounts.vault;
        if supply > 0 {
            let price = value * ONE / supply;
            if v.high_water == 0 {
                v.high_water = price;
            }
            if below_limit(v, price) && !v.paused {
                v.paused = true;
                emit!(Paused { vault: v.key(), price, high_water: v.high_water });
            }
        }
        ctx.accounts.epoch.processed = true;
        v.epoch = e + 1;
        let mut next = v.next_window_at;
        while next <= now {
            next += v.config.interval;
        }
        v.next_window_at = next;
        emit!(WindowProcessed {
            vault: v.key(),
            epoch: e,
            value: value as u64,
            supply: supply as u64,
            fee_shares,
            redeemed_shares: redeem_shares,
            redeemed_assets: redeem_assets,
            deposited_assets: deposit_assets,
            minted_shares: minted,
        });
        Ok(())
    }

    /// Anyone (the bot's keeper) can check the drawdown between windows:
    /// below the limit, trading pauses until a proposal resumes it.
    pub fn check_drawdown<'info>(ctx: Context<'info, CheckDrawdown<'info>>) -> Result<()> {
        let clock = Clock::get()?;
        let v = &ctx.accounts.vault;
        let supply = ctx.accounts.share_mint.supply as u128;
        if supply == 0 || v.paused || v.config.max_drawdown_bps == 0 || v.high_water == 0 {
            return Ok(());
        }
        let price = total_value(v, ctx.accounts.assets.amount, ctx.remaining_accounts, clock.slot)? as u128 * ONE / supply;
        let v = &mut ctx.accounts.vault;
        if below_limit(v, price) {
            v.paused = true;
            emit!(Paused { vault: v.key(), price, high_water: v.high_water });
        }
        Ok(())
    }

    // ---- claims ----

    /// Your shares from a processed epoch's deposit.
    pub fn claim_shares(ctx: Context<ClaimShares>) -> Result<()> {
        let ep = &ctx.accounts.epoch;
        require!(ep.processed, GovError::NotProcessed);
        let assets = std::mem::take(&mut ctx.accounts.request.deposit);
        require!(assets > 0, GovError::NothingToClaim);
        let amount = (assets as u128 * ep.deposit_shares as u128 / ep.deposit_assets as u128) as u64;
        send_shares(&ctx.accounts.vault, &ctx.accounts.escrow, &ctx.accounts.to, &ctx.accounts.share_mint, &ctx.accounts.share_token_program, amount)?;
        emit!(SharesClaimed { vault: ctx.accounts.vault.key(), epoch: ep.epoch, who: ctx.accounts.request.owner, shares: amount });
        Ok(())
    }

    /// Your money from a processed epoch's redemption, as far as the vault
    /// holds it idle (the rest once the desk has brought money back).
    /// Anyone, into the owner's own account.
    pub fn claim_redemption(ctx: Context<ClaimRedemption>) -> Result<()> {
        let ep = &ctx.accounts.epoch;
        require!(ep.processed, GovError::NotProcessed);
        let r = &ctx.accounts.request;
        require!(r.redeem > 0, GovError::NothingToClaim);
        let owed = (r.redeem as u128 * ep.redeem_assets as u128 / ep.redeem_shares as u128) as u64 - r.redeem_paid;
        require!(owed > 0, GovError::NothingToClaim);
        let available = ctx.accounts.assets.amount.saturating_sub(ctx.accounts.vault.pending_deposit_assets);
        let amount = owed.min(available);
        require!(amount > 0, GovError::NotLiquid);
        ctx.accounts.request.redeem_paid += amount;
        ctx.accounts.vault.owed_assets -= amount;
        send_assets(&ctx.accounts.vault, &ctx.accounts.assets, &ctx.accounts.to, &ctx.accounts.asset_mint, &ctx.accounts.asset_token_program, amount)?;
        emit!(RedemptionPaid { vault: ctx.accounts.vault.key(), epoch: ep.epoch, who: ctx.accounts.request.owner, assets: amount });
        Ok(())
    }

    /// Pays fee recipient `index` the fee shares owed to it. Anyone, into
    /// the recipient's own account.
    pub fn claim_fees(ctx: Context<ClaimFees>, index: u8) -> Result<()> {
        let v = &mut ctx.accounts.vault;
        let split = v.fee_splits.get_mut(index as usize).ok_or(GovError::InvalidConfig)?;
        require_keys_eq!(ctx.accounts.to.owner, split.recipient, GovError::Unauthorized);
        let amount = std::mem::take(&mut split.owed);
        require!(amount > 0, GovError::NothingToClaim);
        send_shares(&ctx.accounts.vault, &ctx.accounts.escrow, &ctx.accounts.to, &ctx.accounts.share_mint, &ctx.accounts.share_token_program, amount)?;
        emit!(FeesClaimed { vault: ctx.accounts.vault.key(), recipient: ctx.accounts.to.owner, shares: amount });
        Ok(())
    }
}

// ---------------------------------------------------------------
// VALUE
// ---------------------------------------------------------------

/// Byte layout of Phoenix's accounts (phoenix-rise-accounts 0.9.3), read at
/// fixed offsets so the program stays small and off the stack. The vortex
/// tests check every offset against Phoenix's own crate on a mainnet copy of
/// the PerpAssetMap.
pub mod phoenix_layout {
    pub const TRADER_DISCRIMINANT: [u8; 8] = [41, 97, 73, 105, 110, 214, 112, 9];
    pub const PERP_ASSET_MAP_DISCRIMINANT: [u8; 8] = [142, 54, 115, 223, 124, 239, 108, 38];

    pub const TRADER_AUTHORITY: usize = 56;
    pub const TRADER_COLLATERAL: usize = 88;
    pub const TRADER_WITHDRAW_QUEUE_NODE: usize = 108;
    pub const TRADER_POSITIONS_LEN: usize = 224;
    pub const TRADER_POSITIONS_CAPACITY: usize = 232;
    pub const TRADER_ENTRIES: usize = 240;
    pub const POSITION_ENTRY_LEN: usize = 40;
    /// Position-map keys at or above this (low 32 bits) are spot collateral, not positions.
    pub const POSITION_ASSET_INDEX_BOUND: u32 = 0xFF00_0000;
    pub const POS_BASE_LOTS: usize = 8;
    pub const POS_VIRTUAL_QUOTE_LOTS: usize = 16;
    pub const POS_FUNDING_SNAPSHOT: usize = 24;

    pub const MAP_SLOTS_USED: usize = 32;
    pub const MAP_HEADER_LEN: usize = 48;
    pub const MAP_ENTRY_LEN: usize = 1584;
    pub const MAP_LEN: usize = MAP_HEADER_LEN + 1024 * MAP_ENTRY_LEN;
    /// Within an entry (a 16-byte symbol, then the metadata).
    pub const ENTRY_MARK_SLOT: usize = 16 + 16;
    pub const ENTRY_MARK_TICKS: usize = 16 + 24;
    pub const ENTRY_TICK_SIZE: usize = 16 + 936;
    pub const ENTRY_ASSET_ID_LO: usize = 16 + 944;
    pub const ENTRY_ASSET_ID_HI: usize = 16 + 948;
    pub const ENTRY_CUMULATIVE_FUNDING: usize = 16 + 1312;
    pub const ENTRY_TOMBSTONED: usize = 16 + 1450;
    pub const ENTRY_VARIANT: usize = MAP_ENTRY_LEN - 1;
}

fn rd_u64(d: &[u8], at: usize) -> Result<u64> {
    d.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| error!(GovError::VenueMismatch))
}
fn rd_i64(d: &[u8], at: usize) -> Result<i64> {
    rd_u64(d, at).map(|x| x as i64)
}
fn rd_u32(d: &[u8], at: usize) -> Result<u32> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| error!(GovError::VenueMismatch))
}
fn rd_u16(d: &[u8], at: usize) -> Result<u16> {
    d.get(at..at + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| error!(GovError::VenueMismatch))
}

/// One Phoenix market's numbers the value needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhoenixMarket {
    pub asset_id: u32,
    pub tick_size: u64,
    pub mark_ticks: u64,
    pub mark_slot: u64,
    pub cumulative_funding: i64,
}

/// The active perp market with `asset_id` in a PerpAssetMap's bytes.
pub fn phoenix_market(map: &[u8], asset_id: u32) -> Result<Option<PhoenixMarket>> {
    use phoenix_layout::*;
    require!(map.len() >= MAP_LEN && map[..8] == PERP_ASSET_MAP_DISCRIMINANT, GovError::VenueMismatch);
    let used = (rd_u32(map, MAP_SLOTS_USED)? as usize).min(1024);
    for i in 0..used {
        let e = &map[MAP_HEADER_LEN + i * MAP_ENTRY_LEN..MAP_HEADER_LEN + (i + 1) * MAP_ENTRY_LEN];
        if e[ENTRY_VARIANT] != 0 || e[ENTRY_TOMBSTONED] != 0 {
            continue;
        }
        let id = rd_u16(e, ENTRY_ASSET_ID_LO)? as u32 | ((rd_u16(e, ENTRY_ASSET_ID_HI)? as u32) << 16);
        if id == asset_id {
            return Ok(Some(PhoenixMarket {
                asset_id,
                tick_size: rd_u64(e, ENTRY_TICK_SIZE)?,
                mark_ticks: rd_u64(e, ENTRY_MARK_TICKS)?,
                mark_slot: rd_u64(e, ENTRY_MARK_SLOT)?,
                cumulative_funding: rd_i64(e, ENTRY_CUMULATIVE_FUNDING)?,
            }));
        }
    }
    Ok(None)
}

/// A Phoenix trader account's value in quote lots (micro-USD): collateral,
/// plus each position at the mark price, plus unsettled funding - the sum
/// phoenix-rise-math calls portfolio value. Refuses money in the withdraw
/// queue, spot collateral, a market it can't find and a mark price older
/// than `max_age_slots`.
pub fn phoenix_trader_value(trader: &[u8], authority: &Pubkey, map: &[u8], slot: u64, max_age_slots: u64) -> Result<i128> {
    use phoenix_layout::*;
    require!(trader.len() >= TRADER_ENTRIES && trader[..8] == TRADER_DISCRIMINANT, GovError::VenueMismatch);
    require!(trader[TRADER_AUTHORITY..TRADER_AUTHORITY + 32] == authority.to_bytes(), GovError::VenueMismatch);
    require!(rd_u32(trader, TRADER_WITHDRAW_QUEUE_NODE)? == 0, GovError::Settling);
    let mut value = rd_i64(trader, TRADER_COLLATERAL)? as i128;
    let len = rd_u64(trader, TRADER_POSITIONS_LEN)?;
    require!(len <= rd_u64(trader, TRADER_POSITIONS_CAPACITY)?, GovError::VenueMismatch);
    for i in 0..len as usize {
        let at = TRADER_ENTRIES + i * POSITION_ENTRY_LEN;
        let entry = trader.get(at..at + POSITION_ENTRY_LEN).ok_or(GovError::VenueMismatch)?;
        let asset_index = rd_u64(entry, 0)? as u32;
        if asset_index >= POSITION_ASSET_INDEX_BOUND {
            // Spot collateral (SOL, other tokens): the vault only ever deposits USDC.
            require!(entry[8..].iter().all(|b| *b == 0), GovError::UnsupportedCollateral);
            continue;
        }
        let base = rd_i64(entry, POS_BASE_LOTS)? as i128;
        let virtual_quote = rd_i64(entry, POS_VIRTUAL_QUOTE_LOTS)? as i128;
        let snapshot = rd_i64(entry, POS_FUNDING_SNAPSHOT)? as i128;
        if base == 0 && virtual_quote == 0 {
            continue;
        }
        let m = phoenix_market(map, asset_index)?.ok_or(GovError::VenueMismatch)?;
        require!(slot.saturating_sub(m.mark_slot) <= max_age_slots, GovError::StalePrice);
        let pnl = base * m.mark_ticks as i128 * m.tick_size as i128 + virtual_quote;
        let funding = -(m.cumulative_funding as i128 - snapshot) * base;
        value += pnl + funding;
    }
    Ok(value)
}

/// The vault's value now: idle money (less pending deposits and what's owed
/// to redeemers) plus every venue account, in the vault's order.
fn total_value(v: &Vault, idle: u64, accounts: &[AccountInfo], slot: u64) -> Result<u64> {
    let reserved = v.pending_deposit_assets + v.owed_assets;
    let mut value = idle.saturating_sub(reserved) as i128;
    let mut i = 0;
    let vault_key = vault_address(&v.dao);
    if !v.phoenix_traders.is_empty() {
        let map = accounts.first().ok_or(GovError::VenueMismatch)?;
        require!(map.key() == v.perp_asset_map && *map.owner == PHOENIX_PROGRAM_ID, GovError::VenueMismatch);
        let map_data = map.try_borrow_data()?;
        i = 1;
        for t in &v.phoenix_traders {
            let a = accounts.get(i).ok_or(GovError::VenueMismatch)?;
            require!(a.key() == *t && *a.owner == PHOENIX_PROGRAM_ID, GovError::VenueMismatch);
            value += phoenix_trader_value(&a.try_borrow_data()?, &vault_key, &map_data, slot, v.config.max_price_age_slots)?;
            i += 1;
        }
    }
    for t in &v.extra_tokens {
        let a = accounts.get(i).ok_or(GovError::VenueMismatch)?;
        require!(a.key() == *t, GovError::VenueMismatch);
        require!(*a.owner == anchor_spl::token::ID || *a.owner == anchor_spl::token_2022::ID, GovError::VenueMismatch);
        let acc = TokenAccount::try_deserialize_unchecked(&mut &a.try_borrow_data()?[..])?;
        require_keys_eq!(acc.owner, vault_key, GovError::VenueMismatch);
        value += acc.amount as i128;
        i += 1;
    }
    Ok(value.clamp(0, u64::MAX as i128) as u64)
}

fn below_limit(v: &Vault, price: u128) -> bool {
    v.config.max_drawdown_bps != 0 && price < v.high_water * (BPS - v.config.max_drawdown_bps as u128) / BPS
}

/// Management fee for the time since the last window, and the performance
/// fee above the high water mark, as shares whose holders own exactly the
/// fee's value; credited to each recipient's `owed`.
fn take_fees(v: &mut Vault, value: u128, supply: u128, now: i64) -> Result<u64> {
    let elapsed = (now - v.last_fee_at).max(0) as u128;
    v.last_fee_at = now;
    if supply == 0 || value == 0 || v.fee_splits.is_empty() {
        return Ok(0);
    }
    let mut fee = value * v.config.management_bps as u128 * elapsed / (BPS * YEAR);
    let price = value.saturating_sub(fee) * ONE / supply;
    if price > v.high_water && v.high_water != 0 {
        fee += (price - v.high_water) * supply * v.config.performance_bps as u128 / (ONE * BPS);
    }
    if fee == 0 || fee >= value {
        if price > v.high_water {
            v.high_water = price;
        }
        return Ok(0);
    }
    let fee_shares = u64::try_from(fee * supply / (value - fee)).map_err(|_| GovError::Overflow)?;
    let n = v.fee_splits.len();
    let mut left = fee_shares;
    for (i, s) in v.fee_splits.iter_mut().enumerate() {
        let part = if i == n - 1 { left } else { (fee_shares as u128 * s.bps as u128 / BPS) as u64 };
        left -= part;
        s.owed += part;
    }
    let after = value * ONE / (supply + fee_shares as u128);
    if after > v.high_water {
        v.high_water = after;
    }
    Ok(fee_shares)
}

// ---------------------------------------------------------------
// HELPERS
// ---------------------------------------------------------------

pub fn vault_address(dao: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[VAULT_SEED, dao.as_ref()], &crate::ID).0
}
pub fn assets_address(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ASSETS_SEED, vault.as_ref()], &crate::ID).0
}
pub fn escrow_address(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_SEED, vault.as_ref()], &crate::ID).0
}
pub fn epoch_address(vault: &Pubkey, epoch: u64) -> Pubkey {
    Pubkey::find_program_address(&[EPOCH_SEED, vault.as_ref(), &epoch.to_le_bytes()], &crate::ID).0
}
pub fn request_address(vault: &Pubkey, epoch: u64, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[REQUEST_SEED, vault.as_ref(), &epoch.to_le_bytes(), owner.as_ref()], &crate::ID).0
}
pub fn allowed_address(vault: &Pubkey, who: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ALLOWED_SEED, vault.as_ref(), who.as_ref()], &crate::ID).0
}

fn check_config(c: &VaultConfig) -> Result<()> {
    require!(c.interval > 0 && c.freeze_before >= 0 && c.freeze_before < c.interval && c.settle_delay >= 0, GovError::InvalidConfig);
    require!(c.management_bps <= 1_000 && c.performance_bps <= 5_000 && c.max_drawdown_bps as u128 <= BPS, GovError::InvalidConfig);
    require!(c.max_price_age_slots > 0, GovError::InvalidConfig);
    Ok(())
}

fn to_splits(terms: &[FeeSplitTerms]) -> Result<Vec<FeeSplit>> {
    require!(terms.len() <= MAX_FEE_SPLITS, GovError::InvalidConfig);
    let total: u32 = terms.iter().map(|t| t.bps as u32).sum();
    require!(terms.is_empty() || total == BPS as u32, GovError::InvalidConfig);
    require!(terms.iter().all(|t| t.recipient != Pubkey::default()), GovError::InvalidConfig);
    Ok(terms.iter().map(|t| FeeSplit { recipient: t.recipient, bps: t.bps, owed: 0 }).collect())
}

fn init_epoch(ep: &mut Account<VaultEpoch>, vault: Pubkey, epoch: u64, bump: u8) {
    if ep.vault == Pubkey::default() {
        ep.vault = vault;
        ep.epoch = epoch;
        ep.bump = bump;
    }
}

fn init_request(r: &mut Account<VaultRequest>, vault: Pubkey, epoch: u64, owner: Pubkey, bump: u8) {
    if r.vault == Pubkey::default() {
        r.vault = vault;
        r.epoch = epoch;
        r.owner = owner;
        r.bump = bump;
    }
}

fn mint_shares<'info>(v: &Account<'info, Vault>, mint: &InterfaceAccount<'info, Mint>, to: &AccountInfo<'info>, program: &Interface<'info, TokenInterface>, amount: u64) -> Result<()> {
    let seeds: &[&[u8]] = &[VAULT_SEED, v.dao.as_ref(), &[v.bump]];
    token_interface::mint_to(
        CpiContext::new_with_signer(program.key(), MintTo { mint: mint.to_account_info(), to: to.clone(), authority: v.to_account_info() }, &[seeds]),
        amount,
    )
}

fn burn_escrow<'info>(v: &Account<'info, Vault>, mint: &InterfaceAccount<'info, Mint>, escrow: &InterfaceAccount<'info, TokenAccount>, program: &Interface<'info, TokenInterface>, amount: u64) -> Result<()> {
    let seeds: &[&[u8]] = &[VAULT_SEED, v.dao.as_ref(), &[v.bump]];
    token_interface::burn(
        CpiContext::new_with_signer(program.key(), Burn { mint: mint.to_account_info(), from: escrow.to_account_info(), authority: v.to_account_info() }, &[seeds]),
        amount,
    )
}

fn send_shares<'info>(v: &Account<'info, Vault>, escrow: &InterfaceAccount<'info, TokenAccount>, to: &InterfaceAccount<'info, TokenAccount>, mint: &InterfaceAccount<'info, Mint>, program: &Interface<'info, TokenInterface>, amount: u64) -> Result<()> {
    send(v, escrow, to, mint, program, amount)
}

fn send_assets<'info>(v: &Account<'info, Vault>, assets: &InterfaceAccount<'info, TokenAccount>, to: &InterfaceAccount<'info, TokenAccount>, mint: &InterfaceAccount<'info, Mint>, program: &Interface<'info, TokenInterface>, amount: u64) -> Result<()> {
    send(v, assets, to, mint, program, amount)
}

fn send<'info>(v: &Account<'info, Vault>, from: &InterfaceAccount<'info, TokenAccount>, to: &InterfaceAccount<'info, TokenAccount>, mint: &InterfaceAccount<'info, Mint>, program: &Interface<'info, TokenInterface>, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let seeds: &[&[u8]] = &[VAULT_SEED, v.dao.as_ref(), &[v.bump]];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            program.key(),
            TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: v.to_account_info() },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct VaultConfig {
    /// Seconds between windows, e.g. 30 days.
    pub interval: i64,
    /// No moves this long before a window.
    pub freeze_before: i64,
    /// A window waits this long after the last move.
    pub settle_delay: i64,
    /// Per year, of the vault's value.
    pub management_bps: u16,
    /// Of gains above the high water mark.
    pub performance_bps: u16,
    /// Below the high water mark that pauses trading; 0: off.
    pub max_drawdown_bps: u16,
    /// False: only addresses the DAO allowed may deposit.
    pub open_to_all: bool,
    /// The oldest Phoenix mark price a valuation accepts, in slots.
    pub max_price_age_slots: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct FeeSplitTerms {
    pub recipient: Pubkey,
    /// Of each fee; the splits add up to 10,000.
    pub bps: u16,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct FeeSplit {
    pub recipient: Pubkey,
    pub bps: u16,
    /// Fee shares (in the escrow) not yet claimed.
    pub owed: u64,
}

/// `[VAULT_SEED, dao]`.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    pub dao: Pubkey,
    pub treasury: Pubkey,
    pub asset_mint: Pubkey,
    pub asset_token_program: Pubkey,
    pub asset_decimals: u8,
    /// The DAO's token; the vault is its only minter.
    pub share_mint: Pubkey,
    pub share_token_program: Pubkey,
    /// Idle money: `[ASSETS_SEED, vault]`.
    pub assets: Pubkey,
    /// Shares waiting to be redeemed or claimed, and fee shares owed: `[ESCROW_SEED, vault]`.
    pub escrow: Pubkey,
    /// May seed the vault once (a launch's raise); default: nobody.
    pub seeder: Pubkey,
    pub config: VaultConfig,
    #[max_len(MAX_FEE_SPLITS)]
    pub fee_splits: Vec<FeeSplit>,
    pub perp_asset_map: Pubkey,
    #[max_len(MAX_PHOENIX_TRADERS)]
    pub phoenix_traders: Vec<Pubkey>,
    #[max_len(MAX_EXTRA_TOKENS)]
    pub extra_tokens: Vec<Pubkey>,
    /// The open epoch: requests made now are priced at its window.
    pub epoch: u64,
    pub next_window_at: i64,
    pub last_move_at: i64,
    pub last_fee_at: i64,
    /// The best share price after fees so far (asset units per share unit, times ONE).
    pub high_water: u128,
    pub paused: bool,
    pub seeded: bool,
    /// In the open epoch.
    pub pending_deposit_assets: u64,
    /// Redemptions priced, not yet paid.
    pub owed_assets: u64,
    /// Share units per asset unit for the first deposit (one share per asset).
    pub initial_rate: u64,
    pub bump: u8,
}

/// `[EPOCH_SEED, vault, epoch]`.
#[account]
#[derive(InitSpace)]
pub struct VaultEpoch {
    pub vault: Pubkey,
    pub epoch: u64,
    pub deposit_assets: u64,
    pub deposit_shares: u64,
    pub redeem_shares: u64,
    pub redeem_assets: u64,
    pub processed: bool,
    pub bump: u8,
}

/// `[REQUEST_SEED, vault, epoch, owner]`.
#[account]
#[derive(InitSpace)]
pub struct VaultRequest {
    pub vault: Pubkey,
    pub epoch: u64,
    pub owner: Pubkey,
    pub deposit: u64,
    pub redeem: u64,
    pub redeem_paid: u64,
    pub bump: u8,
}

/// `[ALLOWED_SEED, vault, who]`.
#[account]
#[derive(InitSpace)]
pub struct Allowed {
    pub vault: Pubkey,
    pub who: Pubkey,
    pub ok: bool,
    pub bump: u8,
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
pub struct CreateVault<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The share mint's authority: the DAO's creator or its treasury.
    pub creator: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(init, payer = payer, space = 8 + Vault::INIT_SPACE, seeds = [VAULT_SEED, hub_dao.key().as_ref()], bump)]
    pub vault: Account<'info, Vault>,
    #[account(mut, mint::token_program = share_token_program)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(mint::token_program = asset_token_program)]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init, payer = payer, seeds = [ASSETS_SEED, vault.key().as_ref()], bump,
        token::mint = asset_mint, token::authority = vault, token::token_program = asset_token_program,
    )]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = payer, seeds = [ESCROW_SEED, vault.key().as_ref()], bump,
        token::mint = share_mint, token::authority = vault, token::token_program = share_token_program,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub asset_token_program: Interface<'info, TokenInterface>,
    pub share_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Admin<'info> {
    #[account(seeds = [TREASURY_SEED, vault.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
}

#[derive(Accounts)]
#[instruction(who: Pubkey)]
pub struct SetAllowed<'info> {
    #[account(mut, seeds = [TREASURY_SEED, vault.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    pub vault: Account<'info, Vault>,
    #[account(
        init_if_needed, payer = treasury, space = 8 + Allowed::INIT_SPACE,
        seeds = [ALLOWED_SEED, vault.key().as_ref(), who.as_ref()], bump,
    )]
    pub allowed: Account<'info, Allowed>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Execute<'info> {
    #[account(seeds = [TREASURY_SEED, vault.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
}

#[derive(Accounts)]
pub struct Seed<'info> {
    pub seeder: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = share_mint, token::token_program = share_token_program)]
    pub receiver: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct RequestDeposit<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(
        init_if_needed, payer = user, space = 8 + VaultEpoch::INIT_SPACE,
        seeds = [EPOCH_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes()], bump,
    )]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(
        init_if_needed, payer = user, space = 8 + VaultRequest::INIT_SPACE,
        seeds = [REQUEST_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes(), user.key().as_ref()], bump,
    )]
    pub request: Account<'info, VaultRequest>,
    /// Needed when the vault isn't open to all: `[ALLOWED_SEED, vault, user]`.
    pub allowed: Option<Account<'info, Allowed>>,
    #[account(mut, token::mint = asset_mint, token::authority = user, token::token_program = asset_token_program)]
    pub from: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.asset_mint @ GovError::WrongMint)]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.asset_token_program @ GovError::WrongPool)]
    pub asset_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelDeposit<'info> {
    pub user: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(mut, seeds = [EPOCH_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes()], bump = epoch.bump)]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(mut, seeds = [REQUEST_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes(), user.key().as_ref()], bump = request.bump)]
    pub request: Account<'info, VaultRequest>,
    #[account(mut, token::mint = asset_mint, token::authority = user, token::token_program = asset_token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.asset_mint @ GovError::WrongMint)]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.asset_token_program @ GovError::WrongPool)]
    pub asset_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct RequestRedeem<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    pub vault: Account<'info, Vault>,
    #[account(
        init_if_needed, payer = user, space = 8 + VaultEpoch::INIT_SPACE,
        seeds = [EPOCH_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes()], bump,
    )]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(
        init_if_needed, payer = user, space = 8 + VaultRequest::INIT_SPACE,
        seeds = [REQUEST_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes(), user.key().as_ref()], bump,
    )]
    pub request: Account<'info, VaultRequest>,
    #[account(mut, token::mint = share_mint, token::authority = user, token::token_program = share_token_program)]
    pub from: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.escrow @ GovError::WrongPool)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelRedeem<'info> {
    pub user: Signer<'info>,
    pub vault: Account<'info, Vault>,
    #[account(mut, seeds = [EPOCH_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes()], bump = epoch.bump)]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(mut, seeds = [REQUEST_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes(), user.key().as_ref()], bump = request.bump)]
    pub request: Account<'info, VaultRequest>,
    #[account(mut, token::mint = share_mint, token::authority = user, token::token_program = share_token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.escrow @ GovError::WrongPool)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ProcessWindow<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(
        init_if_needed, payer = payer, space = 8 + VaultEpoch::INIT_SPACE,
        seeds = [EPOCH_SEED, vault.key().as_ref(), &vault.epoch.to_le_bytes()], bump,
    )]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.escrow @ GovError::WrongPool)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CheckDrawdown<'info> {
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
}

#[derive(Accounts)]
pub struct ClaimShares<'info> {
    pub vault: Account<'info, Vault>,
    #[account(has_one = vault @ GovError::WrongPool)]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(mut, has_one = vault @ GovError::WrongPool, constraint = request.epoch == epoch.epoch @ GovError::WrongPool)]
    pub request: Account<'info, VaultRequest>,
    #[account(mut, token::mint = share_mint, token::authority = request.owner, token::token_program = share_token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.escrow @ GovError::WrongPool)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimRedemption<'info> {
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(has_one = vault @ GovError::WrongPool)]
    pub epoch: Account<'info, VaultEpoch>,
    #[account(mut, has_one = vault @ GovError::WrongPool, constraint = request.epoch == epoch.epoch @ GovError::WrongPool)]
    pub request: Account<'info, VaultRequest>,
    #[account(mut, token::mint = asset_mint, token::authority = request.owner, token::token_program = asset_token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.assets @ GovError::WrongPool)]
    pub assets: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.asset_mint @ GovError::WrongMint)]
    pub asset_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.asset_token_program @ GovError::WrongPool)]
    pub asset_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    #[account(mut)]
    pub vault: Account<'info, Vault>,
    #[account(mut, token::mint = share_mint, token::token_program = share_token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = vault.escrow @ GovError::WrongPool)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    #[account(address = vault.share_mint @ GovError::WrongMint)]
    pub share_mint: InterfaceAccount<'info, Mint>,
    #[account(address = vault.share_token_program @ GovError::WrongPool)]
    pub share_token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct VaultCreated {
    pub dao: Pubkey,
    pub vault: Pubkey,
    pub asset_mint: Pubkey,
    pub share_mint: Pubkey,
    pub seeder: Pubkey,
}

#[event]
pub struct ConfigSet {
    pub vault: Pubkey,
}

#[event]
pub struct VenuesSet {
    pub vault: Pubkey,
    pub traders: u8,
    pub extra_tokens: u8,
}

#[event]
pub struct Paused {
    pub vault: Pubkey,
    pub price: u128,
    pub high_water: u128,
}

#[event]
pub struct Resumed {
    pub vault: Pubkey,
}

#[event]
pub struct Moved {
    pub vault: Pubkey,
    pub program: Pubkey,
}

#[event]
pub struct Seeded {
    pub vault: Pubkey,
    pub receiver: Pubkey,
    pub assets: u64,
    pub shares: u64,
}

#[event]
pub struct DepositRequested {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub assets: u64,
}

#[event]
pub struct DepositCancelled {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub assets: u64,
}

#[event]
pub struct RedeemRequested {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub shares: u64,
}

#[event]
pub struct RedeemCancelled {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub shares: u64,
}

#[event]
pub struct WindowProcessed {
    pub vault: Pubkey,
    pub epoch: u64,
    pub value: u64,
    pub supply: u64,
    pub fee_shares: u64,
    pub redeemed_shares: u64,
    pub redeemed_assets: u64,
    pub deposited_assets: u64,
    pub minted_shares: u64,
}

#[event]
pub struct SharesClaimed {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub shares: u64,
}

#[event]
pub struct RedemptionPaid {
    pub vault: Pubkey,
    pub epoch: u64,
    pub who: Pubkey,
    pub assets: u64,
}

#[event]
pub struct FeesClaimed {
    pub vault: Pubkey,
    pub recipient: Pubkey,
    pub shares: u64,
}
