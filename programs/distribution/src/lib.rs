//! Vortexes distribution: a DAO's Distributor and its raises.
//!
//! **Distributor** (`[DISTRIBUTOR_SEED, dao, mint, operator]`): holds every
//! token a DAO has promised someone and lets each owner claim what has
//! vested. An allocation belongs to an address, a chat account (a hash of
//! platform and user id, set by the bot), or both: the owner claims for
//! itself; for an account, the operator (the bot) claims into the member's
//! wallet after checking who they are. Every allocation is backed by tokens
//! the Distributor already holds. Allocations come from a passed proposal
//! (the treasury signs), a raise when it settles, or a welcome grant, which
//! a member of the DAO's Delegate council approves. The operator is part of
//! the address, so nobody can take the bot's Distributor with another one.
//!
//! **Raise** (`[RAISE_SEED, dao, create_key]`): a pro-rata fundraise in one
//! token. Contributions stay open for the whole period, past the hard cap;
//! at the end the decider (the founder of a new DAO, or the treasury through
//! a proposal) accepts any amount from the soft cap up, within the decision
//! window, and it goes to the DAO's treasury and nowhere else. Tokens and
//! refunds are pro rata. Below the soft cap, or with no decision in time,
//! everyone is refunded in full.
//!
//! A prop firm's raise pays into its vault instead (vortex-vault): on
//! acceptance the money goes to the vault, which mints its first shares -
//! the DAO's token - to the Distributor, and those are what contributors
//! share pro rata.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{GovError, TREASURY_SEED};
use vortex_hub::Dao as HubDao;

declare_id!("4FeTS89hg2BtrkV2vN73be93bVvokBRHR7gbC6uUJgAq");

pub const DISTRIBUTOR_SEED: &[u8] = b"distributor";
pub const DISTRIBUTOR_VAULT_SEED: &[u8] = b"distributor_vault";
pub const ALLOCATION_SEED: &[u8] = b"allocation";
pub const WELCOMED_SEED: &[u8] = b"welcomed";
pub const RAISE_SEED: &[u8] = b"raise";
pub const RAISE_VAULT_SEED: &[u8] = b"raise_vault";
pub const CONTRIBUTION_SEED: &[u8] = b"contribution";

#[program]
pub mod vortex_distribution {
    use super::*;

    // ---- the Distributor ----

    /// Opens the Distributor of `mint` for a DAO, run by `operator`. Anyone
    /// (it holds nothing until the DAO funds it).
    pub fn create_distributor(ctx: Context<CreateDistributor>, operator: Pubkey, claim_deadline: i64) -> Result<()> {
        let d = &mut ctx.accounts.distributor;
        d.dao = ctx.accounts.hub_dao.key();
        d.mint = ctx.accounts.mint.key();
        d.vault = ctx.accounts.vault.key();
        d.operator = operator;
        d.token_program = ctx.accounts.token_program.key();
        d.outstanding = 0;
        d.allocation_count = 0;
        d.claim_deadline = claim_deadline;
        d.closed = false;
        d.welcome_amount = 0;
        d.welcome_cap = 0;
        d.welcome_granted = 0;
        d.bump = ctx.bumps.distributor;
        emit!(DistributorCreated { dao: d.dao, distributor: d.key(), mint: d.mint, operator });
        Ok(())
    }

    /// Promises `amount` to `owner` and/or `account`, vesting from `start`:
    /// nothing before `start + cliff`, then linearly until `start +
    /// duration`. Only a passed proposal (the treasury).
    pub fn allocate(ctx: Context<Allocate>, owner: Pubkey, account: [u8; 32], amount: u64, start: i64, cliff: i64, duration: i64) -> Result<()> {
        let id = ctx.accounts.distributor.allocation_count + 1;
        write_allocation(&mut ctx.accounts.distributor, &ctx.accounts.vault, &mut ctx.accounts.allocation, id, ctx.bumps.allocation, owner, account, amount, start, cliff, duration)
    }

