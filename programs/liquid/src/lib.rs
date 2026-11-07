//! Vortexes liquid governance, the Solana counterpart of the EVM
//! `LiquidGovernance.sol`: token-weighted voting where members can hand
//! their votes to a delegate, and still vote themselves on any proposal.
//!
//! - Members stake the DAO's token in its pool (vortex-stake), not here;
//!   one token, one vote. Proposing, quorum, approval, timelock and
//!   execution work as in token-weighted, and each proposal keeps the pool
//!   it was made in.
//! - **Delegating** hands a member's unlocked stake to a delegate. The stake
//!   is locked in the pool for this governance while delegated (the member
//!   and this governance both sign the lock), so the delegate's running
//!   total of everything delegated to it stays right, and a delegate's vote
//!   counts their own stake plus that total straight away. Nobody has to
//!   come back afterwards to add delegators' votes one by one (the EVM
//!   version needs a keeper for that).
//! - **Overriding:** a member who delegated can still vote on any proposal
//!   (`override_vote`), with all their stake. If their delegate already
//!   voted with their tokens, that weight moves from the delegate's choice
//!   to theirs; if not, it's set aside so the delegate's later vote doesn't
//!   count it.
//! - **One hop.** A delegate can't delegate, and a member others delegate to
//!   can't either, so every vote costs the same however many people
//!   delegated. (The EVM version follows chains; on Solana a chain can't be
//!   walked inside one transaction.)
//! - **No double counting** without snapshots: a vote locks the voter's
//!   stake until it closes, and a delegator can't undelegate while a vote
//!   their delegate cast with it is open.
//! - **Delegations belong to a pool.** After the DAO changes its token they
//!   only count on proposals made in the old pool; members delegate again
//!   in the new one.
//!
//! Ordering is by a counter on the DAO's governance account (`seq`), bumped
//! by every delegation and every vote: a delegate's vote includes a
//! delegator's tokens exactly when it came after the delegation.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use vortex_core::{
    execution_expired, has_approval, has_quorum, passed, proposal_state, timelock_complete, validate_instructions,
    validate_uri, GovError, Lifecycle, ProposalCore, ProposalState, StoredInstruction, Tally, VotingConfig,
    GOVERNANCE_SEED, STAKE_SEED,
};
use vortex_hub::{Dao as HubDao, DaoToken};
use vortex_stake::{LockRecord, Pool, Stake};

declare_id!("CMf1cxiDNmEasmM9VQwJSz5p4RVw1A5GZUjTEy95du1W");

pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const VOTE_SEED: &[u8] = b"vote";
pub const OVERRIDE_SEED: &[u8] = b"override";

#[program]
pub mod vortex_liquid {
    use super::*;

    /// Sets up a hub DAO in this program: its rules. The DAO must have a
    /// token on record (members stake it in the hub's pool). The creator
    /// right after `vortex_hub::create_dao`, or the DAO's treasury (inside a
    /// passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, epoch: u32, config: VotingConfig) -> Result<()> {
        config.validate()?;
        let hub_dao = &ctx.accounts.hub_dao;
        let hub_dao_key = hub_dao.key();
        let treasury = vortex_hub::treasury_address(&hub_dao_key);
        let who = ctx.accounts.authority.key();
        // Epoch 1 by the creator for the DAO's first governance; the next
        // epoch by the treasury (a passed proposal) for a pending switch here.
        hub_dao.check_setup(&hub_dao_key, &crate::ID, &who, epoch)?;

