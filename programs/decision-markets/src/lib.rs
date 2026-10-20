//! Vortexes decision markets (futarchy), the Solana counterpart of the EVM
//! `DecisionMarketsGovernance.sol`: no votes. Each proposal opens two
//! markets - one priced as if it passes, one as if it fails - and whichever
//! values the DAO's token higher over the trading window, by time-weighted
//! average price, decides.
//!
//! - **Conditional tokens, on a ledger.** Anyone `split`s the DAO's token
//!   (base) or the quote token (wrapped SOL, USDC... set per DAO) into
//!   equal pass and fail amounts, and `merge`s pairs back while trading
//!   lasts. The EVM version deploys four ERC20s per proposal; here each
//!   proposal keeps the balances itself.
//! - **Two pools per proposal** (pass-base/pass-quote and fail-base/
//!   fail-quote), constant product with a 0.3% fee, as the EVM pair.
//!   Proposing seeds both with the proposer's base and quote (split, so the
//!   same amounts go into each), and trading opens at once - one step, where
//!   the EVM version needs two for gas.
//! - **TWAP.** Each pool keeps a running sum of its price (quote per base,
//!   scaled by 10^12) × seconds, updated before every trade. After
//!   `trading_period` anyone finalizes: it passes if the pass TWAP beats
//!   the fail TWAP by more than `threshold_bps`. Then it runs through the
//!   hub after `timelock`, within `execution_period`.
//! - **Settlement.** The winning side redeems 1:1 for the underlying; the
//!   losing side is worthless. The proposer reclaims what's left in the
//!   winning pool (their seed plus fees, after trades). A cancelled proposal
//!   settles as failed, as it won't run.
//!
//! Each proposal tracks the collateral behind it, and every payout comes
//! out of that, so one proposal's market can never draw on another's.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{validate_instructions, validate_uri, GovError, ProposalCore, StoredInstruction, GOVERNANCE_SEED};
use vortex_hub::{Dao as HubDao, DaoToken};

declare_id!("FfUPS88Xoao8N4QTW9SGtua1XyN2q4HyBpNha7AB5t4S");

pub const BASE_VAULT_SEED: &[u8] = b"base_vault";
pub const QUOTE_VAULT_SEED: &[u8] = b"quote_vault";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const POSITION_SEED: &[u8] = b"position";
/// Prices are quote per base × 10^12.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;

#[program]
pub mod vortex_decision_markets {
    use super::*;