    /// The owner claims what has vested, into a token account of theirs.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let a = &ctx.accounts.allocation;
        require!(a.owner != Pubkey::default() && ctx.accounts.signer.key() == a.owner, GovError::Unauthorized);
        pay(&mut ctx.accounts.distributor, &ctx.accounts.vault, &mut ctx.accounts.allocation, &ctx.accounts.to, &ctx.accounts.mint, &ctx.accounts.token_program)
    }

    /// The operator claims an account's vested tokens into the member's
    /// current wallet (`to`), having checked who they are.
    pub fn claim_for(ctx: Context<Claim>) -> Result<()> {
        require_keys_eq!(ctx.accounts.signer.key(), ctx.accounts.distributor.operator, GovError::Unauthorized);
        require!(ctx.accounts.allocation.account != [0u8; 32], GovError::Unauthorized);
        pay(&mut ctx.accounts.distributor, &ctx.accounts.vault, &mut ctx.accounts.allocation, &ctx.accounts.to, &ctx.accounts.mint, &ctx.accounts.token_program)
    }

    /// Turns welcome grants on (amount above 0) or off, and caps their
    /// total. Only a passed proposal.
    pub fn set_welcome(ctx: Context<DistributorAdmin>, amount: u64, cap: u64) -> Result<()> {
        let d = &mut ctx.accounts.distributor;
        d.welcome_amount = amount;
        d.welcome_cap = cap;
        emit!(WelcomeTermsSet { distributor: d.key(), amount, cap });
        Ok(())
    }

    /// A member of the DAO's Delegate council grants a new member their
    /// welcome tokens, once they've checked the member paid their dues or
    /// registered. One per `key` (the account, or the wallet when there's none).
    pub fn grant_welcome(ctx: Context<GrantWelcome>, member: Pubkey, account: [u8; 32]) -> Result<()> {
        let g = &ctx.accounts.governance;
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance, g.key(), GovError::NotActiveGovernance);
        require!(g.is_council(&ctx.accounts.approver.key()), GovError::NotCouncilMember);
        let d = &mut ctx.accounts.distributor;
        require!(d.welcome_amount > 0 && d.welcome_granted.saturating_add(d.welcome_amount) <= d.welcome_cap, GovError::WelcomeUnavailable);
        d.welcome_granted += d.welcome_amount;
        let amount = d.welcome_amount;
        ctx.accounts.welcomed.bump = ctx.bumps.welcomed;
        let id = d.allocation_count + 1;
        let now = Clock::get()?.unix_timestamp;
        write_allocation(d, &ctx.accounts.vault, &mut ctx.accounts.allocation, id, ctx.bumps.allocation, member, account, amount, now, 0, 0)?;
        emit!(WelcomeGranted { distributor: ctx.accounts.distributor.key(), member, account, approver: ctx.accounts.approver.key() });
        Ok(())
    }

    /// Sends what nobody's been promised back to the treasury. Only a passed proposal.
    pub fn return_unpromised(ctx: Context<ReturnTokens>) -> Result<()> {
        let d = &ctx.accounts.distributor;
        let amount = ctx.accounts.vault.amount.saturating_sub(d.outstanding);
        send_from_distributor(d, &ctx.accounts.vault, &ctx.accounts.treasury_token, &ctx.accounts.mint, &ctx.accounts.token_program, amount)
    }

    /// After the claim deadline: closes the Distributor and returns
    /// everything still in it to the treasury. Only a passed proposal.
    pub fn close_distributor(ctx: Context<ReturnTokens>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let d = &ctx.accounts.distributor;
        require!(!d.closed, GovError::DistributorClosed);
        require!(d.claim_deadline != 0 && now >= d.claim_deadline, GovError::TooEarly);
        let amount = ctx.accounts.vault.amount;
        send_from_distributor(d, &ctx.accounts.vault, &ctx.accounts.treasury_token, &ctx.accounts.mint, &ctx.accounts.token_program, amount)?;
        let d = &mut ctx.accounts.distributor;
        d.closed = true;
        d.outstanding = 0;
        Ok(())
    }

    // ---- raises ----

    /// Opens a raise for a DAO, paying into its treasury and selling tokens
    /// the `distributor` holds. `decider` accepts the amount: the founder of
    /// a new DAO, or the DAO's treasury (then by proposal). Anyone can open
    /// one; contributors see the DAO, its rules and its treasury first.
    ///
    /// For a prop firm, `prop_firm_vault` is the DAO's vault, made with this
    /// raise as its seeder; `sale_supply` is then 0 and set at acceptance to
    /// the shares the vault mints.
    pub fn create_raise(ctx: Context<CreateRaise>, terms: RaiseTerms, decider: Pubkey) -> Result<()> {
        require!(terms.end > terms.start && terms.soft_cap > 0, GovError::InvalidConfig);
        require!(terms.max_contribution == 0 || terms.max_contribution >= terms.min_contribution, GovError::InvalidConfig);
        require_keys_eq!(ctx.accounts.distributor.dao, ctx.accounts.hub_dao.key(), GovError::WrongDao);
        let prop_firm_vault = match &ctx.accounts.prop_firm_vault {
            Some(v) => {
                require!(terms.sale_supply == 0, GovError::InvalidConfig);
                require_keys_eq!(v.dao, ctx.accounts.hub_dao.key(), GovError::WrongDao);
                require_keys_eq!(v.seeder, ctx.accounts.raise.key(), GovError::Unauthorized);
                require_keys_eq!(v.asset_mint, ctx.accounts.currency.key(), GovError::WrongMint);
                require_keys_eq!(v.share_mint, ctx.accounts.distributor.mint, GovError::WrongMint);
                v.key()
            }
            None => {
                require!(terms.sale_supply > 0, GovError::InvalidConfig);
                Pubkey::default()
            }
        };
        let r = &mut ctx.accounts.raise;
        r.prop_firm_vault = prop_firm_vault;
        r.dao = ctx.accounts.hub_dao.key();
        r.create_key = ctx.accounts.create_key.key();
        r.decider = decider;
        r.currency = ctx.accounts.currency.key();
        r.currency_token_program = ctx.accounts.token_program.key();
        r.vault = ctx.accounts.vault.key();
        r.distributor = ctx.accounts.distributor.key();
        r.terms = terms;
        r.total = 0;
        r.accepted = 0;
        r.accepted_at = 0;
        r.bump = ctx.bumps.raise;
        emit!(RaiseCreated { dao: r.dao, raise: r.key(), decider, currency: r.currency, terms });
        Ok(())
    }

    /// Contributes `amount`, tied to the contributor's chat `account` if
    /// given (the same account on every contribution from this wallet).
    pub fn contribute(ctx: Context<Contribute>, amount: u64, account: [u8; 32]) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.raise;
        require!(now >= r.terms.start && now < r.terms.end, GovError::RaiseNotOpen);
        require!(amount > 0, GovError::InvalidAmount);
        let c = &mut ctx.accounts.contribution;
        if c.amount == 0 && c.raise == Pubkey::default() {
            c.raise = r.key();
            c.contributor = ctx.accounts.contributor.key();
            c.account = account;
            c.bump = ctx.bumps.contribution;
        } else if account != [0u8; 32] && c.account != account {
            require!(c.account == [0u8; 32], GovError::Unauthorized);
            c.account = account;
        }
        c.amount = c.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        require!(c.amount >= r.terms.min_contribution, GovError::ContributionLimit);
        require!(r.terms.max_contribution == 0 || c.amount <= r.terms.max_contribution, GovError::ContributionLimit);
        r.total = r.total.checked_add(amount).ok_or(GovError::Overflow)?;
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.from.to_account_info(),
                    mint: ctx.accounts.currency.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.contributor.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.currency.decimals,
        )?;
        emit!(Contributed { raise: r.key(), contributor: c.contributor, account: c.account, amount, total: c.amount });
        Ok(())
    }

    /// The decider accepts `amount` (from the soft cap up to everything
    /// contributed, no upper limit); it goes to the DAO's treasury, or for a
    /// prop firm into its vault, whose first shares go to the Distributor.
    pub fn accept(ctx: Context<Accept>, amount: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.raise;
        require_keys_eq!(ctx.accounts.decider.key(), r.decider, GovError::Unauthorized);
        require!(r.accepted_at == 0, GovError::AlreadyAccepted);
        require!(now >= r.terms.end, GovError::RaiseNotEnded);
        require!(now < r.terms.end + r.terms.decision_window, GovError::DecisionClosed);
        require!(amount >= r.terms.soft_cap && amount <= r.total, GovError::AmountOutOfRange);
        if r.prop_firm_vault == Pubkey::default() {
            let unpromised = ctx.accounts.distributor_vault.amount.saturating_sub(ctx.accounts.distributor.outstanding);
            require!(unpromised >= r.terms.sale_supply, GovError::NotFunded);
            let to = ctx.accounts.treasury_token.as_ref().ok_or(GovError::WrongPool)?;
            send_from_raise(r, &ctx.accounts.vault, to, &ctx.accounts.currency, &ctx.accounts.token_program, amount)?;
        } else {
            let a = &ctx.accounts;
            let (Some(pf_vault), Some(vault_assets), Some(share_mint), Some(share_token_program), Some(_)) =
                (&a.prop_firm_vault, &a.vault_assets, &a.share_mint, &a.share_token_program, &a.vault_program)
            else {
                return err!(GovError::WrongPool);
            };
            require_keys_eq!(pf_vault.key(), r.prop_firm_vault, GovError::WrongPool);
            send_from_raise(r, &a.vault, vault_assets, &a.currency, &a.token_program, amount)?;
            let before = ctx.accounts.distributor_vault.amount;
            let seeds: &[&[u8]] = &[RAISE_SEED, r.dao.as_ref(), r.create_key.as_ref(), &[r.bump]];
            vortex_vault::cpi::seed(CpiContext::new_with_signer(
                vortex_vault::ID,
                vortex_vault::cpi::accounts::Seed {
                    seeder: a.raise.to_account_info(),
                    vault: pf_vault.to_account_info(),
                    assets: vault_assets.to_account_info(),
                    share_mint: share_mint.to_account_info(),
                    receiver: a.distributor_vault.to_account_info(),
                    share_token_program: share_token_program.to_account_info(),
                },
                &[seeds],
            ))?;
            ctx.accounts.distributor_vault.reload()?;
            let minted = ctx.accounts.distributor_vault.amount - before;
            ctx.accounts.raise.terms.sale_supply = minted;
        }
        let r = &mut ctx.accounts.raise;
        r.accepted = amount;
        r.accepted_at = now;
        emit!(RaiseAccepted { raise: r.key(), amount, total: r.total });
        Ok(())
    }

    /// After acceptance: writes one contributor's tokens into the
    /// Distributor (vesting as the terms say) and records their refund.
    /// Anyone; the caller pays for the allocation account.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let r = &ctx.accounts.raise;
        require!(r.accepted_at != 0, GovError::NotAccepted);
        let c = &mut ctx.accounts.contribution;
        require!(!c.settled, GovError::AlreadyFinalized);
        c.settled = true;
        let total = r.total as u128;
        let tokens = (r.terms.sale_supply as u128 * c.amount as u128 / total) as u64;
        // Rounded down, so refunds never add up to more than what wasn't accepted.
        c.refund_owed = (c.amount as u128 * (r.total - r.accepted) as u128 / total) as u64;
        let id = ctx.accounts.distributor.allocation_count + 1;
        let (owner, account, start, cliff, duration) = (c.contributor, c.account, r.accepted_at, r.terms.vest_cliff, r.terms.vest_duration);
        if tokens > 0 {
            write_allocation(&mut ctx.accounts.distributor, &ctx.accounts.distributor_vault, &mut ctx.accounts.allocation, id, ctx.bumps.allocation, owner, account, tokens, start, cliff, duration)?;
        }
        emit!(Settled { raise: ctx.accounts.raise.key(), contributor: owner, tokens, refund: ctx.accounts.contribution.refund_owed });
        Ok(())
    }

    /// Pays a contributor what they're owed back, into their own token
    /// account: everything if the raise failed, else the part not accepted
    /// (once settled). Anyone.
    pub fn withdraw_refund(ctx: Context<WithdrawRefund>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.raise;
        let c = &mut ctx.accounts.contribution;
        let amount = if r.failed(now) {
            std::mem::take(&mut c.amount)
        } else {
            std::mem::take(&mut c.refund_owed)
        };
        require!(amount > 0, GovError::NothingToClaim);
        send_from_raise(r, &ctx.accounts.vault, &ctx.accounts.to, &ctx.accounts.currency, &ctx.accounts.token_program, amount)?;
        emit!(Refunded { raise: r.key(), contributor: c.contributor, amount });
        Ok(())
    }
}