        let g = &mut ctx.accounts.governance;
        g.hub_dao = hub_dao_key;
        g.treasury = treasury;
        g.executor = vortex_hub::executor_address(&hub_dao_key);
        g.config = config;
        g.proposal_count = 0;
        g.seq = 0;
        g.epoch = epoch;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: ctx.accounts.dao_token.mint });
        Ok(())
    }

    /// Hands the member's unlocked stake in the DAO's current pool to `to`,
    /// locking it there for this governance. Not while a vote of their own
    /// is open (it would count twice), and only one hop: `to` can't have
    /// delegated, and nobody can be delegating to the member.
    pub fn delegate(ctx: Context<Delegate>, to: Pubkey) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let owner = ctx.accounts.owner.key();
        require_keys_neq!(to, owner, GovError::SelfDelegation);
        require!(to != Pubkey::default(), GovError::InvalidConfig);
        let pool = ctx.accounts.pool.key();
        let g = &mut ctx.accounts.governance;
        let voter = &mut ctx.accounts.voter;
        init_voter(voter, g.key(), owner, ctx.bumps.voter);
        require!(voter.delegate == Pubkey::default(), GovError::AlreadyDelegated);
        require!(voter.delegators == 0, GovError::HasDelegators);
        require!(now >= voter.locked_until, GovError::TokensLocked);
        let s = &ctx.accounts.stake;
        let amount = s.amount - s.locked;
        require!(amount > 0, GovError::NoVotingPower);

        let d = &mut ctx.accounts.delegate_voter;
        init_voter(d, g.key(), to, ctx.bumps.delegate_voter);
        require!(d.delegate == Pubkey::default(), GovError::DelegateHasDelegated);
        // A delegate's total is in one pool: after a token change it starts again.
        require!(d.delegated_power == 0 || d.power_pool == pool, GovError::WrongPool);
        d.power_pool = pool;
        d.delegated_power = d.delegated_power.checked_add(amount).ok_or(GovError::Overflow)?;
        d.delegators += 1;
        g.seq += 1;
        voter.delegate = to;
        voter.pool = pool;
        voter.delegated_amount = amount;
        voter.delegated_seq = g.seq;

        let g = &ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let epoch_bytes = g.epoch.to_le_bytes();
        let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub_dao.as_ref(), &epoch_bytes, &[g.bump]];
        vortex_stake::cpi::lock(
            CpiContext::new_with_signer(
                ctx.accounts.stake_program.key(),
                vortex_stake::cpi::accounts::Lock {
                    owner: ctx.accounts.owner.to_account_info(),
                    governance: g.to_account_info(),
                    hub_dao: ctx.accounts.hub_dao.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    stake: ctx.accounts.stake.to_account_info(),
                    lock: ctx.accounts.lock.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        emit!(DelegateChanged { governance: g.key(), member: owner, delegate: to, amount });
        Ok(())
    }

    /// Takes the member's votes back and unlocks their stake, once every
    /// vote their delegate cast with them has closed. Works after the DAO
    /// has switched away from this governance, so nobody's stake is stuck.
    pub fn undelegate(ctx: Context<Undelegate>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate != Pubkey::default(), GovError::NotDelegated);
        // An override of theirs is still open: the delegate's total has to keep matching it.
        require!(now >= voter.locked_until, GovError::TokensLocked);
        let d = &mut ctx.accounts.delegate_voter;
        require!(now >= d.power_locked_until, GovError::DelegationLocked);
        let amount = voter.delegated_amount;
        d.delegated_power -= amount;
        d.delegators -= 1;
        voter.delegate = Pubkey::default();
        voter.delegated_amount = 0;

        let g = &ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let epoch_bytes = g.epoch.to_le_bytes();
        let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub_dao.as_ref(), &epoch_bytes, &[g.bump]];
        vortex_stake::cpi::unlock(
            CpiContext::new_with_signer(
                ctx.accounts.stake_program.key(),
                vortex_stake::cpi::accounts::Unlock {
                    governance: g.to_account_info(),
                    stake: ctx.accounts.stake.to_account_info(),
                    lock: ctx.accounts.lock.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        emit!(DelegateChanged { governance: voter.governance, member: voter.owner, delegate: Pubkey::default(), amount });
        Ok(())
    }

    /// Proposes `instructions` for the treasury to run if the vote passes.
    /// `id` must be the next proposal number. The proposer's voting power
    /// (own stake plus what's delegated to them in the current pool) must
    /// meet the threshold.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let pool = &ctx.accounts.pool;
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        let own = ctx.accounts.proposer_stake.as_ref().map_or(0, |s| s.amount);
        let power = match ctx.accounts.voter.as_ref() {
            Some(v) if v.delegate != Pubkey::default() => 0,
            Some(v) if v.power_pool == pool.key() => own.saturating_add(v.delegated_power),
            _ => own,
        };
        require!(power >= g.config.proposal_threshold, GovError::ProposalThresholdNotMet);

        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions };
        p.governance = g.key();
        p.id = id;
        p.proposer = ctx.accounts.proposer.key();
        p.metadata_uri = metadata_uri;
        p.pool = pool.key();
        p.created_at = now;
        p.voting_starts_at = now + g.config.voting_delay as i64;
        p.voting_ends_at = p.voting_starts_at + g.config.voting_period as i64;
        p.quorum_base = pool.total;
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

    /// A member who hasn't delegated votes with their stake in the
    /// proposal's pool plus everything delegated to them there, minus what
    /// delegators already voted themselves on this proposal (`overrides`,
    /// always this proposal's and this member's, so it can't be left out).
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        open_for_votes(p, now)?;
        let owner = ctx.accounts.owner.key();
        let g_key = ctx.accounts.governance.key();
        let voter = &mut ctx.accounts.voter;
        init_voter(voter, g_key, owner, ctx.bumps.voter);
        require!(voter.delegate == Pubkey::default(), GovError::AlreadyDelegated);
        let overridden = read_override(&ctx.accounts.overrides)?;
        let power = if voter.power_pool == p.pool { voter.delegated_power } else { 0 };
        let delegated = power.saturating_sub(overridden);
        let weight = ctx.accounts.stake.amount.checked_add(delegated).ok_or(GovError::Overflow)?;
        require!(weight > 0, GovError::NoVotingPower);
        add_vote(&mut p.tally, choice, weight)?;
        voter.locked_until = voter.locked_until.max(p.voting_ends_at);
        if power > 0 {
            voter.power_locked_until = voter.power_locked_until.max(p.voting_ends_at);
        }
        let ends = p.voting_ends_at;
        lock_stake_until(&ctx.accounts.governance, &ctx.accounts.owner, &ctx.accounts.hub_dao, &ctx.accounts.pool, &ctx.accounts.stake, &ctx.accounts.stake_program, ends)?;

        let g = &mut ctx.accounts.governance;
        g.seq += 1;
        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = owner;
        record.choice = choice;
        record.weight = weight;
        record.delegated = delegated;
        record.seq = g.seq;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: owner, choice, weight, delegated });
        Ok(())
    }

    /// A member who delegated votes themselves on this proposal, with all
    /// their stake in its pool. If their delegate already voted with their
    /// delegated tokens, that weight moves from the delegate's choice;
    /// otherwise it's set aside so the delegate's later vote leaves it out.
    pub fn override_vote(ctx: Context<OverrideVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        open_for_votes(p, now)?;
        let voter = &mut ctx.accounts.voter;
        require!(voter.delegate != Pubkey::default(), GovError::NotDelegated);
        require_keys_eq!(ctx.accounts.delegate_voter.owner, voter.delegate, GovError::WrongDelegate);
        let weight = ctx.accounts.stake.amount;
        require!(weight > 0, GovError::NoVotingPower);
        // What the delegate counts of theirs on this proposal: only delegations in its pool.
        let handed = if voter.pool == p.pool { voter.delegated_amount } else { 0 };

        let delegate_record = &ctx.accounts.delegate_record;
        let mut moved = false;
        if handed > 0 {
            if delegate_record.owner == &crate::ID && !delegate_record.data_is_empty() {
                let mut rec = VoteRecord::try_deserialize(&mut &delegate_record.try_borrow_data()?[..])?;
                // The delegate's vote included this member's tokens only if it came after the delegation.
                if rec.seq > voter.delegated_seq {
                    sub_vote(&mut p.tally, rec.choice, handed)?;
                    rec.weight = rec.weight.saturating_sub(handed);
                    rec.delegated = rec.delegated.saturating_sub(handed);
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
                o.power = o.power.checked_add(handed).ok_or(GovError::Overflow)?;
            }
        }
        add_vote(&mut p.tally, choice, weight)?;
        voter.locked_until = voter.locked_until.max(p.voting_ends_at);
        let (owner, delegate, ends) = (voter.owner, voter.delegate, p.voting_ends_at);
        lock_stake_until(&ctx.accounts.governance, &ctx.accounts.owner, &ctx.accounts.hub_dao, &ctx.accounts.pool, &ctx.accounts.stake, &ctx.accounts.stake_program, ends)?;

        let g = &mut ctx.accounts.governance;
        g.seq += 1;
        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = owner;
        record.choice = choice;
        record.weight = weight;
        record.delegated = 0;
        record.seq = g.seq;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteOverridden { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: owner, delegate, choice, weight, moved });
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

fn init_voter(v: &mut Voter, governance: Pubkey, owner: Pubkey, bump: u8) {
    if v.owner == Pubkey::default() {
        v.governance = governance;
        v.owner = owner;
        v.bump = bump;
    }
}

/// Holds the voter's stake in the proposal's pool until voting ends (the
/// member and this governance both sign).
fn lock_stake_until<'info>(
    g: &Account<'info, Governance>,
    owner: &Signer<'info>,
    hub_dao: &Account<'info, HubDao>,
    pool: &Account<'info, Pool>,
    stake: &Account<'info, Stake>,
    stake_program: &Program<'info, vortex_stake::program::VortexStake>,
    until: i64,
) -> Result<()> {
    let hub = g.hub_dao;
    let epoch_bytes = g.epoch.to_le_bytes();
    let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub.as_ref(), &epoch_bytes, &[g.bump]];
    vortex_stake::cpi::lock_until(
        CpiContext::new_with_signer(
            stake_program.key(),
            vortex_stake::cpi::accounts::LockUntil {
                owner: owner.to_account_info(),
                governance: g.to_account_info(),
                hub_dao: hub_dao.to_account_info(),
                pool: pool.to_account_info(),
                stake: stake.to_account_info(),
            },
            &[seeds],
        ),
        until,
    )
}

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
    pub config: VotingConfig,
    pub proposal_count: u64,
    /// Bumped by every delegation and vote, to order them.
    pub seq: u64,
    /// The hub epoch this setup is for (part of its address).
    pub epoch: u32,
    pub bump: u8,
}