    /// Sets up a hub DAO here: rules, the DAO's token (base), the quote
    /// token, and a vault for each. The creator right after create_dao, or
    /// the treasury when switching here.
    pub fn init_governance(ctx: Context<InitGovernance>, config: MarketsConfig) -> Result<()> {
        config.validate()?;
        let hub_dao = &ctx.accounts.hub_dao;
        let hub_dao_key = hub_dao.key();
        let becoming_active = hub_dao.governance_program == crate::ID
            || hub_dao.pending_switch.as_ref().is_some_and(|s| s.program == crate::ID);
        require!(becoming_active, GovError::NotActiveGovernance);
        let treasury = vortex_hub::treasury_address(&hub_dao_key);
        let who = ctx.accounts.authority.key();
        let first_setup = hub_dao.epoch == 1 && hub_dao.governance_program == crate::ID;
        require!(who == treasury || (first_setup && who == hub_dao.creator), GovError::Unauthorized);
        require_keys_neq!(ctx.accounts.base_mint.key(), ctx.accounts.quote_mint.key(), GovError::InvalidConfig);

        let g = &mut ctx.accounts.governance;
        g.hub_dao = hub_dao_key;
        g.treasury = treasury;
        g.executor = vortex_hub::executor_address(&hub_dao_key);
        g.base_mint = ctx.accounts.base_mint.key();
        g.quote_mint = ctx.accounts.quote_mint.key();
        g.base_vault = ctx.accounts.base_vault.key();
        g.quote_vault = ctx.accounts.quote_vault.key();
        g.config = config;
        g.proposal_count = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), base_mint: g.base_mint, quote_mint: g.quote_mint });
        Ok(())
    }

    /// Proposes `instructions` and opens its markets, seeding each pool with
    /// `base_seed` and `quote_seed` from the proposer. `id` must be
    /// proposal_count + 1.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>, base_seed: u64, quote_seed: u64) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        require!(base_seed >= g.config.min_base_seed && quote_seed >= g.config.min_quote_seed && base_seed > 0 && quote_seed > 0, GovError::InvalidAmount);
        let t = &ctx.accounts.tokens;
        pay_in(&ctx.accounts.proposer_base, &t.base_vault, &t.base_mint, &ctx.accounts.proposer, &t.base_token_program, base_seed)?;
        pay_in(&ctx.accounts.proposer_quote, &t.quote_vault, &t.quote_mint, &ctx.accounts.proposer, &t.quote_token_program, quote_seed)?;

        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions };
        p.governance = g.key();
        p.id = id;
        p.proposer = ctx.accounts.proposer.key();
        p.metadata_uri = metadata_uri;
        p.created_at = now;
        p.trading_deadline = now + g.config.trading_period as i64;
        p.pass_pool = Pool { base: base_seed, quote: quote_seed };
        p.fail_pool = Pool { base: base_seed, quote: quote_seed };
        p.last_update = now;
        p.base_collateral = base_seed;
        p.quote_collateral = quote_seed;
        p.bump = ctx.bumps.proposal;
        emit!(ProposalCreated {
            hub_dao: g.hub_dao,
            proposal: p.key(),
            id,
            proposer: p.proposer,
            metadata_uri: p.metadata_uri.clone(),
            base_seed,
            quote_seed,
            trading_deadline: p.trading_deadline,
        });
        Ok(())
    }

    /// Deposits `base` of the DAO's token and `quote` of the quote token,
    /// getting that much of each as both pass and fail tokens. While trading lasts.
    pub fn split(ctx: Context<Split>, base: u64, quote: u64) -> Result<()> {
        require!(base > 0 || quote > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(!p.finalized && now < p.trading_deadline, GovError::TradingClosed);
        let t = &ctx.accounts.tokens;
        if base > 0 {
            pay_in(&ctx.accounts.owner_base, &t.base_vault, &t.base_mint, &ctx.accounts.owner, &t.base_token_program, base)?;
        }
        if quote > 0 {
            pay_in(&ctx.accounts.owner_quote, &t.quote_vault, &t.quote_mint, &ctx.accounts.owner, &t.quote_token_program, quote)?;
        }
        p.base_collateral = p.base_collateral.checked_add(base).ok_or(GovError::Overflow)?;
        p.quote_collateral = p.quote_collateral.checked_add(quote).ok_or(GovError::Overflow)?;
        let pos = &mut ctx.accounts.position;
        if pos.owner == Pubkey::default() {
            pos.proposal = p.key();
            pos.owner = ctx.accounts.owner.key();
            pos.bump = ctx.bumps.position;
        }
        pos.pass_base = pos.pass_base.checked_add(base).ok_or(GovError::Overflow)?;
        pos.fail_base = pos.fail_base.checked_add(base).ok_or(GovError::Overflow)?;
        pos.pass_quote = pos.pass_quote.checked_add(quote).ok_or(GovError::Overflow)?;
        pos.fail_quote = pos.fail_quote.checked_add(quote).ok_or(GovError::Overflow)?;
        emit!(TokensSplit { proposal: p.key(), owner: pos.owner, base, quote });
        Ok(())
    }

    /// Turns pairs of pass and fail tokens back into the underlying, while trading lasts.
    pub fn merge(ctx: Context<Redeem>, base: u64, quote: u64) -> Result<()> {
        require!(base > 0 || quote > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(!p.finalized && now < p.trading_deadline, GovError::TradingClosed);
        let pos = &mut ctx.accounts.position;
        require!(pos.pass_base >= base && pos.fail_base >= base && pos.pass_quote >= quote && pos.fail_quote >= quote, GovError::InsufficientConditionalBalance);
        pos.pass_base -= base;
        pos.fail_base -= base;
        pos.pass_quote -= quote;
        pos.fail_quote -= quote;
        p.base_collateral = p.base_collateral.checked_sub(base).ok_or(GovError::Overflow)?;
        p.quote_collateral = p.quote_collateral.checked_sub(quote).ok_or(GovError::Overflow)?;
        let (g, t) = (&ctx.accounts.governance, &ctx.accounts.tokens);
        pay_out(g, &t.base_vault, &ctx.accounts.owner_base, &t.base_mint, &t.base_token_program, base)?;
        pay_out(g, &t.quote_vault, &ctx.accounts.owner_quote, &t.quote_mint, &t.quote_token_program, quote)?;
        emit!(Merged { proposal: p.key(), owner: pos.owner, base, quote });
        Ok(())
    }

    /// Swaps in one market: `amount_in` of its base (selling) or quote
    /// (buying) for at least `min_out` of the other. While trading lasts.
    pub fn trade(ctx: Context<Trade>, market: Market, side_in: Side, amount_in: u64, min_out: u64) -> Result<()> {
        require!(amount_in > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(!p.finalized && now < p.trading_deadline, GovError::TradingClosed);
        accrue(p, now);
        let proposal_key = p.key();
        let p: &mut Proposal = p;
        let pos: &mut Position = &mut ctx.accounts.position;
        let (pool, base_bal, quote_bal) = match market {
            Market::Pass => (&mut p.pass_pool, &mut pos.pass_base, &mut pos.pass_quote),
            Market::Fail => (&mut p.fail_pool, &mut pos.fail_base, &mut pos.fail_quote),
        };
        let (bal_in, bal_out, reserve_in, reserve_out) = match side_in {
            Side::Base => (base_bal, quote_bal, &mut pool.base, &mut pool.quote),
            Side::Quote => (quote_bal, base_bal, &mut pool.quote, &mut pool.base),
        };
        require!(*bal_in >= amount_in, GovError::InsufficientConditionalBalance);
        let out = amount_out(amount_in, *reserve_in, *reserve_out);
        require!(out > 0 && out >= min_out, GovError::SlippageExceeded);
        require!(out < *reserve_out, GovError::SlippageExceeded);
        *bal_in -= amount_in;
        *bal_out = bal_out.checked_add(out).ok_or(GovError::Overflow)?;
        *reserve_in = reserve_in.checked_add(amount_in).ok_or(GovError::Overflow)?;
        *reserve_out -= out;
        emit!(Traded { proposal: proposal_key, trader: pos.owner, market, side_in, amount_in, amount_out: out });
        Ok(())
    }

    /// Settles the markets once trading has ended. Anyone.
    pub fn finalize(ctx: Context<Finalize>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(!p.finalized, GovError::AlreadyFinalized);
        require!(now >= p.trading_deadline, GovError::TradingOpen);
        accrue(p, now);
        let elapsed = (p.trading_deadline - p.created_at).max(1) as u128;
        p.pass_twap = p.pass_cumulative / elapsed;
        p.fail_twap = p.fail_cumulative / elapsed;
        p.passed = p.pass_twap * 10_000 > p.fail_twap * (10_000 + c.threshold_bps as u128);
        p.finalized = true;
        if p.passed {
            p.queued_at = now;
        }
        emit!(ProposalFinalized { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, passed: p.passed, pass_twap: p.pass_twap, fail_twap: p.fail_twap });
        Ok(())
    }

    /// Pays out the caller's winning-side tokens 1:1 (the fail side if the
    /// proposal failed or was cancelled); the other side is worthless.
    pub fn redeem(ctx: Context<Redeem>) -> Result<()> {
        let p = &mut ctx.accounts.proposal;
        require!(p.finalized, GovError::NotFinalized);
        let pos = &mut ctx.accounts.position;
        let (base, quote) = if p.passed { (pos.pass_base, pos.pass_quote) } else { (pos.fail_base, pos.fail_quote) };
        require!(base > 0 || quote > 0, GovError::NothingToClaim);
        pos.pass_base = 0;
        pos.pass_quote = 0;
        pos.fail_base = 0;
        pos.fail_quote = 0;
        p.base_collateral = p.base_collateral.checked_sub(base).ok_or(GovError::Overflow)?;
        p.quote_collateral = p.quote_collateral.checked_sub(quote).ok_or(GovError::Overflow)?;
        let (g, t) = (&ctx.accounts.governance, &ctx.accounts.tokens);
        pay_out(g, &t.base_vault, &ctx.accounts.owner_base, &t.base_mint, &t.base_token_program, base)?;
        pay_out(g, &t.quote_vault, &ctx.accounts.owner_quote, &t.quote_mint, &t.quote_token_program, quote)?;
        emit!(Redeemed { proposal: p.key(), owner: pos.owner, base, quote });
        Ok(())
    }

    /// The proposer takes what's left in the winning pool (their seed plus
    /// fees, after trades), once it's settled.
    pub fn reclaim_liquidity(ctx: Context<ReclaimLiquidity>) -> Result<()> {
        let p = &mut ctx.accounts.proposal;
        require!(p.finalized, GovError::NotFinalized);
        require!(!p.liquidity_reclaimed, GovError::AlreadyClaimed);
        let pool = if p.passed { p.pass_pool } else { p.fail_pool };
        p.liquidity_reclaimed = true;
        p.pass_pool = Pool::default();
        p.fail_pool = Pool::default();
        p.base_collateral = p.base_collateral.checked_sub(pool.base).ok_or(GovError::Overflow)?;
        p.quote_collateral = p.quote_collateral.checked_sub(pool.quote).ok_or(GovError::Overflow)?;
        let (g, t) = (&ctx.accounts.governance, &ctx.accounts.tokens);
        pay_out(g, &t.base_vault, &ctx.accounts.proposer_base, &t.base_mint, &t.base_token_program, pool.base)?;
        pay_out(g, &t.quote_vault, &ctx.accounts.proposer_quote, &t.quote_mint, &t.quote_token_program, pool.quote)?;
        emit!(LiquidityReclaimed { proposal: p.key(), base: pool.base, quote: pool.quote });
        Ok(())
    }

    /// The hub's check before it runs a proposal (CONFIRM_EXECUTION): passed,
    /// after the timelock, within the execution period.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.finalized, GovError::NotFinalized);
        require!(p.passed, GovError::ProposalDidNotPass);
        require!(now >= p.queued_at + c.timelock as i64, GovError::ProposalNotExecutable);
        require!(now <= p.queued_at + c.timelock as i64 + c.execution_period as i64, GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't run: its proposer before the markets
    /// settle, or the DAO through a proposal. Unsettled markets settle as
    /// failed (the fail side redeems), since it won't run.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let who = ctx.accounts.authority.key();
        require!((who == p.proposer && !p.finalized) || who == g.treasury, GovError::Unauthorized);
        p.cancelled = true;
        if !p.finalized {
            p.finalized = true;
            p.passed = false;
        }
        emit!(ProposalCancelled { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, by: who });
        Ok(())
    }

    /// Replaces the rules (only through a passed proposal).
    pub fn update_config(ctx: Context<UpdateConfig>, config: MarketsConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }
}

// ---------------------------------------------------------------
// MARKET MATH
// ---------------------------------------------------------------

/// Constant product with a 0.3% fee (the EVM pair's getAmountOut).
pub fn amount_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> u64 {
    let in_fee = amount_in as u128 * 997;
    let out = in_fee * reserve_out as u128 / (reserve_in as u128 * 1000 + in_fee);
    out as u64
}

/// A pool's price: quote per base × PRICE_SCALE.
pub fn price(pool: &Pool) -> u128 {
    if pool.base == 0 {
        return 0;
    }
    pool.quote as u128 * PRICE_SCALE / pool.base as u128
}

/// Adds each pool's price × seconds since the last update, up to the trading deadline.
fn accrue(p: &mut Proposal, now: i64) {
    let until = now.min(p.trading_deadline);
    if until > p.last_update {
        let dt = (until - p.last_update) as u128;
        p.pass_cumulative = p.pass_cumulative.saturating_add(price(&p.pass_pool).saturating_mul(dt));
        p.fail_cumulative = p.fail_cumulative.saturating_add(price(&p.fail_pool).saturating_mul(dt));
        p.last_update = until;
    }
}

// ---------------------------------------------------------------
// TOKEN HELPERS
// ---------------------------------------------------------------

fn pay_in<'info>(
    from: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    authority: &Signer<'info>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new(
            token_program.key(),
            TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: authority.to_account_info() },
        ),
        amount,
        mint.decimals,
    )
}

