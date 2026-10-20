//! Vortexes Sowellian governance, the Solana counterpart of the EVM
//! `SowellianGovernance.sol`: outcome-based governance. A proposal states
//! its success criteria up front; members approve it by token vote; a
//! pari-mutuel YES/NO market lets anyone back their prediction; the
//! proposal runs through the hub; after a measurement period the outcome
//! is resolved, by a Pyth price or by a human resolver who can be
//! challenged and, if challenged, settled by a token vote.
//!
//! - Members deposit the DAO's token; the deposit is their weight in
//!   approval and adjudication votes, locked until those votes close.
//!   Positions and bonds are paid separately, from members' wallets, so
//!   taking a position never shrinks anyone's vote.
//! - **Proposing** posts `proposal_bond` and the criteria: resolution by
//!   oracle (a Pyth feed id, a target in 18-decimal fixed point, and
//!   whether success is at or above it, or at or below) or by a human,
//!   and a measurement period counted from execution.
//! - **Approval vote** for `approval_voting_period` after
//!   `approval_voting_delay`, with quorum and threshold. Rejected: the bond
//!   goes to the treasury. Approved: positions open for `positions_window`.
//! - **Execution** through the hub, after the positions window plus
//!   `execution_timelock`, within `execution_period`. Never run in time:
//!   anyone voids it, positions are refunded and the bond returned.
//! - **Oracle resolution:** once the measurement period is over, anyone
//!   resolves it with a Pyth `PriceUpdateV2` account: owned by the Pyth
//!   Receiver, fully verified, for the proposal's feed, published after the
//!   measurement period ended and no older than `max_oracle_staleness`.
//! - **Human resolution:** anyone proposes the outcome with
//!   `resolution_bond`; for `challenge_period` anyone can challenge with
//!   `challenge_bond`. Unchallenged, it stands. Challenged, a token vote
//!   (`adjudication_*`) decides; success needs quorum and the threshold,
//!   otherwise it's a failure. Whoever was wrong loses their bond to the
//!   treasury; whoever was right gets theirs back.
//! - **Claims:** winners split the whole pool in proportion to their stake.
//!   Bonds are claimed by their owners and forfeits swept to the treasury,
//!   so no one's missing token account can block a resolution.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{validate_instructions, validate_uri, GovError, ProposalCore, StoredInstruction, GOVERNANCE_SEED, MAX_BPS};
use vortex_hub::{Dao as HubDao, DaoToken};

declare_id!("G99YpKvNXVQmTgvXX6XGt1z3LvbEyhhDCENwat1nsrx2");

pub const VAULT_SEED: &[u8] = b"vault";
pub const ESCROW_SEED: &[u8] = b"escrow";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const APPROVAL_VOTE_SEED: &[u8] = b"approval_vote";
pub const ADJUDICATION_VOTE_SEED: &[u8] = b"adjudication_vote";
pub const POSITION_SEED: &[u8] = b"position";

/// The Pyth Solana Receiver (pyth-solana-receiver-sdk 2.0.0), which owns every verified `PriceUpdateV2`.
pub const PYTH_RECEIVER: Pubkey = pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
/// Anchor discriminator of `PriceUpdateV2`: sha256("account:PriceUpdateV2")[..8].
pub const PRICE_UPDATE_V2: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

#[program]
pub mod vortex_sowellian {
    use super::*;