/// A member's delegation state here. Their stake is in the hub's pool.
#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub governance: Pubkey,
    pub owner: Pubkey,
    /// Can't delegate or undelegate before this: the end of the latest vote they cast.
    pub locked_until: i64,
    /// Who they've delegated to (default = nobody).
    pub delegate: Pubkey,
    /// The pool their delegation is in.
    pub pool: Pubkey,
    /// What they delegated (locked in `pool` for this governance).
    pub delegated_amount: u64,
    /// `seq` when they last delegated.
    pub delegated_seq: u64,
    /// Sum of the stake delegated to them, all in `power_pool`.
    pub delegated_power: u64,
    pub power_pool: Pubkey,
    pub delegators: u32,
    /// Their delegators can't undelegate before this: the end of the latest vote they cast with them.
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
    /// The DAO's stake pool when proposed: votes count stake there.
    pub pool: Pubkey,
    pub created_at: i64,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
    /// Everything in the pool when proposed: what quorum is measured against.
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
            + 32
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
#[instruction(epoch: u32)]
pub struct InitGovernance<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(init, payer = payer, space = 8 + Governance::INIT_SPACE, seeds = [GOVERNANCE_SEED, hub_dao.key().as_ref(), &epoch.to_le_bytes()], bump)]
    pub governance: Account<'info, Governance>,
    /// The DAO's token on record at the hub (set_dao_token): a token model
    /// needs one, as members stake it in the hub's pool.
    #[account(seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()], bump = dao_token.bump, seeds::program = vortex_hub::ID)]
    pub dao_token: Account<'info, DaoToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(to: Pubkey)]