fn pay_out<'info>(
    g: &Account<'info, Governance>,
    from: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let hub_dao = g.hub_dao;
    let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub_dao.as_ref(), &[g.bump]];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: g.to_account_info() },
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
pub struct MarketsConfig {
    /// How long both markets trade.
    pub trading_period: u32,
    /// How far the pass TWAP must beat the fail TWAP (basis points).
    pub threshold_bps: u16,
    pub timelock: u32,
    pub execution_period: u32,
    /// Smallest seeds a proposal opens its pools with.
    pub min_base_seed: u64,
    pub min_quote_seed: u64,
}

impl MarketsConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.trading_period > 0 && self.execution_period > 0, GovError::InvalidConfig);
        require!(self.threshold_bps > 0, GovError::InvalidConfig);
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum Market {
    Pass,
    Fail,
}

/// What goes into a trade: the market's base token (selling it) or its quote token (buying base).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum Side {
    Base,
    Quote,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace, Default)]
pub struct Pool {
    pub base: u64,
    pub quote: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    /// The DAO's token.
    pub base_mint: Pubkey,
    /// What it's priced in.
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub config: MarketsConfig,
    pub proposal_count: u64,
    pub bump: u8,
}

#[account]
pub struct Proposal {
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    /// When the markets opened: the start of the TWAP window.
    pub created_at: i64,
    pub trading_deadline: i64,
    pub pass_pool: Pool,
    pub fail_pool: Pool,
    pub last_update: i64,
    /// Sum of price × seconds (u128).
    pub pass_cumulative: u128,
    pub fail_cumulative: u128,
    /// Underlying held for this proposal's markets.
    pub base_collateral: u64,
    pub quote_collateral: u64,
    pub finalized: bool,
    pub passed: bool,
    pub pass_twap: u128,
    pub fail_twap: u128,
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
    pub liquidity_reclaimed: bool,
    pub bump: u8,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + 8 + 8
            + Pool::INIT_SPACE * 2
            + 8 + 16 + 16
            + 8 + 8
            + 1 + 1 + 16 + 16 + 8
            + 1 + 1 + 1 + 1
    }
}

