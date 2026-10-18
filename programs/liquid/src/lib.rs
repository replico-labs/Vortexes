//! Vortexes liquid governance, the Solana counterpart of the EVM
//! `LiquidGovernance.sol`: token-weighted voting where members can hand
//! their votes to a delegate, and still vote themselves on any proposal.
//!
//! - Members deposit the DAO's token into its vault; one token, one vote.
//!   Proposing, quorum, approval, timelock and execution work as in
//!   token-weighted.
//! - **Delegating** points a member's deposit at a delegate. The delegate's
//!   account keeps a running total of everything delegated to it, so a
//!   delegate's vote counts their own deposit plus that total straight away.
//!   Nobody has to come back afterwards to add delegators' votes one by one
//!   (the EVM version needs a keeper for that).
//! - **Overriding:** a member who delegated can still vote on any proposal
//!   (`override_vote`). If their delegate already voted with their tokens,
//!   that weight moves from the delegate's choice to theirs; if not, it's
//!   set aside so the delegate's later vote doesn't count it.
//! - **One hop.** A delegate can't delegate, and a member others delegate to
//!   can't either, so every vote costs the same however many people
//!   delegated. (The EVM version follows chains; on Solana a chain can't be
//!   walked inside one transaction.)
//! - **No double counting** without snapshots: deposits stay locked until
//!   the votes they were counted in close. A delegator's deposit is also
//!   locked while a vote their delegate cast with it is open: until then
//!   they can't withdraw, add to it, or undelegate. They can always
//!   override.
//!
//! Ordering is by a counter on the DAO's governance account (`seq`), bumped
//! by every delegation and every vote: a delegate's vote includes a
//! delegator's tokens exactly when it came after the delegation.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execution_expired, has_approval, has_quorum, passed, proposal_state, timelock_complete, validate_instructions,
    validate_uri, GovError, Lifecycle, ProposalCore, ProposalState, StoredInstruction, Tally, VotingConfig,
    GOVERNANCE_SEED,
};
use vortex_hub::Dao as HubDao;

declare_id!("CMf1cxiDNmEasmM9VQwJSz5p4RVw1A5GZUjTEy95du1W");

pub const VAULT_SEED: &[u8] = b"vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const VOTE_SEED: &[u8] = b"vote";
pub const OVERRIDE_SEED: &[u8] = b"override";

#[program]
pub mod vortex_liquid {
    use super::*;

    /// Sets up a hub DAO in this program: its rules, token and vault. The
    /// creator right after `vortex_hub::create_dao`, or the DAO's treasury
    /// (inside a passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, config: VotingConfig) -> Result<()> {
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

