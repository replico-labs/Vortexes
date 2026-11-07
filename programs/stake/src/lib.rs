//! Vortexes stake: where a DAO's members stake its token.
//!
//! One **pool** per DAO per token (`[POOL_SEED, dao, mint]`). The hub's
//! token record names the DAO's current pool; token-based governance reads
//! each member's voting power from it, so stake stays put through every
//! model switch, and no governance program holds stake of its own.
//!
//! - **Stake and withdraw.** Members stake into the current pool only, and
//!   withdraw from any pool, current or not, whatever is unlocked. When the
//!   DAO changes its token the old pool simply stops taking stake.
//! - **Locks need the member and the DAO's current governance.** Both sign
//!   (the governance through its account's PDA, in a CPI from an
//!   instruction the member signed: a vote, support, joining a draw pool):
//!   - `lock_until` holds a member's whole stake until a time (the end of a
//!     vote they cast), as voting with Solana tokens needs: no balance
//!     history to snapshot, so voted tokens mustn't move until voting ends.
//!   - `lock` holds an amount for that governance (`[LOCK_SEED, stake,
//!     governance]`): conviction support, a sortition draw pool, a liquid
//!     delegation. Only the governance that locked it can `unlock` it, even
//!     after the DAO has switched away, so a replaced governance can always
//!     let members out.
//! - **Converters** move stake from an old pool to the current one at a
//!   rate the DAO voted (new tokens per old), out of tokens the treasury put
//!   in. The old tokens go to the treasury. Only a passed proposal (the
//!   treasury) opens or closes one.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{GovError, POOL_SEED, STAKE_SEED};
use vortex_hub::{Dao as HubDao, DaoToken};

declare_id!("7T27h8r13Xmp1m4vQbHFfs1xTqdQaHnxW7ybHvwjqKAd");

pub const VAULT_SEED: &[u8] = b"vault";
pub const LOCK_SEED: &[u8] = b"lock";
pub const CONVERTER_SEED: &[u8] = b"converter";
pub const CONVERTER_VAULT_SEED: &[u8] = b"converter_vault";

#[program]
pub mod vortex_stake {
    use super::*;

    /// Opens a DAO's pool for `mint`. Anyone can: a pool only matters once
    /// the hub's token record points at it.
    pub fn create_pool(ctx: Context<CreatePool>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.dao = ctx.accounts.hub_dao.key();
        pool.mint = ctx.accounts.mint.key();
        pool.vault = ctx.accounts.vault.key();
        pool.token_program = ctx.accounts.token_program.key();
        pool.total = 0;
        pool.bump = ctx.bumps.pool;
        emit!(PoolCreated { dao: pool.dao, pool: pool.key(), mint: pool.mint });
        Ok(())
    }