    /// Sets up a hub DAO here: rules, token, the deposit vault and the escrow
    /// for bonds and positions. The creator right after create_dao, or the
    /// treasury when switching here.
    pub fn init_governance(ctx: Context<InitGovernance>, config: SowellianConfig) -> Result<()> {
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
        g.escrow = ctx.accounts.escrow.key();
        g.config = config;
        g.proposal_count = 0;
        g.total_deposited = 0;
        g.owed_treasury = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint });
        Ok(())
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        pay_in(&ctx.accounts.owner_token_account, &ctx.accounts.vault, &ctx.accounts.mint, &ctx.accounts.owner, &ctx.accounts.token_program, amount)?;
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

    /// Withdraws a deposit once the votes it was used in have closed.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let voter = &mut ctx.accounts.voter;
        require!(now >= voter.locked_until, GovError::TokensLocked);
        require!(amount <= voter.amount, GovError::InsufficientDeposit);
        voter.amount -= amount;
        let g = &mut ctx.accounts.governance;
        g.total_deposited -= amount;
        pay_out(g, &ctx.accounts.vault, &ctx.accounts.owner_token_account, &ctx.accounts.mint, &ctx.accounts.token_program, amount)?;
        emit!(Withdrawn { governance: g.key(), owner: voter.owner, amount, total: voter.amount });
        Ok(())
    }

    /// Proposes `instructions` with their success criteria, posting the
    /// proposal bond. `id` must be proposal_count + 1.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>, criteria: Criteria) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        validate_instructions(&instructions, &g.treasury)?;
        criteria.validate()?;
        let bond = g.config.proposal_bond;
        if bond > 0 {
            pay_in(&ctx.accounts.proposer_token_account, &ctx.accounts.escrow, &ctx.accounts.mint, &ctx.accounts.proposer, &ctx.accounts.token_program, bond)?;
        }
        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions };
        p.governance = g.key();
        p.id = id;
        p.proposer = ctx.accounts.proposer.key();
        p.metadata_uri = metadata_uri;
        p.criteria = criteria;
        p.phase = Phase::ApprovalVoting;
        p.approval_starts_at = now + g.config.approval_voting_delay as i64;
        p.approval_ends_at = p.approval_starts_at + g.config.approval_voting_period as i64;
        p.approval_quorum_base = g.total_deposited;
        p.approval = Votes::default();
        p.proposal_bond = bond;
        p.bump = ctx.bumps.proposal;
        emit!(ProposalCreated {
            hub_dao: g.hub_dao,
            proposal: p.key(),
            id,
            proposer: p.proposer,
            metadata_uri: p.metadata_uri.clone(),
            approval_starts_at: p.approval_starts_at,
            approval_ends_at: p.approval_ends_at,
        });
        Ok(())
    }

    /// Votes on whether the proposal and its criteria go ahead, weighted by deposit.
    pub fn cast_approval_vote(ctx: Context<CastApprovalVote>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::ApprovalVoting, GovError::WrongPhase);
        require!(now >= p.approval_starts_at && now < p.approval_ends_at, GovError::ProposalNotActive);
        let voter = &mut ctx.accounts.voter;
        require!(voter.amount > 0, GovError::NoVotingPower);
        let weight = voter.amount;
        match choice {
            VoteChoice::For => p.approval.for_votes += weight,
            VoteChoice::Against => p.approval.against_votes += weight,
            VoteChoice::Abstain => p.approval.abstain_votes += weight,
        }
        voter.locked_until = voter.locked_until.max(p.approval_ends_at);
        let r = &mut ctx.accounts.vote_record;
        r.proposal = p.key();
        r.voter = voter.owner;
        r.weight = weight;
        r.bump = ctx.bumps.vote_record;
        emit!(ApprovalVoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: voter.owner, choice, weight });
        Ok(())
    }

    /// Settles the approval vote once it's over. Anyone.
    pub fn finalize_approval(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let c = g.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::ApprovalVoting, GovError::WrongPhase);
        require!(now >= p.approval_ends_at, GovError::VotingNotEnded);
        let v = p.approval;
        let participation = v.for_votes as u128 + v.against_votes as u128 + v.abstain_votes as u128;
        let quorum = p.approval_quorum_base > 0 && participation * MAX_BPS as u128 >= p.approval_quorum_base as u128 * c.approval_quorum_bps as u128;
        let decisive = v.for_votes as u128 + v.against_votes as u128;
        let approved = quorum && decisive > 0 && v.for_votes as u128 * MAX_BPS as u128 >= decisive * c.approval_threshold_bps as u128;
        if approved {
            p.phase = Phase::PositionsOpen;
            p.positions_deadline = now + c.positions_window as i64;
        } else {
            p.phase = Phase::Rejected;
            g.owed_treasury += p.proposal_bond;
        }
        emit!(ApprovalFinalized { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, approved, positions_deadline: p.positions_deadline });
        Ok(())
    }

    /// Backs YES (the criteria will be met) or NO with `amount` of the DAO's token.
    pub fn take_position(ctx: Context<TakePosition>, side: Side, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::PositionsOpen, GovError::WrongPhase);
        require!(now < p.positions_deadline, GovError::PositionsClosed);
        pay_in(&ctx.accounts.owner_token_account, &ctx.accounts.escrow, &ctx.accounts.mint, &ctx.accounts.owner, &ctx.accounts.token_program, amount)?;
        let pos = &mut ctx.accounts.position;
        if pos.owner == Pubkey::default() {
            pos.proposal = p.key();
            pos.owner = ctx.accounts.owner.key();
            pos.bump = ctx.bumps.position;
        }
        match side {
            Side::Yes => {
                pos.yes = pos.yes.checked_add(amount).ok_or(GovError::Overflow)?;
                p.yes_pool = p.yes_pool.checked_add(amount).ok_or(GovError::Overflow)?;
            }
            Side::No => {
                pos.no = pos.no.checked_add(amount).ok_or(GovError::Overflow)?;
                p.no_pool = p.no_pool.checked_add(amount).ok_or(GovError::Overflow)?;
            }
        }
        emit!(PositionTaken { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, owner: pos.owner, side, amount });
        Ok(())
    }

    /// The hub's check before it runs a proposal (CONFIRM_EXECUTION): after
    /// the positions window and the timelock, within the execution period.
    /// Starts the measurement period.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(p.phase == Phase::PositionsOpen, GovError::ProposalNotExecutable);
        let executable_at = p.positions_deadline + c.execution_timelock as i64;
        require!(now >= executable_at, GovError::ProposalNotExecutable);
        require!(now < executable_at + c.execution_period as i64, GovError::ProposalExpired);
        p.phase = Phase::Executed;
        p.executed_at = now;
        p.measurement_deadline = now + p.criteria.measurement_period as i64;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, measurement_deadline: p.measurement_deadline });
        Ok(())
    }

    /// An approved proposal nobody ran within its execution period: void,
    /// positions refunded, bond returned. Anyone.
    pub fn void_expired(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::PositionsOpen, GovError::WrongPhase);
        require!(now >= p.positions_deadline + c.execution_timelock as i64 + c.execution_period as i64, GovError::ProposalNotExecutable);
        p.phase = Phase::Cancelled;
        p.owed_proposer = p.proposal_bond;
        emit!(ProposalVoided { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Resolves an oracle-track proposal with a Pyth price. Anyone, once the
    /// measurement period is over.
    pub fn resolve_via_oracle(ctx: Context<ResolveViaOracle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::Executed, GovError::WrongPhase);
        require!(p.criteria.method == ResolutionMethod::Oracle, GovError::WrongResolutionMethod);
        require!(now >= p.measurement_deadline, GovError::MeasurementNotOver);
        let price = read_pyth(&ctx.accounts.price_update, &p.criteria.feed_id)?;
        require!(price.publish_time >= p.measurement_deadline, GovError::StaleOracleData);
        require!(now - price.publish_time <= c.max_oracle_staleness as i64, GovError::StaleOracleData);
        let observed = to_18_decimals(price.price, price.exponent)?;
        let success = if p.criteria.target_is_minimum { observed >= p.criteria.target_value } else { observed <= p.criteria.target_value };
        p.final_outcome = if success { Outcome::Success } else { Outcome::Failure };
        p.phase = Phase::Finalized;
        p.owed_proposer = p.proposal_bond;
        emit!(ResolvedViaOracle { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, observed, publish_time: price.publish_time, outcome: p.final_outcome });
        Ok(())
    }

    /// Proposes the outcome of a human-track proposal, posting the resolution bond.
    pub fn propose_resolution(ctx: Context<BondAction>, outcome: Outcome) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::Executed, GovError::WrongPhase);
        require!(p.criteria.method == ResolutionMethod::Human, GovError::WrongResolutionMethod);
        require!(now >= p.measurement_deadline, GovError::MeasurementNotOver);
        require!(outcome != Outcome::Unresolved, GovError::InvalidConfig);
        if c.resolution_bond > 0 {
            pay_in(&ctx.accounts.owner_token_account, &ctx.accounts.escrow, &ctx.accounts.mint, &ctx.accounts.owner, &ctx.accounts.token_program, c.resolution_bond)?;
        }
        p.resolver = ctx.accounts.owner.key();
        p.proposed_outcome = outcome;
        p.resolution_bond = c.resolution_bond;
        p.challenge_deadline = now + c.challenge_period as i64;
        p.phase = Phase::ResolutionProposed;
        emit!(ResolutionProposed { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, resolver: p.resolver, outcome, challenge_deadline: p.challenge_deadline });
        Ok(())
    }

    /// Disputes the proposed outcome, posting the challenge bond. Starts the adjudication vote.
    pub fn challenge_resolution(ctx: Context<BondAction>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let total = ctx.accounts.governance.total_deposited;
        let c = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::ResolutionProposed, GovError::WrongPhase);
        require!(now < p.challenge_deadline, GovError::ChallengeWindowClosed);
        if c.challenge_bond > 0 {
            pay_in(&ctx.accounts.owner_token_account, &ctx.accounts.escrow, &ctx.accounts.mint, &ctx.accounts.owner, &ctx.accounts.token_program, c.challenge_bond)?;
        }
        p.challenger = ctx.accounts.owner.key();
        p.challenge_bond = c.challenge_bond;
        p.adjudication_ends_at = now + c.adjudication_voting_period as i64;
        p.adjudication_quorum_base = total;
        p.phase = Phase::Adjudicating;
        emit!(ResolutionChallenged { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, challenger: p.challenger, adjudication_ends_at: p.adjudication_ends_at });
        Ok(())
    }

    /// Nobody challenged in time: the proposed outcome stands. Anyone.
    pub fn finalize_unchallenged(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::ResolutionProposed, GovError::WrongPhase);
        require!(now >= p.challenge_deadline, GovError::ChallengeWindowOpen);
        p.final_outcome = p.proposed_outcome;
        p.phase = Phase::Finalized;
        p.owed_resolver = p.resolution_bond;
        p.owed_proposer = p.proposal_bond;
        emit!(OutcomeFinalized { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, outcome: p.final_outcome, adjudicated: false });
        Ok(())
    }

    /// Votes Success or Failure in a challenged resolution, weighted by deposit.
    pub fn cast_adjudication_vote(ctx: Context<CastAdjudicationVote>, outcome: Outcome) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::Adjudicating, GovError::WrongPhase);
        require!(now < p.adjudication_ends_at, GovError::ProposalNotActive);
        require!(outcome != Outcome::Unresolved, GovError::InvalidConfig);
        let voter = &mut ctx.accounts.voter;
        require!(voter.amount > 0, GovError::NoVotingPower);
        let weight = voter.amount;
        if outcome == Outcome::Success {
            p.success_votes += weight;
        } else {
            p.failure_votes += weight;
        }
        voter.locked_until = voter.locked_until.max(p.adjudication_ends_at);
        let r = &mut ctx.accounts.vote_record;
        r.proposal = p.key();
        r.voter = voter.owner;
        r.weight = weight;
        r.bump = ctx.bumps.vote_record;
        emit!(AdjudicationVoteCast { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, voter: voter.owner, outcome, weight });
        Ok(())
    }

    /// Settles a challenged resolution once the vote is over. Success needs
    /// quorum and the threshold; anything else is a failure. Anyone.
    pub fn finalize_adjudication(ctx: Context<Settle>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let c = g.config;
        let p = &mut ctx.accounts.proposal;
        require!(p.phase == Phase::Adjudicating, GovError::WrongPhase);
        require!(now >= p.adjudication_ends_at, GovError::VotingNotEnded);
        let participation = p.success_votes as u128 + p.failure_votes as u128;
        let quorum = p.adjudication_quorum_base > 0 && participation * MAX_BPS as u128 >= p.adjudication_quorum_base as u128 * c.adjudication_quorum_bps as u128;
        let threshold = participation > 0 && p.success_votes as u128 * MAX_BPS as u128 >= participation * c.adjudication_threshold_bps as u128;
        p.final_outcome = if quorum && threshold { Outcome::Success } else { Outcome::Failure };
        p.phase = Phase::Finalized;
        if p.final_outcome == p.proposed_outcome {
            p.owed_resolver = p.resolution_bond;
            g.owed_treasury += p.challenge_bond;
        } else {
            p.owed_challenger = p.challenge_bond;
            g.owed_treasury += p.resolution_bond;
        }
        p.owed_proposer = p.proposal_bond;
        emit!(OutcomeFinalized { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, outcome: p.final_outcome, adjudicated: true });
        Ok(())
    }

    /// Pays out a position: winners split the whole pool pro rata; on a void
    /// or cancelled proposal everyone gets their stake back.
    pub fn claim_position(ctx: Context<ClaimPosition>) -> Result<()> {
        let p = &ctx.accounts.proposal;
        let pos = &mut ctx.accounts.position;
        require!(!pos.claimed, GovError::AlreadyClaimed);
        let payout: u64 = match p.phase {
            Phase::Finalized => {
                let (stake, winning) = if p.final_outcome == Outcome::Success { (pos.yes, p.yes_pool) } else { (pos.no, p.no_pool) };
                require!(stake > 0, GovError::NothingToClaim);
                (stake as u128 * (p.yes_pool as u128 + p.no_pool as u128) / winning as u128) as u64
            }
            Phase::Cancelled => pos.yes + pos.no,
            _ => return err!(GovError::WrongPhase),
        };
        pos.claimed = true;
        pay_out(&ctx.accounts.governance, &ctx.accounts.escrow, &ctx.accounts.owner_token_account, &ctx.accounts.mint, &ctx.accounts.token_program, payout)?;
        emit!(PositionClaimed { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, owner: pos.owner, payout });
        Ok(())
    }

    /// The proposer, resolver or challenger takes back what they're owed from this proposal.
    pub fn claim_bond(ctx: Context<ClaimBond>) -> Result<()> {
        let who = ctx.accounts.owner.key();
        let p = &mut ctx.accounts.proposal;
        let mut amount = 0u64;
        if who == p.proposer {
            amount += std::mem::take(&mut p.owed_proposer);
        }
        if who == p.resolver {
            amount += std::mem::take(&mut p.owed_resolver);
        }
        if who == p.challenger {
            amount += std::mem::take(&mut p.owed_challenger);
        }
        require!(amount > 0, GovError::NothingToClaim);
        pay_out(&ctx.accounts.governance, &ctx.accounts.escrow, &ctx.accounts.owner_token_account, &ctx.accounts.mint, &ctx.accounts.token_program, amount)?;
        emit!(BondClaimed { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, owner: who, amount });
        Ok(())
    }

    /// Sends forfeited bonds to the treasury's token account. Anyone.
    pub fn sweep_to_treasury(ctx: Context<SweepToTreasury>) -> Result<()> {
        let g = &mut ctx.accounts.governance;
        let amount = std::mem::take(&mut g.owed_treasury);
        require!(amount > 0, GovError::NothingToClaim);
        pay_out(g, &ctx.accounts.escrow, &ctx.accounts.treasury_token_account, &ctx.accounts.mint, &ctx.accounts.token_program, amount)?;
        emit!(SweptToTreasury { hub_dao: g.hub_dao, amount });
        Ok(())
    }

    /// Cancels a proposal before it runs: its proposer while the approval
    /// vote is open, or the DAO through a proposal any time before execution.
    /// Positions are refunded and the bond returned.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        let who = ctx.accounts.authority.key();
        let allowed = (who == p.proposer && p.phase == Phase::ApprovalVoting)
            || (who == g.treasury && matches!(p.phase, Phase::ApprovalVoting | Phase::PositionsOpen));
        require!(allowed, GovError::Unauthorized);
        p.phase = Phase::Cancelled;
        p.owed_proposer = p.proposal_bond;
        emit!(ProposalCancelled { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, by: who });
        Ok(())
    }

    /// Replaces the rules (only through a passed proposal).
    pub fn update_config(ctx: Context<UpdateConfig>, config: SowellianConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }
}

