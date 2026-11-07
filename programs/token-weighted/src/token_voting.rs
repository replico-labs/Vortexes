// Token voting, shared by vortex-token-weighted and vortex-quadratic.
//
// Each of those programs is its own lib.rs with its own program ID, a
// `vote_weight(deposit) -> u64` function, `include!("token_voting.rs")`,
// and a `#[program]` module whose instructions call `handlers::*` here.
// Everything else - accounts, rules, events - is this file, so the two
// models can't drift apart.
//
// A DAO's governance here is the account `[GOVERNANCE_SEED, hub_dao, epoch]`:
// - Members stake the DAO's token in its pool (vortex-stake), not here;
//   voting power is `vote_weight(stake)`. Stake survives every switch.
// - Anyone with at least `proposal_threshold` staked (raw tokens) proposes
//   a set of instructions. The proposal keeps the DAO's pool as it was when
//   proposed, so a token change mid-vote doesn't touch it. After
//   `voting_delay`, voting is open for `voting_period`: for, against or
//   abstain, one vote per member.
// - It passes with quorum (for + against + abstain >= `quorum_bps` of
//   `vote_weight(everything in the pool when proposed)`) and approval
//   (for / (for + against) >= `approval_bps`). Then anyone queues it.
// - After `timelock` (and before `execution_period` runs out), anyone runs
//   it through the hub: the hub calls `confirm_execution` here, then runs
//   the instructions signed by the DAO's treasury.
//
// Voting tokens stay locked: a vote locks the member's stake in the pool
// until voting closes (signed by the member and this governance), so the
// same tokens can't vote twice from another wallet (Solana tokens have no
// balance history to snapshot).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use vortex_core::{
    execution_expired, has_approval, has_quorum, passed, proposal_state, timelock_complete, validate_instructions,
    validate_uri, GovError, Lifecycle, ProposalCore, ProposalState, StoredInstruction, Tally, VotingConfig,
    GOVERNANCE_SEED, STAKE_SEED,
};
use vortex_hub::{Dao as HubDao, DaoToken};
use vortex_stake::{Pool, Stake};

pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const VOTE_SEED: &[u8] = b"vote";

/// The instruction bodies. Each program's lib.rs declares its own
/// `#[program]` module (so each gets its own name and IDL) whose
/// instructions call these.
pub mod handlers {
    use super::*;

    /// Sets up a hub DAO in this program: its rules. The DAO must have a
    /// token on record (members stake it in the hub's pool). Called by the
    /// DAO's creator right after `vortex_hub::create_dao`, or by the DAO's
    /// treasury (inside a passed proposal) when switching to this model.
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
        g.epoch = epoch;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: ctx.accounts.dao_token.mint });
        Ok(())
    }

    /// Proposes `instructions`, run by the DAO's treasury if the vote
    /// passes. `id` must be the next proposal number (proposal_count + 1).
    /// Only while this program runs the DAO.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        let power = ctx.accounts.proposer_stake.as_ref().map_or(0, |s| s.amount);
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
        p.pool = ctx.accounts.pool.key();
        p.quorum_base = vote_weight(ctx.accounts.pool.total);
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

    /// One vote per member per proposal, weighted by `vote_weight` of their
    /// stake in the proposal's pool. Locks that stake until voting closes
    /// (the member and this governance both sign the lock).
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(!p.cancelled && !p.executed, GovError::ProposalNotActive);
        require!(now >= p.voting_starts_at && now < p.voting_ends_at, GovError::ProposalNotActive);
        let owner = ctx.accounts.owner.key();
        let weight = vote_weight(ctx.accounts.stake.amount);
        require!(weight > 0, GovError::NoVotingPower);

        let total = match choice {
            VoteChoice::For => &mut p.tally.for_votes,
            VoteChoice::Against => &mut p.tally.against_votes,
            VoteChoice::Abstain => &mut p.tally.abstain_votes,
        };
        *total = total.checked_add(weight).ok_or(GovError::Overflow)?;

        let g = &ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let epoch_bytes = g.epoch.to_le_bytes();
        let seeds: &[&[u8]] = &[GOVERNANCE_SEED, hub_dao.as_ref(), &epoch_bytes, &[g.bump]];
        vortex_stake::cpi::lock_until(
            CpiContext::new_with_signer(
                ctx.accounts.stake_program.key(),
                vortex_stake::cpi::accounts::LockUntil {
                    owner: ctx.accounts.owner.to_account_info(),
                    governance: g.to_account_info(),
                    hub_dao: ctx.accounts.hub_dao.to_account_info(),
                    pool: ctx.accounts.pool.to_account_info(),
                    stake: ctx.accounts.stake.to_account_info(),
                },
                &[seeds],
            ),
            p.voting_ends_at,
        )?;

        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = owner;
        record.choice = choice;
        record.weight = weight;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: owner, choice, weight });
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
    /// Marks the proposal executed and answers with its address; the hub
    /// then runs its instructions.
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

    /// Cancels a proposal that hasn't executed. Only its proposer, or the
    /// DAO itself through a proposal.
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

    /// Replaces the voting rules. Needs the treasury's signature, so only a
    /// passed proposal can do it.
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
// ACCOUNTS
// ---------------------------------------------------------------

/// A hub DAO's setup in this program.
#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    /// The DAO's treasury in the hub: the only signer proposals may use.
    pub treasury: Pubkey,
    /// The hub's executor PDA for this DAO: the only caller of confirm_execution.
    pub executor: Pubkey,
    pub config: VotingConfig,
    pub proposal_count: u64,
    /// The hub epoch this setup is for (part of its address).
    pub epoch: u32,
    pub bump: u8,
}

#[account]
pub struct Proposal {
    /// Standard prefix the hub reads: DAO, epoch, instructions.
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
    /// `vote_weight(everything in the pool)` when proposed: what quorum is measured against.
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

    /// State at `now` under `config` (what clients show).
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
    pub weight: u64,
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
    /// The hub DAO's creator, or its treasury (inside a passed proposal).
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
    /// The proposer's stake, when the DAO requires one to propose.
    #[account(seeds = [STAKE_SEED, pool.key().as_ref(), proposer.key().as_ref()], bump, seeds::program = vortex_stake::ID)]
    pub proposer_stake: Option<Account<'info, Stake>>,
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
    #[account(has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(mut, has_one = governance @ GovError::WrongDao, has_one = pool @ GovError::WrongPool)]
    pub proposal: Account<'info, Proposal>,
    pub pool: Account<'info, Pool>,
    /// The member's stake in the proposal's pool.
    #[account(mut, seeds = [STAKE_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = stake.bump, seeds::program = vortex_stake::ID)]
    pub stake: Account<'info, Stake>,
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