        let g = &mut ctx.accounts.governance;
        g.hub_dao = hub_dao_key;
        g.treasury = treasury;
        g.executor = vortex_hub::executor_address(&hub_dao_key);
        g.mint = ctx.accounts.mint.key();
        g.vault = ctx.accounts.vault.key();
        g.config = config;
        g.proposal_count = 0;
        g.total_deposited = 0;
        g.seq = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint });
        Ok(())
    }

    /// Moves `amount` of the DAO's token into the vault. A delegator's new
    /// tokens go to their delegate's total too (pass `delegate_voter`), and
    /// can't be added while the delegate's vote with the old amount is open.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        if voter.owner == Pubkey::default() {
            voter.governance = ctx.accounts.governance.key();
            voter.owner = ctx.accounts.owner.key();
            voter.bump = ctx.bumps.voter;
        }
        if voter.delegate != Pubkey::default() {
            // Not while a vote counting the old amount is open (theirs or the delegate's).
            require!(now >= voter.locked_until, GovError::TokensLocked);
            let d = delegate_of(voter, ctx.accounts.delegate_voter.as_deref_mut())?;
            require!(now >= d.power_locked_until, GovError::DelegationLocked);
            d.delegated_power = d.delegated_power.checked_add(amount).ok_or(GovError::Overflow)?;
        }
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        let voter = &mut ctx.accounts.voter;
        voter.amount = voter.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        let g = &mut ctx.accounts.governance;
        g.total_deposited = g.total_deposited.checked_add(amount).ok_or(GovError::Overflow)?;
        emit!(Deposited { governance: g.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Takes `amount` back out, once the votes it was counted in (the
    /// member's own, and their delegate's) have closed.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(now >= voter.locked_until, GovError::TokensLocked);
        require!(amount <= voter.amount, GovError::InsufficientDeposit);
        if voter.delegate != Pubkey::default() {
            let d = delegate_of(voter, ctx.accounts.delegate_voter.as_deref_mut())?;
            require!(now >= d.power_locked_until, GovError::DelegationLocked);
            d.delegated_power -= amount;
        }
        voter.amount -= amount;
        let g = &mut ctx.accounts.governance;
        g.total_deposited -= amount;

        let hub_dao = g.hub_dao;
        let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub_dao.as_ref(), &[g.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.owner_token_account.to_account_info(),
                    authority: g.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        emit!(Withdrawn { governance: g.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Hands the member's votes to `to`. Not while a vote of their own is
    /// open (it would count twice), and only one hop: `to` can't have
    /// delegated, and nobody can be delegating to the member.
    pub fn delegate(ctx: Context<Delegate>, to: Pubkey) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let owner = ctx.accounts.owner.key();
        require_keys_neq!(to, owner, GovError::SelfDelegation);
        require!(to != Pubkey::default(), GovError::InvalidConfig);
        let g = &mut ctx.accounts.governance;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate == Pubkey::default(), GovError::AlreadyDelegated);
        require!(voter.delegators == 0, GovError::HasDelegators);
        require!(now >= voter.locked_until, GovError::TokensLocked);

        let d = &mut ctx.accounts.delegate_voter;
        if d.owner == Pubkey::default() {
            d.governance = g.key();
            d.owner = to;
            d.bump = ctx.bumps.delegate_voter;
        }
        require!(d.delegate == Pubkey::default(), GovError::DelegateHasDelegated);
        d.delegated_power = d.delegated_power.checked_add(voter.amount).ok_or(GovError::Overflow)?;
        d.delegators += 1;
        g.seq += 1;
        voter.delegate = to;
        voter.delegated_seq = g.seq;
        emit!(DelegateChanged { governance: g.key(), member: owner, delegate: to, amount: voter.amount });
        Ok(())
    }

    /// Takes the member's votes back, once every vote their delegate cast
    /// with them has closed.
    pub fn undelegate(ctx: Context<Undelegate>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate != Pubkey::default(), GovError::NotDelegated);
        // An override of theirs is still open: the delegate's total has to keep matching it.
        require!(now >= voter.locked_until, GovError::TokensLocked);
        let d = delegate_of(voter, Some(&mut *ctx.accounts.delegate_voter))?;
        require!(now >= d.power_locked_until, GovError::DelegationLocked);
        d.delegated_power -= voter.amount;
        d.delegators -= 1;
        voter.delegate = Pubkey::default();
        emit!(DelegateChanged { governance: voter.governance, member: voter.owner, delegate: Pubkey::default(), amount: voter.amount });
        Ok(())
    }

    /// Proposes `instructions` for the treasury to run if the vote passes.
    /// `id` must be the next proposal number. The proposer's voting power
    /// (own deposit plus what's delegated to them) must meet the threshold.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        let power = ctx.accounts.voter.as_ref().map_or(0, |v| if v.delegate == Pubkey::default() { v.amount.saturating_add(v.delegated_power) } else { 0 });
        require!(power >= g.config.proposal_threshold, GovError::ProposalThresholdNotMet);

        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions };
        p.governance = g.key();
        p.id = id;
        p.proposer = ctx.accounts.proposer.key();
        p.metadata_uri = metadata_uri;
        p.created_at = now;
        p.voting_starts_at = now + g.config.voting_delay as i64;
        p.voting_ends_at = p.voting_starts_at + g.config.voting_period as i64;
        p.quorum_base = g.total_deposited;
        p.tally = Tally::default();
        p.queued_at = 0;
        p.executed = false;
        p.cancelled = false;
        p.bump = ctx.bumps.proposal;
        emit!(ProposalCreated {
            hub_dao: g.hub_dao,
            proposal: p.key(),
            id,
            proposer: p.proposer,
            metadata_uri: p.metadata_uri.clone(),
            voting_starts_at: p.voting_starts_at,
            voting_ends_at: p.voting_ends_at,
        });
        Ok(())
    }

    /// A member who hasn't delegated votes with their deposit plus
    /// everything delegated to them, minus what delegators already voted
    /// themselves on this proposal (`overrides`, always this proposal's and
    /// this member's, so it can't be left out).
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        open_for_votes(p, now)?;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate == Pubkey::default(), GovError::AlreadyDelegated);
        let overridden = read_override(&ctx.accounts.overrides)?;
        let delegated = voter.delegated_power.saturating_sub(overridden);
        let weight = voter.amount.checked_add(delegated).ok_or(GovError::Overflow)?;
        require!(weight > 0, GovError::NoVotingPower);
        add_vote(&mut p.tally, choice, weight)?;
        voter.locked_until = voter.locked_until.max(p.voting_ends_at);
        if voter.delegated_power > 0 {
            voter.power_locked_until = voter.power_locked_until.max(p.voting_ends_at);
        }

        let g = &mut ctx.accounts.governance;
        g.seq += 1;
        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = voter.owner;
        record.choice = choice;
        record.weight = weight;
        record.delegated = delegated;
        record.seq = g.seq;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: voter.owner, choice, weight, delegated });
        Ok(())
    }

    /// A member who delegated votes themselves on this proposal. If their
    /// delegate already voted with their tokens, that weight moves from the
    /// delegate's choice to this one; otherwise it's set aside so the
    /// delegate's later vote leaves it out.
    pub fn override_vote(ctx: Context<OverrideVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        open_for_votes(p, now)?;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate != Pubkey::default(), GovError::NotDelegated);
        require_keys_eq!(ctx.accounts.delegate_voter.owner, voter.delegate, GovError::WrongDelegate);
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);

        let delegate_record = &ctx.accounts.delegate_record;
        let mut moved = false;
        if delegate_record.owner == &crate::ID && !delegate_record.data_is_empty() {
            let mut rec = VoteRecord::try_deserialize(&mut &delegate_record.try_borrow_data()?[..])?;
            // The delegate's vote included this member's tokens only if it came after the delegation.
            if rec.seq > voter.delegated_seq {
                sub_vote(&mut p.tally, rec.choice, weight)?;
                rec.weight = rec.weight.saturating_sub(weight);
                rec.delegated = rec.delegated.saturating_sub(weight);
                rec.try_serialize(&mut &mut delegate_record.try_borrow_mut_data()?[..])?;
                moved = true;
            }
        } else {
            let o = &mut ctx.accounts.overrides;
            if o.proposal == Pubkey::default() {
                o.proposal = p.key();
                o.delegate = voter.delegate;
                o.bump = ctx.bumps.overrides;
            }
            o.power = o.power.checked_add(weight).ok_or(GovError::Overflow)?;
        }
        add_vote(&mut p.tally, choice, weight)?;
        voter.locked_until = voter.locked_until.max(p.voting_ends_at);

        let g = &mut ctx.accounts.governance;
        g.seq += 1;
        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = voter.owner;
        record.choice = choice;
        record.weight = weight;
        record.delegated = 0;
        record.seq = g.seq;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteOverridden { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: voter.owner, delegate: voter.delegate, choice, weight, moved });
        Ok(())
    }

    /// Queues a proposal that passed. Anyone can call it once voting ends.
    pub fn queue(ctx: Context<Queue>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.queued_at == 0, GovError::ProposalAlreadyQueued);
        require!(now >= p.voting_ends_at, GovError::VotingNotEnded);
        require!(has_quorum(&p.tally, p.quorum_base, &config), GovError::QuorumNotReached);
        require!(has_approval(&p.tally, &config), GovError::ApprovalThresholdNotMet);
        p.queued_at = now;
        emit!(ProposalQueued { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, executable_at: now + config.timelock as i64 });
        Ok(())
    }

    /// The hub's question "can this proposal run now?" (see vortex-core's
    /// CONFIRM_EXECUTION). Only the hub's executor PDA for this DAO can ask.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let life = p.lifecycle();
        require!(timelock_complete(&life, &config, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &config, now), GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed: its proposer, or the DAO
    /// itself through a proposal.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let who = ctx.accounts.authority.key();
        require!(who == p.proposer || who == g.treasury, GovError::Unauthorized);
        p.cancelled = true;
        emit!(ProposalCancelled { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, by: who });
        Ok(())
    }

    /// Replaces the voting rules (only through a passed proposal).
    pub fn update_config(ctx: Context<UpdateConfig>, config: VotingConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }

    /// Returns a vote record's rent to its voter once voting has closed.
    pub fn close_vote_record(ctx: Context<CloseVoteRecord>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &ctx.accounts.proposal;
        require!(now >= p.voting_ends_at || p.cancelled, GovError::VotingNotEnded);
        Ok(())
    }
}

