//! Vortexes token-weighted governance, the Solana counterpart of the EVM
//! `Governance.sol`.
//!
//! - A DAO governs with one SPL token (Token or Token-2022). Members
//!   deposit it into the DAO's vault; their deposit is their voting power.
//! - Anyone with at least `proposal_threshold` deposited proposes a set of
//!   instructions. After `voting_delay`, voting is open for
//!   `voting_period`: for, against or abstain, one vote per member.
//! - It passes with quorum (for + against + abstain >= `quorum_bps` of all
//!   deposits when it was proposed) and approval (for / (for + against) >=
//!   `approval_bps`). Then anyone queues it, and after `timelock` anyone
//!   executes it within `execution_period`. Its instructions run signed by
//!   the DAO's treasury PDA.
//!
//! Instead of the EVM's past-balance snapshots, voting tokens stay locked:
//! a member who votes can't withdraw until every proposal they voted on
//! has closed, so the same tokens can't vote twice from another wallet.
//!
//! Changing the rules (`update_config`) needs the treasury's signature,
//! which only an executing proposal can give - the `onlyGovernance` of the
//! EVM contracts.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execute_instructions, execution_expired, instructions_len, passed, proposal_state, timelock_complete,
    validate_instructions, validate_name, validate_uri, GovError, Lifecycle, ProposalState, StoredInstruction, Tally,
    VotingConfig, MAX_NAME_LEN, TREASURY_SEED,
};

declare_id!("HGy7TqBWCxacyVoznw4UciYdcdwpmJ7JdCoZu8XTTHQ");

pub const DAO_SEED: &[u8] = b"dao";
pub const VAULT_SEED: &[u8] = b"vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const VOTE_SEED: &[u8] = b"vote";

#[program]
pub mod vortex_token_weighted {
    use super::*;

    /// Creates a DAO governed by `mint`. `create_key` is any fresh keypair;
    /// it only makes the DAO's address unique.
    pub fn create_dao(ctx: Context<CreateDao>, name: String, config: VotingConfig) -> Result<()> {
        validate_name(&name)?;
        config.validate()?;
        let dao = &mut ctx.accounts.dao;
        dao.create_key = ctx.accounts.create_key.key();
        dao.creator = ctx.accounts.payer.key();
        dao.name = name;
        dao.mint = ctx.accounts.mint.key();
        dao.vault = ctx.accounts.vault.key();
        dao.config = config;
        dao.proposal_count = 0;
        dao.total_deposited = 0;
        dao.bump = ctx.bumps.dao;
        dao.treasury_bump = Pubkey::find_program_address(&[TREASURY_SEED, dao.key().as_ref()], &crate::ID).1;
        emit!(DaoCreated { dao: dao.key(), mint: dao.mint, creator: dao.creator, name: dao.name.clone() });
        Ok(())
    }

