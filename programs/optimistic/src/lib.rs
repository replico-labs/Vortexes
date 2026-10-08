//! Vortexes optimistic governance, the Solana counterpart of the EVM
//! `OptimisticGovernance.sol`: proposals pass by default, and only disputed
//! ones need a vote.
//!
//! - Members deposit the DAO's token into its vault; the deposit is their
//!   voting power if a vote is needed.
//! - Anyone with at least `proposal_threshold` deposited proposes. During
//!   `challenge_period`, anyone may challenge it by posting
//!   `challenge_bond` tokens from their wallet. Only one challenge per
//!   proposal.
//! - Unchallenged: once the window ends, anyone finalizes it and it's
//!   queued.
//! - Challenged: a token vote runs for `voting_period` (for, against or
//!   abstain, weighted by deposit). It passes with quorum (`quorum_bps` of
//!   all deposits when proposed) and approval (`approval_bps` of
//!   for + against). If it passes, it's queued and the bond goes to the
//!   DAO's treasury; if not, the bond goes back to the challenger.
//! - Queued proposals run through the hub after `timelock`, within
//!   `execution_period`, like every other model.
//!
//! Unlike the EVM version, a challenger whose proposal gets cancelled can
//! reclaim their bond (`reclaim_bond`) instead of it being stuck.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execution_expired, has_approval, has_quorum, passed, timelock_complete, validate_instructions, validate_uri,
    GovError, Lifecycle, ProposalCore, StoredInstruction, Tally, GOVERNANCE_SEED,
};
use vortex_hub::Dao as HubDao;

declare_id!("Bjcfj2p8da2mjzDzMbcpxSrqayuiDToDVZdaBPmGPpdX");

pub const VAULT_SEED: &[u8] = b"vault";
pub const BOND_VAULT_SEED: &[u8] = b"bond_vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const VOTE_SEED: &[u8] = b"vote";

#[program]
pub mod vortex_optimistic {
    use super::*;