// ---------------------------------------------------------------
// HELPERS
// ---------------------------------------------------------------

fn open_for_votes(p: &Proposal, now: i64) -> Result<()> {
    require!(!p.cancelled && !p.executed, GovError::ProposalNotActive);
    require!(now >= p.voting_starts_at && now < p.voting_ends_at, GovError::ProposalNotActive);
    Ok(())
}

fn add_vote(t: &mut Tally, choice: VoteChoice, weight: u64) -> Result<()> {
    let total = match choice {
        VoteChoice::For => &mut t.for_votes,
        VoteChoice::Against => &mut t.against_votes,
        VoteChoice::Abstain => &mut t.abstain_votes,
    };
    *total = total.checked_add(weight).ok_or(GovError::Overflow)?;
    Ok(())
}

fn sub_vote(t: &mut Tally, choice: VoteChoice, weight: u64) -> Result<()> {
    let total = match choice {
        VoteChoice::For => &mut t.for_votes,
        VoteChoice::Against => &mut t.against_votes,
        VoteChoice::Abstain => &mut t.abstain_votes,
    };
    *total = total.checked_sub(weight).ok_or(GovError::Overflow)?;
    Ok(())
}

/// The delegate's account, which must be passed (and be the right one) when the member has delegated.
fn delegate_of<'a>(voter: &Voter, d: Option<&'a mut Voter>) -> Result<&'a mut Voter> {
    let d = d.ok_or(GovError::WrongDelegate)?;
    require_keys_eq!(d.owner, voter.delegate, GovError::WrongDelegate);
    require_keys_eq!(d.governance, voter.governance, GovError::WrongDao);
    Ok(d)
}