    /// Moves `amount` of the DAO's token from the owner into the vault,
    /// adding to their voting power.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
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
        if voter.owner == Pubkey::default() {
            voter.dao = ctx.accounts.dao.key();
            voter.owner = ctx.accounts.owner.key();
            voter.bump = ctx.bumps.voter;
        }
        voter.amount = voter.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        let dao = &mut ctx.accounts.dao;
        dao.total_deposited = dao.total_deposited.checked_add(amount).ok_or(GovError::Overflow)?;
        emit!(Deposited { dao: dao.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Takes `amount` back out of the vault, once every proposal the owner
    /// voted on has closed.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(now >= voter.locked_until, GovError::TokensLocked);
        require!(amount <= voter.amount, GovError::InsufficientDeposit);
        voter.amount -= amount;
        let dao = &mut ctx.accounts.dao;
        dao.total_deposited -= amount;

        let create_key = dao.create_key;
        let seeds: &[&[u8]] = &[DAO_SEED, create_key.as_ref(), &[dao.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.owner_token_account.to_account_info(),
                    authority: dao.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;
        emit!(Withdrawn { dao: dao.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Proposes `instructions`, run by the treasury if the vote passes.
    /// `id` must be the DAO's next proposal number (proposal_count + 1).
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let dao = &mut ctx.accounts.dao;
        require!(id == dao.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        let treasury = treasury_address(&dao.key(), dao.treasury_bump)?;
        validate_instructions(&instructions, &treasury)?;
        let power = ctx.accounts.voter.as_ref().map_or(0, |v| v.amount);
        require!(power >= dao.config.proposal_threshold, GovError::ProposalThresholdNotMet);

        let now = Clock::get()?.unix_timestamp;
        dao.proposal_count = id;
        let proposal = &mut ctx.accounts.proposal;
        proposal.dao = dao.key();
        proposal.id = id;
        proposal.proposer = ctx.accounts.proposer.key();
        proposal.metadata_uri = metadata_uri;
        proposal.created_at = now;
        proposal.voting_starts_at = now + dao.config.voting_delay as i64;
        proposal.voting_ends_at = proposal.voting_starts_at + dao.config.voting_period as i64;
        proposal.quorum_base = dao.total_deposited;
        proposal.tally = Tally::default();
        proposal.queued_at = 0;
        proposal.executed = false;
        proposal.cancelled = false;
        proposal.bump = ctx.bumps.proposal;
        proposal.instructions = instructions;
        emit!(ProposalCreated {
            dao: dao.key(),
            proposal: proposal.key(),
            id,
            proposer: proposal.proposer,
            metadata_uri: proposal.metadata_uri.clone(),
            voting_starts_at: proposal.voting_starts_at,
            voting_ends_at: proposal.voting_ends_at,
        });
        Ok(())
    }

    /// One vote per member per proposal, weighted by their deposit. Locks
    /// that deposit until voting closes.
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let proposal = &mut ctx.accounts.proposal;
        require!(!proposal.cancelled && !proposal.executed, GovError::ProposalNotActive);
        require!(now >= proposal.voting_starts_at && now < proposal.voting_ends_at, GovError::ProposalNotActive);
        let voter = &mut ctx.accounts.voter;
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);

        match choice {
            VoteChoice::For => proposal.tally.for_votes = proposal.tally.for_votes.checked_add(weight).ok_or(GovError::Overflow)?,
            VoteChoice::Against => proposal.tally.against_votes = proposal.tally.against_votes.checked_add(weight).ok_or(GovError::Overflow)?,
            VoteChoice::Abstain => proposal.tally.abstain_votes = proposal.tally.abstain_votes.checked_add(weight).ok_or(GovError::Overflow)?,
        }
        voter.locked_until = voter.locked_until.max(proposal.voting_ends_at);

        let record = &mut ctx.accounts.vote_record;
        record.proposal = proposal.key();
        record.voter = voter.owner;
        record.choice = choice;
        record.weight = weight;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteCast { dao: proposal.dao, proposal: proposal.key(), id: proposal.id, voter: voter.owner, choice, weight });
        Ok(())
    }

    /// Queues a proposal that passed. Anyone can call it once voting ends.
    pub fn queue(ctx: Context<Queue>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config = ctx.accounts.dao.config;
        let proposal = &mut ctx.accounts.proposal;
        require!(!proposal.executed, GovError::ProposalAlreadyExecuted);
        require!(!proposal.cancelled, GovError::ProposalAlreadyCancelled);
        require!(proposal.queued_at == 0, GovError::ProposalAlreadyQueued);
        require!(now >= proposal.voting_ends_at, GovError::VotingNotEnded);
        require!(vortex_core::has_quorum(&proposal.tally, proposal.quorum_base, &config), GovError::QuorumNotReached);
        require!(vortex_core::has_approval(&proposal.tally, &config), GovError::ApprovalThresholdNotMet);
        proposal.queued_at = now;
        emit!(ProposalQueued { dao: proposal.dao, proposal: proposal.key(), id: proposal.id, executable_at: now + config.timelock as i64 });
        Ok(())
    }

    /// Runs a queued proposal's instructions once its timelock has passed.
    /// Anyone can call it; pass every account the instructions use (and
    /// the programs they call) as remaining accounts.
    pub fn execute<'info>(ctx: Context<'info, Execute<'info>>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let dao = &ctx.accounts.dao;
        let config = dao.config;
        let proposal = &mut ctx.accounts.proposal;
        require!(!proposal.executed, GovError::ProposalAlreadyExecuted);
        require!(!proposal.cancelled, GovError::ProposalAlreadyCancelled);
        let life = proposal.lifecycle();
        require!(timelock_complete(&life, &config, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &config, now), GovError::ProposalExpired);

        // Recorded on-chain before any instruction runs, so an instruction
        // calling back into this program can't execute it a second time.
        proposal.executed = true;
        proposal.exit(&crate::ID)?;

        let dao_key = dao.key();
        let seeds: &[&[u8]] = &[TREASURY_SEED, dao_key.as_ref(), &[dao.treasury_bump]];
        let instructions = proposal.instructions.clone();
        execute_instructions(&instructions, ctx.remaining_accounts, seeds)?;
        emit!(ProposalExecuted { dao: dao_key, proposal: proposal.key(), id: proposal.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed. Only its proposer, or the
    /// DAO itself through a proposal.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let dao = &ctx.accounts.dao;
        let proposal = &mut ctx.accounts.proposal;
        require!(!proposal.executed, GovError::ProposalAlreadyExecuted);
        require!(!proposal.cancelled, GovError::ProposalAlreadyCancelled);
        let who = ctx.accounts.authority.key();
        require!(who == proposal.proposer || who == treasury_address(&dao.key(), dao.treasury_bump)?, GovError::Unauthorized);
        proposal.cancelled = true;
        emit!(ProposalCancelled { dao: dao.key(), proposal: proposal.key(), id: proposal.id, by: who });
        Ok(())
    }

    /// Replaces the DAO's voting rules. Needs the treasury's signature, so
    /// only a passed proposal can do it.
    pub fn update_config(ctx: Context<UpdateConfig>, config: VotingConfig) -> Result<()> {
        config.validate()?;
        let dao = &mut ctx.accounts.dao;
        dao.config = config;
        emit!(ConfigUpdated { dao: dao.key(), config });
        Ok(())
    }

    /// Returns a vote record's rent to its voter once voting has closed.
    pub fn close_vote_record(ctx: Context<CloseVoteRecord>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(now >= ctx.accounts.proposal.voting_ends_at || ctx.accounts.proposal.cancelled, GovError::VotingNotEnded);
        Ok(())
    }
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Dao {
    pub create_key: Pubkey,
    pub creator: Pubkey,
    #[max_len(MAX_NAME_LEN)]
    pub name: String,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub config: VotingConfig,
    pub proposal_count: u64,
    /// Everything in the vault, i.e. all voting power.
    pub total_deposited: u64,
    pub bump: u8,
    pub treasury_bump: u8,
}

/// The DAO's treasury PDA.
pub fn treasury_address(dao: &Pubkey, treasury_bump: u8) -> Result<Pubkey> {
    Pubkey::create_program_address(&[TREASURY_SEED, dao.as_ref(), &[treasury_bump]], &crate::ID).map_err(|_| error!(GovError::WrongDao))
}

#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub dao: Pubkey,
    pub owner: Pubkey,
    /// Deposited tokens = voting power.
    pub amount: u64,
    /// Can't withdraw before this (end of the latest vote they cast in).
    pub locked_until: i64,
    pub bump: u8,
}

#[account]
pub struct Proposal {
    pub dao: Pubkey,
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
    pub instructions: Vec<StoredInstruction>,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + 32 + 8 + 32 + (4 + metadata_uri.len()) + 8 * 3 + 8 + Tally::INIT_SPACE + 8 + 1 + 1 + 1 + instructions_len(instructions)
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
pub struct CreateDao<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub create_key: Signer<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(init, payer = payer, space = 8 + Dao::INIT_SPACE, seeds = [DAO_SEED, create_key.key().as_ref()], bump)]
    pub dao: Account<'info, Dao>,
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, dao.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = dao,
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
    pub dao: Account<'info, Dao>,
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Voter::INIT_SPACE,
        seeds = [VOTER_SEED, dao.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub voter: Account<'info, Voter>,
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
    pub dao: Account<'info, Dao>,
    #[account(mut, seeds = [VOTER_SEED, dao.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut)]
    pub dao: Account<'info, Dao>,
    /// The proposer's deposit, when the DAO requires one to propose.
    #[account(seeds = [VOTER_SEED, dao.key().as_ref(), proposer.key().as_ref()], bump)]
    pub voter: Option<Account<'info, Voter>>,
    #[account(
        init,
        payer = proposer,
        space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, dao.key().as_ref(), &id.to_le_bytes()],
        bump,
    )]
    pub proposal: Account<'info, Proposal>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CastVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, dao.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
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
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct Execute<'info> {
    /// Not `mut`: an executing proposal may change it (update_config), and
    /// this instruction must not write a stale copy back over that.
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct Cancel<'info> {
    pub authority: Signer<'info>,
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub dao: Account<'info, Dao>,
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
pub struct DaoCreated {
    pub dao: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub name: String,
}

#[event]
pub struct Deposited {
    pub dao: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct Withdrawn {
    pub dao: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub total: u64,
}

#[event]
pub struct ProposalCreated {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
}

#[event]
pub struct VoteCast {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
}

#[event]
pub struct ProposalQueued {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub executable_at: i64,
}

#[event]
pub struct ProposalExecuted {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
}

#[event]
pub struct ProposalCancelled {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub by: Pubkey,
}

#[event]
pub struct ConfigUpdated {
    pub dao: Pubkey,
    pub config: VotingConfig,
}