// ---------------------------------------------------------------
// HELPERS
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

pub struct PythPrice {
    pub price: i64,
    pub exponent: i32,
    pub publish_time: i64,
}

/// A fully verified Pyth `PriceUpdateV2` for `feed_id`. Layout (pyth-solana-receiver-sdk 2.0.0):
/// discriminator, write_authority (32), verification_level (Partial = 0 + u8 | Full = 1),
/// feed_id (32), price i64, conf u64, exponent i32, publish_time i64, ...
fn read_pyth(account: &UncheckedAccount, feed_id: &[u8; 32]) -> Result<PythPrice> {
    require_keys_eq!(*account.owner, PYTH_RECEIVER, GovError::WrongOracleAccount);
    let data = account.try_borrow_data()?;
    require!(data.len() >= 8 + 32 + 1 && data[..8] == PRICE_UPDATE_V2, GovError::WrongOracleAccount);
    // Only fully verified updates.
    require!(data[40] == 1, GovError::WrongOracleAccount);
    let m = &data[41..];
    require!(m.len() >= 32 + 8 + 8 + 4 + 8, GovError::WrongOracleAccount);
    require!(m[..32] == feed_id[..], GovError::WrongOracleAccount);
    let price = i64::from_le_bytes(m[32..40].try_into().unwrap());
    let exponent = i32::from_le_bytes(m[48..52].try_into().unwrap());
    let publish_time = i64::from_le_bytes(m[52..60].try_into().unwrap());
    Ok(PythPrice { price, exponent, publish_time })
}