// ---------------------------------------------------------------
// HELPERS
// ---------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_allocation(
    d: &mut Account<Distributor>,
    vault: &InterfaceAccount<TokenAccount>,
    a: &mut Account<Allocation>,
    id: u64,
    bump: u8,
    owner: Pubkey,
    account: [u8; 32],
    amount: u64,
    start: i64,
    cliff: i64,
    duration: i64,
) -> Result<()> {
    require!(!d.closed, GovError::DistributorClosed);
    require!(owner != Pubkey::default() || account != [0u8; 32], GovError::Unauthorized);
    require!(amount > 0 && cliff >= 0 && duration >= 0, GovError::InvalidAmount);
    require!(vault.amount.saturating_sub(d.outstanding) >= amount, GovError::NotFunded);
    d.outstanding += amount;
    d.allocation_count = id;
    a.distributor = d.key();
    a.id = id;
    a.owner = owner;
    a.account = account;
    a.total = amount;
    a.claimed = 0;
    a.start = start;
    a.cliff = cliff;
    a.duration = duration;
    a.bump = bump;
    emit!(Allocated { distributor: d.key(), allocation: a.key(), id, owner, account, amount, start, cliff, duration });
    Ok(())
}

impl Allocation {
    /// How much has vested by `now`.
    pub fn vested(&self, now: i64) -> u64 {
        if now < self.start + self.cliff {
            return 0;
        }
        if self.duration <= self.cliff || now >= self.start + self.duration {
            return self.total;
        }
        (self.total as u128 * (now - self.start) as u128 / self.duration as u128) as u64
    }
}

