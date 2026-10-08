//! Vortexes board governance, the Solana counterpart of the EVM
//! `BoardGovernance.sol`: a multisig. No token; power is being a signer.
//!
//! - Signers propose; proposing counts as the proposer's confirmation.
//! - Other signers confirm (or revoke their confirmation). Once
//!   `required_approvals` signers have confirmed, the proposal is queued;
//!   if confirmations drop below that before it runs, it's unqueued.
//! - After `timelock` (and within `execution_period`) it runs through the
//!   hub, signed by the DAO's treasury.
//! - Adding or removing signers and changing the rules only happen through
//!   a passed proposal (the treasury's signature).
//!
//! Unlike the EVM version, only the confirmations of people who are still
//! signers count when a proposal runs, so removing a signer also removes
//! their pending confirmations.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use vortex_core::{
    execution_expired, timelock_complete, validate_instructions, validate_uri, GovError, Lifecycle, ProposalCore,
    StoredInstruction, VotingConfig, GOVERNANCE_SEED,
};
use vortex_hub::Dao as HubDao;

declare_id!("5FLdbqX8Q6FsFZSPxiFK3ftaF4VBrHoz7MdDQ4E2jQzA");

pub const PROPOSAL_SEED: &[u8] = b"proposal";
/// Most signers a board can have.
pub const MAX_SIGNERS: usize = 20;

#[program]
pub mod vortex_board {
    use super::*;

    /// Sets up a hub DAO in this program: its signers and rules. The
    /// creator right after `vortex_hub::create_dao`, or the DAO's treasury
    /// (inside a passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, signers: Vec<Pubkey>, config: BoardConfig) -> Result<()> {
        validate_signers(&signers)?;
        config.validate(signers.len())?;
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
        g.signers = signers;
        g.config = config;
        g.proposal_count = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), signers: g.signers.clone(), required_approvals: config.required_approvals });
        Ok(())
    }

    /// A signer proposes instructions for the treasury to run; their own
    /// confirmation counts. `id` must be proposal_count + 1.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        let proposer = ctx.accounts.proposer.key();
        require!(g.is_signer(&proposer), GovError::NotSigner);
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;

        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions };
        p.governance = g.key();
        p.id = id;
        p.proposer = proposer;
        p.metadata_uri = metadata_uri;
        p.created_at = now;
        p.confirmations = Vec::new();
        p.queued_at = 0;
        p.executed = false;
        p.cancelled = false;
        p.bump = ctx.bumps.proposal;
        emit!(ProposalCreated { hub_dao: g.hub_dao, proposal: p.key(), id, proposer, metadata_uri: p.metadata_uri.clone() });
        add_confirmation(g, p, proposer, now)
    }

    /// A signer confirms a proposal. Reaching `required_approvals` queues it.
    pub fn confirm(ctx: Context<Confirm>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let signer = ctx.accounts.signer.key();
        require!(g.is_signer(&signer), GovError::NotSigner);
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(!p.confirmations.contains(&signer), GovError::AlreadyConfirmed);
        add_confirmation(g, p, signer, Clock::get()?.unix_timestamp)
    }

    /// A signer withdraws their confirmation. Dropping below
    /// `required_approvals` unqueues the proposal.
    pub fn revoke_confirmation(ctx: Context<Confirm>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let signer = ctx.accounts.signer.key();
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let at = p.confirmations.iter().position(|k| *k == signer).ok_or(GovError::NotConfirmedBySigner)?;
        p.confirmations.swap_remove(at);
        let count = g.current_confirmations(p);
        if p.queued_at != 0 && count < g.config.required_approvals as usize {
            p.queued_at = 0;
            emit!(ProposalUnqueued { hub_dao: g.hub_dao, proposal: p.key(), id: p.id });
        }
        emit!(ConfirmationRevoked { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, signer, confirmations: count as u16 });
        Ok(())
    }

    /// The hub's check before it runs a proposal (vortex-core's
    /// CONFIRM_EXECUTION); only the hub's executor PDA for this DAO.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let timing = g.config.timing();
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        // Signers may have changed since it was queued: count only current ones.
        require!(g.current_confirmations(p) >= g.config.required_approvals as usize, GovError::ThresholdNotMet);
        let life = p.lifecycle();
        require!(timelock_complete(&life, &timing, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &timing, now), GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: g.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed: its proposer, or the DAO
    /// through a proposal.
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

    /// Adds a signer (only through a passed proposal).
    pub fn add_signer(ctx: Context<TreasuryOnly>, signer: Pubkey) -> Result<()> {
        let g = &mut ctx.accounts.governance;
        require!(signer != Pubkey::default(), GovError::InvalidAmount);
        require!(!g.is_signer(&signer), GovError::AlreadySigner);
        require!(g.signers.len() < MAX_SIGNERS, GovError::TooManySigners);
        g.signers.push(signer);
        emit!(SignersChanged { hub_dao: g.hub_dao, signers: g.signers.clone(), required_approvals: g.config.required_approvals });
        Ok(())
    }

    /// Removes a signer (only through a passed proposal). Can't leave fewer
    /// signers than required approvals.
    pub fn remove_signer(ctx: Context<TreasuryOnly>, signer: Pubkey) -> Result<()> {
        let g = &mut ctx.accounts.governance;
        let at = g.signers.iter().position(|k| *k == signer).ok_or(GovError::NotSigner)?;
        require!(g.signers.len() > g.config.required_approvals as usize, GovError::InvalidApprovals);
        g.signers.swap_remove(at);
        emit!(SignersChanged { hub_dao: g.hub_dao, signers: g.signers.clone(), required_approvals: g.config.required_approvals });
        Ok(())
    }

    /// Replaces the rules: required approvals, timelock, execution window
    /// (only through a passed proposal).
    pub fn update_config(ctx: Context<TreasuryOnly>, config: BoardConfig) -> Result<()> {
        let g = &mut ctx.accounts.governance;
        config.validate(g.signers.len())?;
        g.config = config;
        emit!(SignersChanged { hub_dao: g.hub_dao, signers: g.signers.clone(), required_approvals: config.required_approvals });
        Ok(())
    }
}