pub struct Delegate<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()], bump = dao_token.bump, seeds::program = vortex_hub::ID)]
    pub dao_token: Account<'info, DaoToken>,
    /// Delegations are made in the DAO's current pool.
    #[account(address = dao_token.pool @ GovError::PoolNotCurrent)]
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump, seeds::program = vortex_stake::ID)]
    pub stake: Account<'info, Stake>,
    /// CHECK: vortex-stake's lock record for this stake and governance, made or checked there.
    #[account(mut)]
    pub lock: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = owner, space = 8 + Voter::INIT_SPACE, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump)]
    pub voter: Account<'info, Voter>,
    /// The delegate's account, made here if they've never voted.
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Voter::INIT_SPACE,
        seeds = [VOTER_SEED, governance.key().as_ref(), to.as_ref()],
        bump,
    )]
    pub delegate_voter: Account<'info, Voter>,
    pub stake_program: Program<'info, vortex_stake::program::VortexStake>,
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
    /// The member's stake in the delegation's pool.
    #[account(mut, seeds = [STAKE_SEED, voter.pool.as_ref(), owner.key().as_ref()], bump = stake.bump, seeds::program = vortex_stake::ID)]
    pub stake: Account<'info, Stake>,
    #[account(mut)]
    pub lock: Account<'info, LockRecord>,
    pub stake_program: Program<'info, vortex_stake::program::VortexStake>,
}