    /// Stakes `amount` into the DAO's current pool.
    pub fn stake(ctx: Context<StakeTokens>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        require_keys_eq!(ctx.accounts.dao_token.pool, ctx.accounts.pool.key(), GovError::PoolNotCurrent);
        transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.owner_token_account,
            &ctx.accounts.mint,
            &ctx.accounts.vault,
            ctx.accounts.owner.to_account_info(),
            &[],
            amount,
        )?;
        let s = &mut ctx.accounts.stake;
        if s.owner == Pubkey::default() {
            s.pool = ctx.accounts.pool.key();
            s.owner = ctx.accounts.owner.key();
            s.bump = ctx.bumps.stake;
        }
        s.amount = s.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        let pool = &mut ctx.accounts.pool;
        pool.total = pool.total.checked_add(amount).ok_or(GovError::Overflow)?;
        emit!(Staked { pool: pool.key(), owner: s.owner, amount, total: s.amount });
        Ok(())
    }

    /// Takes `amount` back out of any of the DAO's pools: whatever isn't
    /// locked, once every vote it was locked for has ended.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let s = &mut ctx.accounts.stake;
        s.take(amount, Clock::get()?.unix_timestamp)?;
        let pool = &mut ctx.accounts.pool;
        pool.total -= amount;
        let seeds: &[&[u8]] = &[POOL_SEED, pool.dao.as_ref(), pool.mint.as_ref(), &[pool.bump]];
        transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            &ctx.accounts.mint,
            &ctx.accounts.owner_token_account,
            pool.to_account_info(),
            &[seeds],
            amount,
        )?;
        emit!(Withdrawn { pool: pool.key(), owner: s.owner, amount, total: s.amount });
        Ok(())
    }

    /// Holds the member's whole stake until `until` (the end of a vote they
    /// cast). Signed by the member and the DAO's current governance.
    pub fn lock_until(ctx: Context<LockUntil>, until: i64) -> Result<()> {
        let s = &mut ctx.accounts.stake;
        s.locked_until = s.locked_until.max(until);
        emit!(LockedUntil { stake: s.key(), governance: ctx.accounts.governance.key(), until: s.locked_until });
        Ok(())
    }

    /// Locks `amount` of the member's stake for the DAO's current
    /// governance, which signs, with the member.
    pub fn lock(ctx: Context<Lock>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let s = &mut ctx.accounts.stake;
        require!(amount <= s.amount - s.locked, GovError::InsufficientDeposit);
        s.locked += amount;
        let l = &mut ctx.accounts.lock;
        if l.locker == Pubkey::default() {
            l.stake = s.key();
            l.locker = ctx.accounts.governance.key();
            l.bump = ctx.bumps.lock;
        }
        l.amount = l.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        emit!(Locked { stake: s.key(), locker: l.locker, amount, locked: l.amount });
        Ok(())
    }

    /// Releases `amount` the signing governance locked earlier. Works after
    /// the DAO has switched away from it.
    pub fn unlock(ctx: Context<Unlock>, amount: u64) -> Result<()> {
        let l = &mut ctx.accounts.lock;
        require!(amount <= l.amount, GovError::InsufficientDeposit);
        l.amount -= amount;
        let s = &mut ctx.accounts.stake;
        s.locked -= amount;
        emit!(Unlocked { stake: s.key(), locker: l.locker, amount, locked: l.amount });
        Ok(())
    }

    /// Opens a converter from an old pool into the DAO's current one, at
    /// `rate_num / rate_den` new tokens per old, and moves `fund` new tokens
    /// from the treasury into its vault (0 to fund it later by a plain
    /// transfer). Only a passed proposal (the treasury signs and pays rent).
    /// Funding here keeps a token change with its converter small enough
    /// for one proposal transaction.
    pub fn open_converter(ctx: Context<OpenConverter>, rate_num: u64, rate_den: u64, fund: u64) -> Result<()> {
        require!(rate_num > 0 && rate_den > 0, GovError::InvalidConfig);
        if fund > 0 {
            let from = ctx.accounts.treasury_account.as_ref().ok_or(GovError::InvalidAmount)?;
            transfer(
                &ctx.accounts.token_program,
                from,
                &ctx.accounts.mint,
                &ctx.accounts.converter_vault,
                ctx.accounts.treasury.to_account_info(),
                &[],
                fund,
            )?;
        }
        require_keys_neq!(ctx.accounts.from_pool.key(), ctx.accounts.to_pool.key(), GovError::InvalidConfig);
        let c = &mut ctx.accounts.converter;
        c.dao = ctx.accounts.hub_dao.key();
        c.from_pool = ctx.accounts.from_pool.key();
        c.to_pool = ctx.accounts.to_pool.key();
        c.vault = ctx.accounts.converter_vault.key();
        c.rate_num = rate_num;
        c.rate_den = rate_den;
        c.open = true;
        c.bump = ctx.bumps.converter;
        emit!(ConverterOpened { dao: c.dao, converter: c.key(), from_pool: c.from_pool, to_pool: c.to_pool, rate_num, rate_den });
        Ok(())
    }

    /// Converts `amount` of the member's unlocked stake in the old pool into
    /// stake in the current one, at the converter's rate. The old tokens go
    /// to the treasury; the new ones come out of the converter.
    pub fn convert(ctx: Context<Convert>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let c = &ctx.accounts.converter;
        require!(c.open, GovError::ConverterClosed);
        let out = u64::try_from(amount as u128 * c.rate_num as u128 / c.rate_den as u128).map_err(|_| GovError::Overflow)?;
        require!(out > 0, GovError::InvalidAmount);
        require!(out <= ctx.accounts.converter_vault.amount, GovError::ConverterUnderfunded);

        let from = &mut ctx.accounts.from_stake;
        from.take(amount, Clock::get()?.unix_timestamp)?;
        let from_pool = &mut ctx.accounts.from_pool;
        from_pool.total -= amount;
        let from_seeds: &[&[u8]] = &[POOL_SEED, from_pool.dao.as_ref(), from_pool.mint.as_ref(), &[from_pool.bump]];
        transfer(
            &ctx.accounts.from_token_program,
            &ctx.accounts.from_vault,
            &ctx.accounts.from_mint,
            &ctx.accounts.treasury_from_account,
            from_pool.to_account_info(),
            &[from_seeds],
            amount,
        )?;

        let c = &ctx.accounts.converter;
        let converter_seeds: &[&[u8]] = &[CONVERTER_SEED, c.from_pool.as_ref(), c.to_pool.as_ref(), &[c.bump]];
        transfer(
            &ctx.accounts.to_token_program,
            &ctx.accounts.converter_vault,
            &ctx.accounts.to_mint,
            &ctx.accounts.to_vault,
            c.to_account_info(),
            &[converter_seeds],
            out,
        )?;
        let to = &mut ctx.accounts.to_stake;
        if to.owner == Pubkey::default() {
            to.pool = ctx.accounts.to_pool.key();
            to.owner = ctx.accounts.owner.key();
            to.bump = ctx.bumps.to_stake;
        }
        to.amount = to.amount.checked_add(out).ok_or(GovError::Overflow)?;
        let to_pool = &mut ctx.accounts.to_pool;
        to_pool.total = to_pool.total.checked_add(out).ok_or(GovError::Overflow)?;
        emit!(Converted { converter: c.key(), owner: to.owner, amount_in: amount, amount_out: out });
        Ok(())
    }

    /// Closes a converter and returns what's left in it to the treasury.
    /// Only a passed proposal.
    pub fn close_converter(ctx: Context<CloseConverter>) -> Result<()> {
        let c = &mut ctx.accounts.converter;
        require!(c.open, GovError::ConverterClosed);
        c.open = false;
        let left = ctx.accounts.converter_vault.amount;
        if left > 0 {
            let seeds: &[&[u8]] = &[CONVERTER_SEED, c.from_pool.as_ref(), c.to_pool.as_ref(), &[c.bump]];
            transfer(
                &ctx.accounts.token_program,
                &ctx.accounts.converter_vault,
                &ctx.accounts.mint,
                &ctx.accounts.treasury_account,
                c.to_account_info(),
                &[seeds],
                left,
            )?;
        }
        emit!(ConverterClosed { converter: c.key(), returned: left });
        Ok(())
    }
}