/// `price × 10^exponent` in 18-decimal fixed point (the EVM version's unit).
pub fn to_18_decimals(price: i64, exponent: i32) -> Result<i128> {
    let shift = 18 + exponent;
    require!((0..=36).contains(&shift), GovError::WrongOracleAccount);
    (price as i128).checked_mul(10i128.pow(shift as u32)).ok_or(error!(GovError::Overflow))
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct SowellianConfig {
    /// Posted with every proposal (raw tokens); lost to the treasury if the approval vote fails.
    pub proposal_bond: u64,
    pub approval_voting_delay: u32,
    pub approval_voting_period: u32,
    pub approval_quorum_bps: u16,
    pub approval_threshold_bps: u16,
    /// How long positions can be taken after approval.
    pub positions_window: u32,
    /// Wait after the positions window before the proposal can run.
    pub execution_timelock: u32,
    /// How long after that it can still run.
    pub execution_period: u32,
    pub resolution_bond: u64,
    pub challenge_period: u32,
    pub challenge_bond: u64,
    pub adjudication_voting_period: u32,
    pub adjudication_quorum_bps: u16,
    pub adjudication_threshold_bps: u16,
    /// Oldest Pyth price (seconds) an oracle resolution accepts.
    pub max_oracle_staleness: u32,
}

impl SowellianConfig {
    pub fn validate(&self) -> Result<()> {
        let bps = |b: u16| b >= 1 && b <= MAX_BPS;
        require!(self.approval_voting_period > 0 && self.positions_window > 0 && self.execution_period > 0, GovError::InvalidConfig);
        require!(self.challenge_period > 0 && self.adjudication_voting_period > 0 && self.max_oracle_staleness > 0, GovError::InvalidConfig);
        require!(bps(self.approval_quorum_bps) && bps(self.adjudication_quorum_bps), GovError::InvalidQuorum);
        require!(bps(self.approval_threshold_bps) && bps(self.adjudication_threshold_bps), GovError::InvalidApprovalThreshold);
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Criteria {
    pub method: ResolutionMethod,
    /// Oracle track: the Pyth price feed id.
    pub feed_id: [u8; 32],
    /// Oracle track: the target, 18-decimal fixed point.
    pub target_value: i128,
    /// Success at or above the target (true), or at or below it (false).
    pub target_is_minimum: bool,
    /// Seconds after execution before the outcome can be resolved.
    pub measurement_period: u32,
}

impl Criteria {
    pub fn validate(&self) -> Result<()> {
        require!(self.measurement_period > 0, GovError::InvalidConfig);
        if self.method == ResolutionMethod::Oracle {
            require!(self.feed_id != [0; 32], GovError::InvalidConfig);
        }
        Ok(())
    }
}

/// Same order as the EVM contract's Phase (ResolutionPending is unused there too).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace, Default)]
pub enum Phase {
    #[default]
    ApprovalVoting,
    Rejected,
    PositionsOpen,
    Executed,
    ResolutionPending,
    ResolutionProposed,
    Adjudicating,
    Finalized,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace, Default)]
pub enum Outcome {
    #[default]
    Unresolved,
    Success,
    Failure,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum ResolutionMethod {
    Oracle,
    Human,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum Side {
    Yes,
    No,
}

/// Same order as the EVM contracts' VoteType.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum VoteChoice {
    Against,
    For,
    Abstain,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace, Default)]
pub struct Votes {
    pub for_votes: u64,
    pub against_votes: u64,
    pub abstain_votes: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    pub mint: Pubkey,
    /// Members' deposits (voting weight).
    pub vault: Pubkey,
    /// Bonds and positions.
    pub escrow: Pubkey,
    pub config: SowellianConfig,
    pub proposal_count: u64,
    pub total_deposited: u64,
    /// Forfeited bonds waiting to be swept to the treasury.
    pub owed_treasury: u64,
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
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    pub criteria: Criteria,
    pub phase: Phase,
    pub approval_starts_at: i64,
    pub approval_ends_at: i64,
    /// All deposits when proposed: the approval vote's quorum base.
    pub approval_quorum_base: u64,
    pub approval: Votes,
    pub positions_deadline: i64,
    pub executed_at: i64,
    pub measurement_deadline: i64,
    pub proposal_bond: u64,
    pub yes_pool: u64,
    pub no_pool: u64,
    pub resolver: Pubkey,
    pub resolution_bond: u64,
    pub proposed_outcome: Outcome,
    pub challenge_deadline: i64,
    pub challenger: Pubkey,
    pub challenge_bond: u64,
    pub adjudication_ends_at: i64,
    /// All deposits when challenged: the adjudication vote's quorum base.
    pub adjudication_quorum_base: u64,
    pub success_votes: u64,
    pub failure_votes: u64,
    pub final_outcome: Outcome,
    /// Bonds waiting to be claimed.
    pub owed_proposer: u64,
    pub owed_resolver: u64,
    pub owed_challenger: u64,
    pub bump: u8,
}

impl Proposal {
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction]) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + Criteria::INIT_SPACE
            + Phase::INIT_SPACE
            + 8 * 3 + Votes::INIT_SPACE
            + 8 * 3
            + 8 * 3
            + 32 + 8 + Outcome::INIT_SPACE + 8
            + 32 + 8 + 8 + 8
            + 8 * 2 + Outcome::INIT_SPACE
            + 8 * 3
            + 1
    }
}