#[derive(Accounts)]
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()], bump = dao_token.bump, seeds::program = vortex_hub::ID)]
    pub dao_token: Account<'info, DaoToken>,
    /// The DAO's current pool, which the proposal keeps.
    #[account(address = dao_token.pool @ GovError::PoolNotCurrent)]
    pub pool: Account<'info, Pool>,
    #[account(seeds = [STAKE_SEED, pool.key().as_ref(), proposer.key().as_ref()], bump, seeds::program = vortex_stake::ID)]
    pub proposer_stake: Option<Account<'info, Stake>>,
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
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(mut, has_one = governance @ GovError::WrongDao, has_one = pool @ GovError::WrongPool)]
    pub proposal: Account<'info, Proposal>,
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump, seeds::program = vortex_stake::ID)]
    pub stake: Account<'info, Stake>,
    #[account(init_if_needed, payer = owner, space = 8 + Voter::INIT_SPACE, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump)]
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
    pub stake_program: Program<'info, vortex_stake::program::VortexStake>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OverrideVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(mut, has_one = governance @ GovError::WrongDao, has_one = pool @ GovError::WrongPool)]
    pub proposal: Box<Account<'info, Proposal>>,
    pub pool: Box<Account<'info, Pool>>,
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump, seeds::program = vortex_stake::ID)]
    pub stake: Box<Account<'info, Stake>>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Box<Account<'info, Voter>>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), voter.delegate.as_ref()], bump = delegate_voter.bump)]
    pub delegate_voter: Box<Account<'info, Voter>>,
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
    pub overrides: Box<Account<'info, Override>>,
    #[account(
        init,
        payer = owner,
        space = 8 + VoteRecord::INIT_SPACE,
        seeds = [VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub vote_record: Box<Account<'info, VoteRecord>>,
    pub stake_program: Program<'info, vortex_stake::program::VortexStake>,
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