fn pay<'info>(
    d: &mut Account<'info, Distributor>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    a: &mut Account<'info, Allocation>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
) -> Result<()> {
    require!(!d.closed, GovError::DistributorClosed);
    let now = Clock::get()?.unix_timestamp;
    let amount = a.vested(now) - a.claimed;
    require!(amount > 0, GovError::NothingToClaim);
    a.claimed += amount;
    d.outstanding -= amount;
    send_from_distributor(d, vault, to, mint, token_program, amount)?;
    emit!(Claimed { allocation: a.key(), to: to.key(), amount });
    Ok(())
}

fn send_from_distributor<'info>(
    d: &Account<'info, Distributor>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let seeds: &[&[u8]] = &[DISTRIBUTOR_SEED, d.dao.as_ref(), d.mint.as_ref(), d.operator.as_ref(), &[d.bump]];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked { from: vault.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: d.to_account_info() },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

fn send_from_raise<'info>(
    r: &Account<'info, Raise>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    token_program: &Interface<'info, TokenInterface>,
    amount: u64,
) -> Result<()> {
    let seeds: &[&[u8]] = &[RAISE_SEED, r.dao.as_ref(), r.create_key.as_ref(), &[r.bump]];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked { from: vault.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: r.to_account_info() },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Distributor {
    pub dao: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    /// The bot's operator: claims for chat accounts.
    pub operator: Pubkey,
    pub token_program: Pubkey,
    /// Tokens promised and not yet claimed.
    pub outstanding: u64,
    pub allocation_count: u64,
    /// After this the DAO may close it and take everything back (0: never).
    pub claim_deadline: i64,
    pub closed: bool,
    pub welcome_amount: u64,
    pub welcome_cap: u64,
    pub welcome_granted: u64,
    pub bump: u8,
}

/// `[ALLOCATION_SEED, distributor, id]`.
#[account]
#[derive(InitSpace)]
pub struct Allocation {
    pub distributor: Pubkey,
    pub id: u64,
    /// The wallet that may claim (default: none).
    pub owner: Pubkey,
    /// The chat account the operator may claim for (zero: none).
    pub account: [u8; 32],
    pub total: u64,
    pub claimed: u64,
    pub start: i64,
    pub cliff: i64,
    pub duration: i64,
    pub bump: u8,
}

/// `[WELCOMED_SEED, distributor, key]`: one welcome grant per member.
#[account]
#[derive(InitSpace)]
pub struct Welcomed {
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct RaiseTerms {
    pub soft_cap: u64,
    /// The stated target; not a ceiling.
    pub hard_cap: u64,
    pub start: i64,
    pub end: i64,
    pub min_contribution: u64,
    /// Per contributor; 0: none.
    pub max_contribution: u64,
    /// Tokens shared among contributors, held by the Distributor.
    pub sale_supply: u64,
    pub vest_cliff: i64,
    pub vest_duration: i64,
    /// After `end`, how long the decider has to accept an amount.
    pub decision_window: i64,
}

#[account]
#[derive(InitSpace)]
pub struct Raise {
    pub dao: Pubkey,
    pub create_key: Pubkey,
    pub decider: Pubkey,
    pub currency: Pubkey,
    pub currency_token_program: Pubkey,
    pub vault: Pubkey,
    pub distributor: Pubkey,
    /// A prop firm's vault the money goes into; default: the treasury.
    pub prop_firm_vault: Pubkey,
    pub terms: RaiseTerms,
    pub total: u64,
    pub accepted: u64,
    pub accepted_at: i64,
    pub bump: u8,
}

impl Raise {
    /// Below the soft cap at the end, or nothing accepted within the window.
    pub fn failed(&self, now: i64) -> bool {
        self.accepted_at == 0 && now >= self.terms.end && (self.total < self.terms.soft_cap || now >= self.terms.end + self.terms.decision_window)
    }
}

/// `[CONTRIBUTION_SEED, raise, contributor]`.
#[account]
#[derive(InitSpace)]
pub struct Contribution {
    pub raise: Pubkey,
    pub contributor: Pubkey,
    pub account: [u8; 32],
    pub amount: u64,
    pub settled: bool,
    pub refund_owed: u64,
    pub bump: u8,
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
#[instruction(operator: Pubkey)]
pub struct CreateDistributor<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init, payer = payer, space = 8 + Distributor::INIT_SPACE,
        seeds = [DISTRIBUTOR_SEED, hub_dao.key().as_ref(), mint.key().as_ref(), operator.as_ref()], bump,
    )]
    pub distributor: Account<'info, Distributor>,
    #[account(
        init, payer = payer,
        seeds = [DISTRIBUTOR_VAULT_SEED, distributor.key().as_ref()], bump,
        token::mint = mint, token::authority = distributor, token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Allocate<'info> {
    /// The DAO's treasury: a passed proposal. It pays the rent.
    #[account(mut, seeds = [TREASURY_SEED, distributor.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub distributor: Account<'info, Distributor>,
    #[account(address = distributor.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = treasury, space = 8 + Allocation::INIT_SPACE,
        seeds = [ALLOCATION_SEED, distributor.key().as_ref(), &(distributor.allocation_count + 1).to_le_bytes()], bump,
    )]
    pub allocation: Account<'info, Allocation>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    pub signer: Signer<'info>,
    #[account(mut)]
    pub distributor: Account<'info, Distributor>,
    #[account(mut, address = distributor.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, has_one = distributor @ GovError::WrongPool)]
    pub allocation: Account<'info, Allocation>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(address = distributor.mint @ GovError::WrongMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(address = distributor.token_program @ GovError::WrongPool)]
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct DistributorAdmin<'info> {
    #[account(seeds = [TREASURY_SEED, distributor.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub distributor: Account<'info, Distributor>,
}