/// One per member per proposal per vote, so nobody votes twice.
#[account]
#[derive(InitSpace)]
pub struct VoteRecord {
    pub proposal: Pubkey,
    pub voter: Pubkey,
    pub weight: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Position {
    pub proposal: Pubkey,
    pub owner: Pubkey,
    pub yes: u64,
    pub no: u64,
    pub claimed: bool,
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
    pub hub_dao: Account<'info, HubDao>,
    #[account(init, payer = payer, space = 8 + Governance::INIT_SPACE, seeds = [GOVERNANCE_SEED, hub_dao.key().as_ref()], bump)]
    pub governance: Account<'info, Governance>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// The DAO's token on record at the hub (set_dao_token): setting up -
    /// a switch included - only works with that mint.
    #[account(
        seeds = [vortex_hub::TOKEN_SEED, hub_dao.key().as_ref()],
        bump = dao_token.bump,
        seeds::program = vortex_hub::ID,
        constraint = dao_token.mint == mint.key() @ GovError::WrongMint,
    )]
    pub dao_token: Account<'info, DaoToken>,
    #[account(
        init, payer = payer, seeds = [VAULT_SEED, governance.key().as_ref()], bump,
        token::mint = mint, token::authority = governance, token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = payer, seeds = [ESCROW_SEED, governance.key().as_ref()], bump,
        token::mint = mint, token::authority = governance, token::token_program = token_program,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
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
    #[account(mut, has_one = hub_dao @ GovError::WrongDao, has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(
        init, payer = proposer, space = Proposal::space(&metadata_uri, &instructions),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, token::mint = mint, token::authority = proposer, token::token_program = token_program)]
    pub proposer_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CastApprovalVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = owner, space = 8 + VoteRecord::INIT_SPACE,
        seeds = [APPROVAL_VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub vote_record: Account<'info, VoteRecord>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CastAdjudicationVote<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = owner, space = 8 + VoteRecord::INIT_SPACE,
        seeds = [ADJUDICATION_VOTE_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub vote_record: Account<'info, VoteRecord>,
    pub system_program: Program<'info, System>,
}

/// Phase changes anyone can trigger.
#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

#[derive(Accounts)]
pub struct TakePosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(
        init_if_needed, payer = owner, space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub position: Account<'info, Position>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
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
pub struct ResolveViaOracle<'info> {
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    /// CHECK: owner, layout, verification, feed and age checked by read_pyth.
    pub price_update: UncheckedAccount<'info>,
}

/// Posting a resolution or challenge bond.
#[derive(Accounts)]
pub struct BondAction<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimPosition<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, seeds = [POSITION_SEED, proposal.key().as_ref(), owner.key().as_ref()], bump = position.bump)]
    pub position: Account<'info, Position>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimBond<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SweepToTreasury<'info> {
    #[account(mut, has_one = mint @ GovError::WrongDao, has_one = escrow @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    #[account(mut, token::mint = mint, token::authority = governance.treasury, token::token_program = token_program)]
    pub treasury_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub escrow: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
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
    pub approval_starts_at: i64,
    pub approval_ends_at: i64,
}