    /// Sets up a hub DAO in this program: rules, token, vault and bond
    /// vault. The creator right after `vortex_hub::create_dao`, or the DAO's
    /// treasury (inside a passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, config: OptimisticConfig) -> Result<()> {
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
        g.bond_vault = ctx.accounts.bond_vault.key();
        g.config = config;
        g.proposal_count = 0;
        g.total_deposited = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint });
        Ok(())
    }

    /// Deposits the DAO's token, adding voting power.
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
            voter.governance = ctx.accounts.governance.key();
            voter.owner = ctx.accounts.owner.key();
            voter.bump = ctx.bumps.voter;
        }
        voter.amount = voter.amount.checked_add(amount).ok_or(GovError::Overflow)?;
        let g = &mut ctx.accounts.governance;
        g.total_deposited = g.total_deposited.checked_add(amount).ok_or(GovError::Overflow)?;
        emit!(Deposited { governance: g.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Withdraws a deposit once every vote the owner cast in has closed.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(now >= voter.locked_until, GovError::TokensLocked);
        require!(amount <= voter.amount, GovError::InsufficientDeposit);
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

    /// Proposes instructions for the treasury to run. They pass after the
    /// challenge window unless challenged. `id` must be proposal_count + 1.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        let power = ctx.accounts.voter.as_ref().map_or(0, |v| v.amount);
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
        p.challenge_deadline = now + g.config.challenge_period as i64;
        p.challenged = false;
        p.challenger = Pubkey::default();
        p.challenge_bond = 0;
        p.voting_ends_at = 0;
        p.quorum_base = g.total_deposited;
        p.tally = Tally::default();
        p.bond_resolved = false;
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
            challenge_deadline: p.challenge_deadline,
        });
        Ok(())
    }

    /// Disputes a proposal within its challenge window by posting the bond
    /// from the challenger's wallet. Starts a token vote.
    pub fn challenge(ctx: Context<Challenge>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(!p.challenged, GovError::AlreadyChallenged);
        require!(now < p.challenge_deadline, GovError::ChallengeWindowClosed);
        let bond = g.config.challenge_bond;
        if bond > 0 {
            token_interface::transfer_checked(
                CpiContext::new(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.challenger_token_account.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        to: ctx.accounts.bond_vault.to_account_info(),
                        authority: ctx.accounts.challenger.to_account_info(),
                    },
                ),
                bond,
                ctx.accounts.mint.decimals,
            )?;
        }
        p.challenged = true;
        p.challenger = ctx.accounts.challenger.key();
        p.challenge_bond = bond;
        p.voting_ends_at = now + g.config.voting_period as i64;
        emit!(Challenged { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, challenger: p.challenger, bond, voting_ends_at: p.voting_ends_at });
        Ok(())
    }

    /// Votes on a challenged proposal, weighted by deposit. One vote per
    /// member; locks their deposit until voting closes.
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(!p.cancelled && !p.executed, GovError::ProposalNotActive);
        require!(p.challenged && now < p.voting_ends_at, GovError::ProposalNotActive);
        let voter = &mut ctx.accounts.voter;
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);
        let total = match choice {
            VoteChoice::For => &mut p.tally.for_votes,
            VoteChoice::Against => &mut p.tally.against_votes,
            VoteChoice::Abstain => &mut p.tally.abstain_votes,
        };
        *total = total.checked_add(weight).ok_or(GovError::Overflow)?;
        voter.locked_until = voter.locked_until.max(p.voting_ends_at);
        let record = &mut ctx.accounts.vote_record;
        record.proposal = p.key();
        record.voter = voter.owner;
        record.choice = choice;
        record.weight = weight;
        record.bump = ctx.bumps.vote_record;
        emit!(VoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: voter.owner, choice, weight });
        Ok(())
    }

    /// Queues an unchallenged proposal once its window has closed. Anyone.
    pub fn finalize_unchallenged(ctx: Context<FinalizeUnchallenged>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let timelock = ctx.accounts.governance.config.timelock;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(!p.challenged, GovError::AlreadyChallenged);
        require!(now >= p.challenge_deadline, GovError::ChallengeWindowOpen);
        require!(p.queued_at == 0, GovError::ProposalAlreadyQueued);
        p.queued_at = now;
        emit!(ProposalQueued { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, executable_at: now + timelock as i64 });
        Ok(())
    }

    /// Settles a challenged proposal once voting ends. Anyone. Passed: it's
    /// queued and the bond goes to the treasury. Failed: the bond goes back
    /// to the challenger.
    pub fn finalize_challenge(ctx: Context<FinalizeChallenge>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let config = g.config;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.challenged, GovError::NotChallenged);
        require!(now >= p.voting_ends_at, GovError::VotingNotEnded);
        require!(!p.bond_resolved, GovError::BondAlreadyResolved);
        let succeeded = p.passed(&config);

        // Only the account being paid needs passing in.
        let (to, expected_owner) = if succeeded {
            (ctx.accounts.treasury_token_account.as_ref(), g.treasury)
        } else {
            (ctx.accounts.challenger_token_account.as_ref(), p.challenger)
        };
        let to = to.ok_or(GovError::Unauthorized)?;
        require_keys_eq!(to.owner, expected_owner, GovError::Unauthorized);
        pay_out_bond(g, &ctx.accounts.bond_vault, to, &ctx.accounts.mint, &ctx.accounts.token_program, p.challenge_bond)?;
        p.bond_resolved = true;
        if succeeded {
            p.queued_at = now;
            emit!(ProposalQueued { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, executable_at: now + config.timelock as i64 });
        }
        emit!(ChallengeSettled { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, passed: succeeded, bond_to: expected_owner });
        Ok(())
    }

    /// Returns the bond to the challenger of a proposal that was cancelled
    /// before its challenge was settled. Anyone.
    pub fn reclaim_bond(ctx: Context<ReclaimBond>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        require!(p.cancelled, GovError::ProposalNotActive);
        require!(p.challenged, GovError::NotChallenged);
        require!(!p.bond_resolved, GovError::BondAlreadyResolved);
        require_keys_eq!(ctx.accounts.challenger_token_account.owner, p.challenger, GovError::Unauthorized);
        pay_out_bond(g, &ctx.accounts.bond_vault, &ctx.accounts.challenger_token_account, &ctx.accounts.mint, &ctx.accounts.token_program, p.challenge_bond)?;
        p.bond_resolved = true;
        Ok(())
    }

    /// The hub's check before it runs a proposal (vortex-core's
    /// CONFIRM_EXECUTION); only the hub's executor PDA for this DAO.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let life = p.lifecycle();
        let timing = config.voting();
        require!(timelock_complete(&life, &timing, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &timing, now), GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed: its proposer, or the DAO
    /// through a proposal. A challenger's unsettled bond can then be
    /// reclaimed.
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

    /// Replaces the rules (only through a passed proposal).
    pub fn update_config(ctx: Context<UpdateConfig>, config: OptimisticConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }

    /// Returns a vote record's rent once voting has closed.
    pub fn close_vote_record(ctx: Context<CloseVoteRecord>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &ctx.accounts.proposal;
        require!(now >= p.voting_ends_at || p.cancelled, GovError::VotingNotEnded);
        Ok(())
    }
}