#[derive(Accounts)]
#[instruction(member: Pubkey, account: [u8; 32])]
pub struct GrantWelcome<'info> {
    #[account(mut)]
    pub approver: Signer<'info>,
    #[account(address = distributor.dao @ GovError::WrongDao)]
    pub hub_dao: Account<'info, HubDao>,
    /// The DAO's current Delegate governance.
    pub governance: Account<'info, vortex_delegate::Governance>,
    #[account(mut)]
    pub distributor: Account<'info, Distributor>,
    #[account(address = distributor.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = approver, space = 8 + Welcomed::INIT_SPACE,
        seeds = [WELCOMED_SEED, distributor.key().as_ref(), &welcome_key(&member, &account)], bump,
    )]
    pub welcomed: Account<'info, Welcomed>,
    #[account(
        init, payer = approver, space = 8 + Allocation::INIT_SPACE,
        seeds = [ALLOCATION_SEED, distributor.key().as_ref(), &(distributor.allocation_count + 1).to_le_bytes()], bump,
    )]
    pub allocation: Account<'info, Allocation>,
    pub system_program: Program<'info, System>,
}

/// One welcome per chat account, or per wallet when there's no account.
pub fn welcome_key(member: &Pubkey, account: &[u8; 32]) -> [u8; 32] {
    if *account != [0u8; 32] {
        *account
    } else {
        member.to_bytes()
    }
}