/// A trader's pass and fail balances in one proposal's markets.
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub proposal: Pubkey,
    pub owner: Pubkey,
    pub pass_base: u64,
    pub pass_quote: u64,
    pub fail_base: u64,
    pub fail_quote: u64,
    pub bump: u8,
}

// ---------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
pub struct InitGovernance<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Box<Account<'info, HubDao>>,
    #[account(init, payer = payer, space = 8 + Governance::INIT_SPACE, seeds = [GOVERNANCE_SEED, hub_dao.key().as_ref()], bump)]
    pub governance: Box<Account<'info, Governance>>,
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    /// The DAO's token on record at the hub (set_dao_token): setting up -
    /// a switch included - only works with that mint.
    #[account(
        seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()],
        bump = dao_token.bump,
        seeds::program = vortex_hub::ID,
        constraint = dao_token.mint == base_mint.key() @ GovError::WrongMint,
    )]
    pub dao_token: Box<Account<'info, DaoToken>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init, payer = payer, seeds = [BASE_VAULT_SEED, governance.key().as_ref()], bump,
        token::mint = base_mint, token::authority = governance, token::token_program = base_token_program,
    )]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        init, payer = payer, seeds = [QUOTE_VAULT_SEED, governance.key().as_ref()], bump,
        token::mint = quote_mint, token::authority = governance, token::token_program = quote_token_program,
    )]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// The DAO's two vaults, mints and token programs, checked against its governance.