/// For governance programs: holds `stake` until `until`, signed by the
/// member (`owner`, who signed the governance's instruction) and the
/// governance account (`governance_seeds`, its PDA seeds with bump).
#[cfg(feature = "cpi")]
#[allow(clippy::too_many_arguments)]
pub fn hold_until<'info>(
    stake_program: Pubkey,
    owner: AccountInfo<'info>,
    governance: AccountInfo<'info>,
    hub_dao: AccountInfo<'info>,
    pool: AccountInfo<'info>,
    stake: AccountInfo<'info>,
    governance_seeds: &[&[u8]],
    until: i64,
) -> Result<()> {
    cpi::lock_until(
        CpiContext::new_with_signer(stake_program, cpi::accounts::LockUntil { owner, governance, hub_dao, pool, stake }, &[governance_seeds]),
        until,
    )
}

fn transfer<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &InterfaceAccount<'info, TokenAccount>,
    authority: AccountInfo<'info>,
    signer: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority },
            signer,
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

/// A DAO's pool for one token: `[POOL_SEED, dao, mint]`.
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub dao: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub token_program: Pubkey,
    /// Everything staked here (raw tokens).
    pub total: u64,
    pub bump: u8,
}

/// One member's stake in a pool: `[STAKE_SEED, pool, owner]`.
#[account]
#[derive(InitSpace)]
pub struct Stake {
    pub pool: Pubkey,
    pub owner: Pubkey,
    /// Staked (raw tokens).
    pub amount: u64,
    /// Held by `Lock`s, one per governance that locked some.
    pub locked: u64,
    /// The whole stake stays until this time (the end of the latest vote
    /// it was cast in).
    pub locked_until: i64,
    pub bump: u8,
}

impl Stake {
    /// Takes `amount` out: only what isn't locked, and only once votes it
    /// was cast in have ended.
    pub fn take(&mut self, amount: u64, now: i64) -> Result<()> {
        require!(now >= self.locked_until, GovError::TokensLocked);
        require!(amount <= self.amount - self.locked, GovError::InsufficientDeposit);
        self.amount -= amount;
        Ok(())
    }
}

/// What one governance holds of one stake: `[LOCK_SEED, stake, governance]`.
#[account]
#[derive(InitSpace)]
pub struct LockRecord {
    pub stake: Pubkey,
    /// The governance account that locked it, the only one that can unlock it.
    pub locker: Pubkey,
    pub amount: u64,
    pub bump: u8,
}