#[event]
pub struct ApprovalVoteCast {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
}

#[event]
pub struct ApprovalFinalized {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub approved: bool,
    pub positions_deadline: i64,
}

#[event]
pub struct PositionTaken {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub owner: Pubkey,
    pub side: Side,
    pub amount: u64,
}

#[event]
pub struct ProposalExecuted {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub measurement_deadline: i64,
}

#[event]
pub struct ProposalVoided {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
}

#[event]
pub struct ResolvedViaOracle {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    /// The price, 18-decimal fixed point.
    pub observed: i128,
    pub publish_time: i64,
    pub outcome: Outcome,
}

#[event]
pub struct ResolutionProposed {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub resolver: Pubkey,
    pub outcome: Outcome,
    pub challenge_deadline: i64,
}

#[event]
pub struct ResolutionChallenged {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub challenger: Pubkey,
    pub adjudication_ends_at: i64,
}

#[event]
pub struct AdjudicationVoteCast {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub outcome: Outcome,
    pub weight: u64,
}

#[event]
pub struct OutcomeFinalized {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub outcome: Outcome,
    pub adjudicated: bool,
}

#[event]
pub struct PositionClaimed {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub owner: Pubkey,
    pub payout: u64,
}

#[event]
pub struct BondClaimed {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct SweptToTreasury {
    pub hub_dao: Pubkey,
    pub amount: u64,
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
    pub config: SowellianConfig,
}