#[derive(Accounts)]
pub struct Tokens<'info> {
    #[account(
        has_one = base_mint @ GovError::WrongDao, has_one = quote_mint @ GovError::WrongDao,
        has_one = base_vault @ GovError::WrongDao, has_one = quote_vault @ GovError::WrongDao,
    )]
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao, constraint = governance.key() == tokens.governance.key() @ GovError::WrongDao)]
    pub governance: Box<Account<'info, Governance>>,
    pub hub_dao: Box<Account<'info, HubDao>>,
    #[account(
        init, payer = proposer, space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub proposal: Box<Account<'info, Proposal>>,
    #[account(mut, token::mint = tokens.base_mint, token::authority = proposer, token::token_program = tokens.base_token_program)]
    pub proposer_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = tokens.quote_mint, token::authority = proposer, token::token_program = tokens.quote_token_program)]
    pub proposer_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub tokens: Tokens<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Split<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, constraint = proposal.governance == tokens.governance.key() @ GovError::WrongDao)]
    pub proposal: Box<Account<'info, Proposal>>,
    #[account(
        init_if_needed, payer = owner, space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub position: Box<Account<'info, Position>>,
    #[account(mut, token::mint = tokens.base_mint, token::authority = owner, token::token_program = tokens.base_token_program)]
    pub owner_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = tokens.quote_mint, token::authority = owner, token::token_program = tokens.quote_token_program)]
    pub owner_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub tokens: Tokens<'info>,
    pub system_program: Program<'info, System>,
}