/// Moves the bond out of the bond vault, signed by the governance PDA.
fn pay_out_bond<'info>(
    g: &Account<'info, Governance>,
    bond_vault: &InterfaceAccount<'info, TokenAccount>,
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
            TransferChecked {
                from: bond_vault.to_account_info(),
                mint: mint.to_account_info(),
                to: to.to_account_info(),
                authority: g.to_account_info(),
            },
            &[seeds],
        ),
        amount,
        mint.decimals,
    )
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

/// A DAO's rules. Times in seconds; bond and threshold in raw tokens.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct OptimisticConfig {
    /// How long anyone can challenge a new proposal.
    pub challenge_period: u32,
    /// Tokens a challenger posts (0 = free to challenge).
    pub challenge_bond: u64,
    /// Used only for challenged proposals.
    pub quorum_bps: u16,
    /// Used only for challenged proposals.
    pub approval_bps: u16,
    /// How long a challenge vote lasts.
    pub voting_period: u32,
    pub timelock: u32,
    pub execution_period: u32,
    pub proposal_threshold: u64,
}

impl OptimisticConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.challenge_period > 0, GovError::InvalidChallengePeriod);
        self.voting().validate()
    }

    /// The challenge vote's rules, in vortex-core's shape.
    pub fn voting(&self) -> vortex_core::VotingConfig {
        vortex_core::VotingConfig {
            quorum_bps: self.quorum_bps,
            approval_bps: self.approval_bps,
            voting_delay: 0,
            voting_period: self.voting_period,
            timelock: self.timelock,
            execution_period: self.execution_period,
            proposal_threshold: self.proposal_threshold,
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    /// Holds challengers' bonds until they're settled.
    pub bond_vault: Pubkey,
    pub config: OptimisticConfig,
    pub proposal_count: u64,
    pub total_deposited: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub governance: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub locked_until: i64,
    pub bump: u8,
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
    pub challenge_deadline: i64,
    pub challenged: bool,
    pub challenger: Pubkey,
    /// The bond actually posted (the config may change later).
    pub challenge_bond: u64,
    /// 0 unless challenged.
    pub voting_ends_at: i64,
    /// All deposits when proposed: what a challenge vote's quorum is measured against.
    pub quorum_base: u64,
    pub tally: Tally,
    pub bond_resolved: bool,
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
    pub bump: u8,
}

/// Where an optimistic proposal is. Same states as the EVM contract.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptimisticState {
    /// Unchallenged so far, window still open.
    ChallengeWindow,
    /// Challenged, vote in progress.
    Active,
    /// Ready to queue: window passed unchallenged, or the vote passed.
    Succeeded,
    Queued,
    /// The challenge vote struck it down.
    Defeated,
    Executed,
    Cancelled,
    Expired,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + 8 + 8 + 1 + 32 + 8 + 8 + 8 + Tally::INIT_SPACE + 1 + 8 + 1 + 1 + 1
    }

    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle {
            voting_starts_at: self.created_at,
            voting_ends_at: self.voting_ends_at,
            queued_at: self.queued_at,
            executed: self.executed,
            cancelled: self.cancelled,
        }
    }

    /// Whether a challenge vote passed (quorum and approval).
    pub fn passed(&self, config: &OptimisticConfig) -> bool {
        let voting = config.voting();
        has_quorum(&self.tally, self.quorum_base, &voting) && has_approval(&self.tally, &voting)
    }

    /// State at `now` (what clients show).
    pub fn state(&self, config: &OptimisticConfig, now: i64) -> OptimisticState {
        if self.cancelled {
            return OptimisticState::Cancelled;
        }
        if self.executed {
            return OptimisticState::Executed;
        }
        if self.queued_at != 0 {
            return if execution_expired(&self.lifecycle(), &config.voting(), now) {
                OptimisticState::Expired
            } else {
                OptimisticState::Queued
            };
        }
        if !self.challenged {
            return if now < self.challenge_deadline { OptimisticState::ChallengeWindow } else { OptimisticState::Succeeded };
        }
        if now < self.voting_ends_at {
            return OptimisticState::Active;
        }
        if passed(&self.tally, self.quorum_base, &config.voting()) {
            OptimisticState::Succeeded
        } else {
            OptimisticState::Defeated
        }
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
pub struct InitGovernance<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(init, payer = payer, space = 8 + Governance::INIT_SPACE, seeds = [GOVERNANCE_SEED, hub_dao.key().as_ref()], bump)]
    pub governance: Account<'info, Governance>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init, payer = payer, seeds = [VAULT_SEED, governance.key().as_ref()], bump,
        token::mint = mint, token::authority = governance, token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = payer, seeds = [BOND_VAULT_SEED, governance.key().as_ref()], bump,
        token::mint = mint, token::authority = governance, token::token_program = token_program,
    )]
    pub bond_vault: InterfaceAccount<'info, TokenAccount>,
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
        init_if_needed, payer = owner, space = 8 + Voter::INIT_SPACE,
        seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump,
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
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
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
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), proposer.key().as_ref()], bump)]
    pub voter: Option<Account<'info, Voter>>,
    #[account(
        init, payer = proposer, space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub proposal: Account<'info, Proposal>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Challenge<'info> {
    pub challenger: Signer<'info>,
    #[account(has_one = mint @ GovError::WrongDao, has_one = bond_vault @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, token::mint = mint, token::authority = challenger, token::token_program = token_program)]
    pub challenger_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub bond_vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CastVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = owner, space = 8 + VoteRecord::INIT_SPACE,
        seeds = [VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub vote_record: Account<'info, VoteRecord>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeUnchallenged<'info> {
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct FinalizeChallenge<'info> {
    #[account(has_one = mint @ GovError::WrongDao, has_one = bond_vault @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut)]
    pub bond_vault: InterfaceAccount<'info, TokenAccount>,
    /// Receives the bond if the proposal passed (a treasury-owned account).
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub treasury_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    /// Receives the bond if it failed (a challenger-owned account).
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub challenger_token_account: Option<InterfaceAccount<'info, TokenAccount>>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ReclaimBond<'info> {
    #[account(has_one = mint @ GovError::WrongDao, has_one = bond_vault @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut)]
    pub bond_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub challenger_token_account: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
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
        mut, close = owner,
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
pub struct ProposalCreated {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub challenge_deadline: i64,
}

#[event]
pub struct Challenged {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub challenger: Pubkey,
    pub bond: u64,
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
pub struct ChallengeSettled {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub passed: bool,
    pub bond_to: Pubkey,
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
    pub config: OptimisticConfig,
}