#[derive(Accounts)]
pub struct ReturnTokens<'info> {
    #[account(seeds = [TREASURY_SEED, distributor.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub distributor: Account<'info, Distributor>,
    #[account(mut, address = distributor.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = treasury, token::token_program = token_program)]
    pub treasury_token: InterfaceAccount<'info, TokenAccount>,
    #[account(address = distributor.mint @ GovError::WrongMint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(address = distributor.token_program @ GovError::WrongPool)]
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CreateRaise<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub create_key: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    pub distributor: Account<'info, Distributor>,
    pub currency: InterfaceAccount<'info, Mint>,
    #[account(
        init, payer = payer, space = 8 + Raise::INIT_SPACE,
        seeds = [RAISE_SEED, hub_dao.key().as_ref(), create_key.key().as_ref()], bump,
    )]
    pub raise: Account<'info, Raise>,
    #[account(
        init, payer = payer,
        seeds = [RAISE_VAULT_SEED, raise.key().as_ref()], bump,
        token::mint = currency, token::authority = raise, token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    /// A prop firm's vault, seeded by this raise.
    pub prop_firm_vault: Option<Box<Account<'info, vortex_vault::Vault>>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Contribute<'info> {
    #[account(mut)]
    pub contributor: Signer<'info>,
    #[account(mut)]
    pub raise: Account<'info, Raise>,
    #[account(
        init_if_needed, payer = contributor, space = 8 + Contribution::INIT_SPACE,
        seeds = [CONTRIBUTION_SEED, raise.key().as_ref(), contributor.key().as_ref()], bump,
    )]
    pub contribution: Account<'info, Contribution>,
    #[account(mut, token::mint = currency, token::authority = contributor, token::token_program = token_program)]
    pub from: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = raise.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(address = raise.currency @ GovError::WrongMint)]
    pub currency: InterfaceAccount<'info, Mint>,
    #[account(address = raise.currency_token_program @ GovError::WrongPool)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Accept<'info> {
    pub decider: Signer<'info>,
    #[account(mut)]
    pub raise: Box<Account<'info, Raise>>,
    #[account(mut, address = raise.vault @ GovError::WrongPool)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// The DAO's treasury: where the money goes, and nowhere else.
    /// CHECK: the hub's treasury PDA for the raise's DAO (seeds checked).
    #[account(seeds = [TREASURY_SEED, raise.dao.as_ref()], bump, seeds::program = vortex_hub::ID)]
    pub treasury: UncheckedAccount<'info>,
    /// Where the money goes, unless the raise is a prop firm's.
    #[account(mut, token::mint = currency, token::authority = treasury, token::token_program = token_program)]
    pub treasury_token: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    #[account(address = raise.distributor @ GovError::WrongPool)]
    pub distributor: Box<Account<'info, Distributor>>,
    #[account(mut, address = distributor.vault @ GovError::WrongPool)]
    pub distributor_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    // A prop firm's vault, which takes the money and mints the shares sold.
    #[account(mut)]
    pub prop_firm_vault: Option<Box<Account<'info, vortex_vault::Vault>>>,
    #[account(mut)]
    pub vault_assets: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    #[account(mut)]
    pub share_mint: Option<Box<InterfaceAccount<'info, Mint>>>,
    pub share_token_program: Option<Interface<'info, TokenInterface>>,
    pub vault_program: Option<Program<'info, vortex_vault::program::VortexVault>>,
    #[account(address = raise.currency @ GovError::WrongMint)]
    pub currency: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = raise.currency_token_program @ GovError::WrongPool)]
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub raise: Account<'info, Raise>,
    #[account(mut, has_one = raise @ GovError::WrongPool)]
    pub contribution: Account<'info, Contribution>,
    #[account(mut, address = raise.distributor @ GovError::WrongPool)]
    pub distributor: Account<'info, Distributor>,
    #[account(address = distributor.vault @ GovError::WrongPool)]
    pub distributor_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = payer, space = 8 + Allocation::INIT_SPACE,
        seeds = [ALLOCATION_SEED, distributor.key().as_ref(), &(distributor.allocation_count + 1).to_le_bytes()], bump,
    )]
    pub allocation: Account<'info, Allocation>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawRefund<'info> {
    pub raise: Account<'info, Raise>,
    #[account(mut, has_one = raise @ GovError::WrongPool)]
    pub contribution: Account<'info, Contribution>,
    #[account(mut, address = raise.vault @ GovError::WrongPool)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    /// The contributor's own token account.
    #[account(mut, token::mint = currency, token::authority = contribution.contributor, token::token_program = token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    #[account(address = raise.currency @ GovError::WrongMint)]
    pub currency: InterfaceAccount<'info, Mint>,
    #[account(address = raise.currency_token_program @ GovError::WrongPool)]
    pub token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct DistributorCreated {
    pub dao: Pubkey,
    pub distributor: Pubkey,
    pub mint: Pubkey,
    pub operator: Pubkey,
}

#[event]
pub struct Allocated {
    pub distributor: Pubkey,
    pub allocation: Pubkey,
    pub id: u64,
    pub owner: Pubkey,
    pub account: [u8; 32],
    pub amount: u64,
    pub start: i64,
    pub cliff: i64,
    pub duration: i64,
}

#[event]
pub struct Claimed {
    pub allocation: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct WelcomeTermsSet {
    pub distributor: Pubkey,
    pub amount: u64,
    pub cap: u64,
}

#[event]
pub struct WelcomeGranted {
    pub distributor: Pubkey,
    pub member: Pubkey,
    pub account: [u8; 32],
    pub approver: Pubkey,
}

#[event]
pub struct RaiseCreated {
    pub dao: Pubkey,
    pub raise: Pubkey,
    pub decider: Pubkey,
    pub currency: Pubkey,
    pub terms: RaiseTerms,
}

#[event]
pub struct Contributed {
    pub raise: Pubkey,
    pub contributor: Pubkey,
    pub account: [u8; 32],
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct RaiseAccepted {
    pub raise: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct Settled {
    pub raise: Pubkey,
    pub contributor: Pubkey,
    pub tokens: u64,
    pub refund: u64,
}

#[event]
pub struct Refunded {
    pub raise: Pubkey,
    pub contributor: Pubkey,
    pub amount: u64,
}