/// Moves stake from an old pool to the current one:
/// `[CONVERTER_SEED, from_pool, to_pool]`.
#[account]
#[derive(InitSpace)]
pub struct Converter {
    pub dao: Pubkey,
    pub from_pool: Pubkey,
    pub to_pool: Pubkey,
    /// Holds the new token it pays out.
    pub vault: Pubkey,
    /// New tokens per old: `rate_num / rate_den`.
    pub rate_num: u64,
    pub rate_den: u64,
    pub open: bool,
    pub bump: u8,
}

// ---------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(init, payer = payer, space = 8 + Pool::INIT_SPACE, seeds = [POOL_SEED, hub_dao.key().as_ref(), mint.key().as_ref()], bump)]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, pool.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = pool,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct StakeTokens<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = mint @ GovError::WrongMint, has_one = vault @ GovError::WrongPool, has_one = token_program @ GovError::WrongPool)]
    pub pool: Account<'info, Pool>,
    /// The hub's token record for the pool's DAO: names the current pool.
    #[account(seeds = [vortex_hub::TOKEN_SEED, pool.dao.as_ref()], bump = dao_token.bump, seeds::program = vortex_hub::ID)]
    pub dao_token: Account<'info, DaoToken>,
    #[account(init_if_needed, payer = owner, space = 8 + Stake::INIT_SPACE, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump)]
    pub stake: Account<'info, Stake>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub owner: Signer<'info>,
    #[account(mut, has_one = mint @ GovError::WrongMint, has_one = vault @ GovError::WrongPool, has_one = token_program @ GovError::WrongPool)]
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump)]
    pub stake: Account<'info, Stake>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

/// The member and the DAO's current governance both sign.
#[derive(Accounts)]
pub struct LockUntil<'info> {
    pub owner: Signer<'info>,
    /// The DAO's current governance account (a PDA its program signs for).
    #[account(address = hub_dao.governance @ GovError::NotActiveGovernance)]
    pub governance: Signer<'info>,
    #[account(address = pool.dao @ GovError::WrongDao)]
    pub hub_dao: Account<'info, HubDao>,
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump)]
    pub stake: Account<'info, Stake>,
}

#[derive(Accounts)]
pub struct Lock<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(address = hub_dao.governance @ GovError::NotActiveGovernance)]
    pub governance: Signer<'info>,
    #[account(address = pool.dao @ GovError::WrongDao)]
    pub hub_dao: Account<'info, HubDao>,
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump)]
    pub stake: Account<'info, Stake>,
    #[account(init_if_needed, payer = owner, space = 8 + LockRecord::INIT_SPACE, seeds = [LOCK_SEED, stake.key().as_ref(), governance.key().as_ref()], bump)]
    pub lock: Account<'info, LockRecord>,
    pub system_program: Program<'info, System>,
}

/// Only the governance that locked it signs; the member doesn't need to.
#[derive(Accounts)]
pub struct Unlock<'info> {
    pub governance: Signer<'info>,
    #[account(mut)]
    pub stake: Account<'info, Stake>,
    #[account(mut, seeds = [LOCK_SEED, stake.key().as_ref(), governance.key().as_ref()], bump = lock.bump)]
    pub lock: Account<'info, LockRecord>,
}