/// Power delegators set aside on this proposal before the delegate voted (0 if none).
fn read_override(account: &UncheckedAccount) -> Result<u64> {
    if account.owner != &crate::ID || account.data_is_empty() {
        return Ok(0);
    }
    Ok(Override::try_deserialize(&mut &account.try_borrow_data()?[..])?.power)
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub config: VotingConfig,
    pub proposal_count: u64,
    pub total_deposited: u64,
    /// Bumped by every delegation and vote, to order them.
    pub seq: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub governance: Pubkey,
    pub owner: Pubkey,
    /// Deposited tokens (raw).
    pub amount: u64,
    /// Can't withdraw or delegate before this: the end of the latest vote they cast.
    pub locked_until: i64,
    /// Who they've delegated to (default = nobody).
    pub delegate: Pubkey,
    /// `seq` when they last delegated.
    pub delegated_seq: u64,
    /// Sum of the deposits delegated to them.
    pub delegated_power: u64,
    pub delegators: u32,
    /// Their delegators' deposits are locked until this: the end of the latest vote they cast with them.
    pub power_locked_until: i64,
    pub bump: u8,
}

#[account]
pub struct Proposal {
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub created_at: i64,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
    /// All deposits when proposed: what quorum is measured against.
    pub quorum_base: u64,
    pub tally: Tally,
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
    pub bump: u8,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + 8 * 3 + 8 + Tally::INIT_SPACE + 8 + 1 + 1 + 1
    }

    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle {
            voting_starts_at: self.voting_starts_at,
            voting_ends_at: self.voting_ends_at,
            queued_at: self.queued_at,
            executed: self.executed,
            cancelled: self.cancelled,
        }
    }

    pub fn state(&self, config: &VotingConfig, now: i64) -> ProposalState {
        proposal_state(&self.lifecycle(), config, passed(&self.tally, self.quorum_base, config), now)
    }
}

#[account]
#[derive(InitSpace)]
pub struct VoteRecord {
    pub proposal: Pubkey,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    /// Everything this vote counts (own + delegated, less overrides since).
    pub weight: u64,
    /// The delegated part of `weight`.
    pub delegated: u64,
    /// Governance `seq` when cast.
    pub seq: u64,
    pub bump: u8,
}

/// Power delegators voted themselves on a proposal before their delegate did.
#[account]
#[derive(InitSpace)]
pub struct Override {
    pub proposal: Pubkey,
    pub delegate: Pubkey,
    pub power: u64,
    pub bump: u8,
}

/// Same order as the EVM contracts' VoteType.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum VoteChoice {
    Against,
    For,
    Abstain,
}