/// Merging and redeeming: tokens out to the owner's accounts.
#[derive(Accounts)]
pub struct Redeem<'info> {
    pub owner: Signer<'info>,
    #[account(constraint = governance.key() == tokens.governance.key() @ GovError::WrongDao)]
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Box<Account<'info, Proposal>>,
    #[account(mut, seeds = [POSITION_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump = position.bump)]
    pub position: Box<Account<'info, Position>>,
    #[account(mut, token::mint = tokens.base_mint, token::token_program = tokens.base_token_program)]
    pub owner_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = tokens.quote_mint, token::token_program = tokens.quote_token_program)]
    pub owner_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub tokens: Tokens<'info>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    pub owner: Signer<'info>,
    #[account(mut)]
    pub proposal: Box<Account<'info, Proposal>>,
    #[account(mut, seeds = [POSITION_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump = position.bump)]
    pub position: Box<Account<'info, Position>>,
}

#[derive(Accounts)]
pub struct Finalize<'info> {
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Box<Account<'info, Proposal>>,
}

#[derive(Accounts)]
pub struct ReclaimLiquidity<'info> {
    pub proposer: Signer<'info>,
    #[account(constraint = governance.key() == tokens.governance.key() @ GovError::WrongDao)]
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut, has_one = governance @ GovError::WrongDao, has_one = proposer @ GovError::Unauthorized)]
    pub proposal: Box<Account<'info, Proposal>>,
    #[account(mut, token::mint = tokens.base_mint, token::token_program = tokens.base_token_program)]
    pub proposer_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = tokens.quote_mint, token::token_program = tokens.quote_token_program)]
    pub proposer_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    pub tokens: Tokens<'info>,
}

/// Account order fixed by vortex-core's CONFIRM_EXECUTION interface.
#[derive(Accounts)]
pub struct ConfirmExecution<'info> {
    #[account(address = governance.executor @ GovError::Unauthorized)]
    pub executor: Signer<'info>,
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Box<Account<'info, Proposal>>,
}

#[derive(Accounts)]
pub struct Cancel<'info> {
    pub authority: Signer<'info>,
    pub governance: Box<Account<'info, Governance>>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Box<Account<'info, Proposal>>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub governance: Box<Account<'info, Governance>>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct GovernanceInitialized {
    pub hub_dao: Pubkey,
    pub governance: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
}

#[event]
pub struct ProposalCreated {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub base_seed: u64,
    pub quote_seed: u64,
    pub trading_deadline: i64,
}

#[event]
pub struct TokensSplit {
    pub proposal: Pubkey,
    pub owner: Pubkey,
    pub base: u64,
    pub quote: u64,
}

#[event]
pub struct Merged {
    pub proposal: Pubkey,
    pub owner: Pubkey,
    pub base: u64,
    pub quote: u64,
}

#[event]
pub struct Traded {
    pub proposal: Pubkey,
    pub trader: Pubkey,
    pub market: Market,
    pub side_in: Side,
    pub amount_in: u64,
    pub amount_out: u64,
}

#[event]
pub struct ProposalFinalized {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub passed: bool,
    pub pass_twap: u128,
    pub fail_twap: u128,
}

#[event]
pub struct Redeemed {
    pub proposal: Pubkey,
    pub owner: Pubkey,
    pub base: u64,
    pub quote: u64,
}

#[event]
pub struct LiquidityReclaimed {
    pub proposal: Pubkey,
    pub base: u64,
    pub quote: u64,
}

#[event]
pub struct ProposalExecuted {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
}

#[event]
pub struct ProposalCancelled {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub by: Pubkey,
}

#[event]
pub struct ConfigUpdated {
    pub hub_dao: Pubkey,
    pub config: MarketsConfig,
}