/// Records a confirmation and queues the proposal when it reaches the threshold.
fn add_confirmation<'info>(g: &Account<'info, Governance>, p: &mut Account<'info, Proposal>, signer: Pubkey, now: i64) -> Result<()> {
    p.confirmations.push(signer);
    let count = g.current_confirmations(p);
    emit!(Confirmed { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, signer, confirmations: count as u16 });
    if p.queued_at == 0 && count >= g.config.required_approvals as usize {
        p.queued_at = now;
        emit!(ProposalQueued { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, executable_at: now + g.config.timelock as i64 });
    }
    Ok(())
}

fn validate_signers(signers: &[Pubkey]) -> Result<()> {
    require!(!signers.is_empty(), GovError::InvalidApprovals);
    require!(signers.len() <= MAX_SIGNERS, GovError::TooManySigners);
    for (i, s) in signers.iter().enumerate() {
        require!(*s != Pubkey::default(), GovError::InvalidAmount);
        require!(!signers[..i].contains(s), GovError::AlreadySigner);
    }
    Ok(())
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct BoardConfig {
    /// Confirmations needed (M of N).
    pub required_approvals: u16,
    /// Seconds between reaching the threshold and being able to run.
    pub timelock: u32,
    /// Seconds after the timelock during which it can still run.
    pub execution_period: u32,
}

impl BoardConfig {
    pub fn validate(&self, signer_count: usize) -> Result<()> {
        require!(self.required_approvals >= 1 && self.required_approvals as usize <= signer_count, GovError::InvalidApprovals);
        Ok(())
    }

    /// Timelock and execution window, in vortex-core's shape.
    pub fn timing(&self) -> VotingConfig {
        VotingConfig {
            quorum_bps: 1,
            approval_bps: 1,
            voting_delay: 0,
            voting_period: 1,
            timelock: self.timelock,
            execution_period: self.execution_period,
            proposal_threshold: 0,
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    #[max_len(MAX_SIGNERS)]
    pub signers: Vec<Pubkey>,
    pub config: BoardConfig,
    pub proposal_count: u64,
    pub bump: u8,
}

impl Governance {
    pub fn is_signer(&self, key: &Pubkey) -> bool {
        self.signers.contains(key)
    }

    /// Confirmations on `p` from people who are signers now.
    pub fn current_confirmations(&self, p: &Proposal) -> usize {
        p.confirmations.iter().filter(|k| self.is_signer(k)).count()
    }
}

#[account]
pub struct Proposal {
    /// Standard prefix the hub reads.
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub created_at: i64,
    /// Who has confirmed (at most MAX_SIGNERS).
    pub confirmations: Vec<Pubkey>,
    /// When it reached the threshold; 0 if it hasn't (or dropped back).
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
    pub bump: u8,
}

/// Where a board proposal is. Same states as the EVM contract.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardState {
    /// Collecting confirmations.
    Active,
    Queued,
    Executed,
    Cancelled,
    Expired,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + 8 + (4 + 32 * MAX_SIGNERS) + 8 + 1 + 1 + 1
    }

    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle {
            voting_starts_at: self.created_at,
            voting_ends_at: self.created_at,
            queued_at: self.queued_at,
            executed: self.executed,
            cancelled: self.cancelled,
        }
    }

    /// State at `now` (what clients show).
    pub fn state(&self, config: &BoardConfig, now: i64) -> BoardState {
        if self.cancelled {
            BoardState::Cancelled
        } else if self.executed {
            BoardState::Executed
        } else if self.queued_at == 0 {
            BoardState::Active
        } else if execution_expired(&self.lifecycle(), &config.timing(), now) {
            BoardState::Expired
        } else {
            BoardState::Queued
        }
    }
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
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(
        init, payer = proposer, space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub proposal: Account<'info, Proposal>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Confirm<'info> {
    pub signer: Signer<'info>,
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
pub struct TreasuryOnly<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct GovernanceInitialized {
    pub hub_dao: Pubkey,
    pub governance: Pubkey,
    pub signers: Vec<Pubkey>,
    pub required_approvals: u16,
}

#[event]
pub struct ProposalCreated {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
}

#[event]
pub struct Confirmed {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub signer: Pubkey,
    pub confirmations: u16,
}

#[event]
pub struct ConfirmationRevoked {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub signer: Pubkey,
    pub confirmations: u16,
}

#[event]
pub struct ProposalQueued {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub executable_at: i64,
}

#[event]
pub struct ProposalUnqueued {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
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
pub struct SignersChanged {
    pub hub_dao: Pubkey,
    pub signers: Vec<Pubkey>,
    pub required_approvals: u16,
}