#[derive(Accounts)]
pub struct OpenConverter<'info> {
    /// The DAO's treasury: a passed proposal. It pays the rent.
    #[account(mut, address = vortex_hub::treasury_address(&hub_dao.key()) @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()], bump = dao_token.bump, seeds::program = vortex_hub::ID)]
    pub dao_token: Account<'info, DaoToken>,
    #[account(constraint = from_pool.dao == hub_dao.key() @ GovError::WrongDao)]
    pub from_pool: Account<'info, Pool>,
    /// Converters only pay into the current pool.
    #[account(
        constraint = to_pool.dao == hub_dao.key() @ GovError::WrongDao,
        address = dao_token.pool @ GovError::PoolNotCurrent,
        has_one = mint @ GovError::WrongMint,
        has_one = token_program @ GovError::WrongPool,
    )]
    pub to_pool: Account<'info, Pool>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = treasury,
        space = 8 + Converter::INIT_SPACE,
        seeds = [CONVERTER_SEED, from_pool.key().as_ref(), to_pool.key().as_ref()],
        bump,
    )]
    pub converter: Account<'info, Converter>,
    #[account(
        init,
        payer = treasury,
        seeds = [CONVERTER_VAULT_SEED, converter.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = converter,
        token::token_program = token_program,
    )]
    pub converter_vault: InterfaceAccount<'info, TokenAccount>,
    /// The treasury's account for the new token, when `fund` is above 0.
    #[account(mut, token::mint = mint, token::authority = treasury, token::token_program = token_program)]
    pub treasury_account: Option<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Convert<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = from_pool @ GovError::WrongPool, has_one = to_pool @ GovError::WrongPool)]
    pub converter: Box<Account<'info, Converter>>,
    #[account(address = converter.vault @ GovError::WrongPool, mut)]
    pub converter_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = from_pool.mint == from_mint.key() @ GovError::WrongMint,
        constraint = from_pool.vault == from_vault.key() @ GovError::WrongPool,
        constraint = from_pool.token_program == from_token_program.key() @ GovError::WrongPool,
    )]
    pub from_pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [STAKE_SEED, from_pool.key().as_ref(), owner.key().as_ref()], bump = from_stake.bump)]
    pub from_stake: Box<Account<'info, Stake>>,
    #[account(mut)]
    pub from_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub from_mint: Box<InterfaceAccount<'info, Mint>>,
    /// The treasury's account for the old token: it gets what's converted.
    #[account(
        mut,
        token::mint = from_mint,
        token::authority = vortex_hub::treasury_address(&from_pool.dao),
        token::token_program = from_token_program,
    )]
    pub treasury_from_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = to_pool.mint == to_mint.key() @ GovError::WrongMint,
        constraint = to_pool.vault == to_vault.key() @ GovError::WrongPool,
        constraint = to_pool.token_program == to_token_program.key() @ GovError::WrongPool,
    )]
    pub to_pool: Box<Account<'info, Pool>>,
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Stake::INIT_SPACE,
        seeds = [STAKE_SEED, to_pool.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub to_stake: Box<Account<'info, Stake>>,
    #[account(mut)]
    pub to_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub to_mint: Box<InterfaceAccount<'info, Mint>>,
    pub from_token_program: Interface<'info, TokenInterface>,
    pub to_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseConverter<'info> {
    #[account(address = vortex_hub::treasury_address(&converter.dao) @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub converter: Account<'info, Converter>,
    #[account(mut, address = converter.vault @ GovError::WrongPool)]
    pub converter_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = treasury, token::token_program = token_program)]
    pub treasury_account: InterfaceAccount<'info, TokenAccount>,
    #[account(address = converter_vault.mint @ GovError::WrongMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct PoolCreated {
    pub dao: Pubkey,
    pub pool: Pubkey,
    pub mint: Pubkey,
}

#[event]
pub struct Staked {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct Withdrawn {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct LockedUntil {
    pub stake: Pubkey,
    pub governance: Pubkey,
    pub until: i64,
}

#[event]
pub struct Locked {
    pub stake: Pubkey,
    pub locker: Pubkey,
    pub amount: u64,
    pub locked: u64,
}

#[event]
pub struct Unlocked {
    pub stake: Pubkey,
    pub locker: Pubkey,
    pub amount: u64,
    pub locked: u64,
}

#[event]
pub struct ConverterOpened {
    pub dao: Pubkey,
    pub converter: Pubkey,
    pub from_pool: Pubkey,
    pub to_pool: Pubkey,
    pub rate_num: u64,
    pub rate_den: u64,
}

#[event]
pub struct Converted {
    pub converter: Pubkey,
    pub owner: Pubkey,
    pub amount_in: u64,
    pub amount_out: u64,
}

#[event]
pub struct ConverterClosed {
    pub converter: Pubkey,
    pub returned: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stake(amount: u64, locked: u64, locked_until: i64) -> Stake {
        Stake { pool: Pubkey::default(), owner: Pubkey::default(), amount, locked, locked_until, bump: 0 }
    }

    #[test]
    fn only_unlocked_stake_comes_out() {
        let mut s = stake(100, 30, 50);
        assert!(s.take(10, 49).is_err(), "a vote it was cast in is still open");
        assert!(s.take(71, 50).is_err(), "30 of 100 is locked");
        s.take(70, 50).unwrap();
        assert_eq!((s.amount, s.locked), (30, 30));
        assert!(s.take(1, 60).is_err());
    }

    #[test]
    fn addresses_match_vortex_core() {
        let (dao, mint, owner) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let pool = vortex_core::pool_address(&dao, &mint);
        assert_eq!(pool, Pubkey::find_program_address(&[POOL_SEED, dao.as_ref(), mint.as_ref()], &crate::ID).0);
        assert_eq!(vortex_core::stake_address(&pool, &owner), Pubkey::find_program_address(&[STAKE_SEED, pool.as_ref(), owner.as_ref()], &crate::ID).0);
        assert_eq!(vortex_core::STAKE_PROGRAM_ID, crate::ID);
    }
}