// ---------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
pub struct InitGovernance<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(init, payer = payer, space = 8 + Governance::INIT_SPACE, seeds = [GOVERNANCE_SEED, hub_dao.key().as_ref()], bump)]
    pub governance: Account<'info, Governance>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, governance.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = governance,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = mint @ GovError::WrongDao, has_one = vault @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Voter::INIT_SPACE,
        seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub voter: Account<'info, Voter>,
    /// The member's delegate, when they've delegated.
    #[account(mut)]
    pub delegate_voter: Option<Account<'info, Voter>>,
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
    #[account(mut, has_one = mint @ GovError::WrongDao, has_one = vault @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    /// The member's delegate, when they've delegated.
    #[account(mut)]
    pub delegate_voter: Option<Account<'info, Voter>>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(to: Pubkey)]
pub struct Delegate<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    /// The delegate's account, made here if they've never deposited.
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Voter::INIT_SPACE,
        seeds = [VOTER_SEED, governance.key().as_ref(), to.as_ref()],
        bump,
    )]
    pub delegate_voter: Account<'info, Voter>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Undelegate<'info> {
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), voter.delegate.as_ref()], bump = delegate_voter.bump)]
    pub delegate_voter: Account<'info, Voter>,
}

#[derive(Accounts)]
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), proposer.key().as_ref()], bump)]
    pub voter: Option<Account<'info, Voter>>,
    #[account(
        init,
        payer = proposer,
        space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()],
        bump,
    )]
    pub proposal: Account<'info, Proposal>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CastVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    /// What delegators set aside on this proposal (may not exist yet).
    /// CHECK: address fixed by the seeds; read only if this program owns it.
    #[account(seeds = [OVERRIDE_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump)]
    pub overrides: UncheckedAccount<'info>,
    #[account(
        init,
        payer = owner,
        space = 8 + VoteRecord::INIT_SPACE,
        seeds = [VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub vote_record: Account<'info, VoteRecord>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OverrideVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), voter.delegate.as_ref()], bump = delegate_voter.bump)]
    pub delegate_voter: Account<'info, Voter>,
    /// The delegate's vote on this proposal, if they've voted.
    /// CHECK: address fixed by the seeds; read and updated only if this program owns it.
    #[account(mut, seeds = [VOTE_SEED, proposal.key().as_ref(), voter.delegate.as_ref()], bump)]
    pub delegate_record: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Override::INIT_SPACE,
        seeds = [OVERRIDE_SEED, proposal.key().as_ref(), voter.delegate.as_ref()],
        bump,
    )]
    pub overrides: Account<'info, Override>,
    #[account(
        init,
        payer = owner,
        space = 8 + VoteRecord::INIT_SPACE,
        seeds = [VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub vote_record: Account<'info, VoteRecord>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Queue<'info> {
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

/// Account order fixed by vortex-core's CONFIRM_EXECUTION interface.
#[derive(Accounts)]
pub struct ConfirmExecution<'info> {
    #[account(address = governance.executor @ GovError::Unauthorized)]
    pub executor: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct Cancel<'info> {
    pub authority: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
}

#[derive(Accounts)]
pub struct CloseVoteRecord<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub proposal: Account<'info, Proposal>,
    #[account(
        mut,
        close = owner,
        has_one = proposal @ GovError::WrongDao,
        constraint = vote_record.voter == owner.key() @ GovError::Unauthorized,
    )]
    pub vote_record: Account<'info, VoteRecord>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct GovernanceInitialized {
    pub hub_dao: Pubkey,
    pub governance: Pubkey,
    pub mint: Pubkey,
}

#[event]
pub struct Deposited {
    pub governance: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct Withdrawn {
    pub governance: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct DelegateChanged {
    pub governance: Pubkey,
    pub member: Pubkey,
    /// Default when they took their votes back.
    pub delegate: Pubkey,
    pub amount: u64,
}

#[event]
pub struct ProposalCreated {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
}

#[event]
pub struct VoteCast {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
    /// The part of `weight` delegated to the voter.
    pub delegated: u64,
}

#[event]
pub struct VoteOverridden {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub delegate: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
    /// True when the delegate had already voted with these tokens and the weight moved.
    pub moved: bool,
}

#[event]
pub struct ProposalQueued {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub executable_at: i64,
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
    pub config: VotingConfig,
}
