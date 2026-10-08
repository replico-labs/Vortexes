//! Vortexes quadratic governance: a deposit of n tokens gives sqrt(n)
//! votes, so large holders count for less. The Solana counterpart of the
//! EVM `QuadraticGovernance.sol`; quorum is measured against
//! sqrt(all deposits), and the proposal threshold stays in raw tokens, as
//! there.
//!
//! Everything else is token-weighted's voting, shared through
//! `token_voting.rs` so the two can't drift apart.

declare_id!("ES1UEWvh4uHf2nwE3pYSWms8QNEXDn2JtSiNjtAWi3tR");

/// Votes for a deposit of `deposit` raw tokens: its integer square root.
pub fn vote_weight(deposit: u64) -> u64 {
    deposit.isqrt()
}

include!("../../token-weighted/src/token_voting.rs");

#[program]
pub mod vortex_quadratic {
    use super::*;

    /// Sets up a hub DAO in this program (creator after create_dao, or the
    /// treasury when switching to this model).
    pub fn init_governance(ctx: Context<InitGovernance>, config: VotingConfig) -> Result<()> {
        handlers::init_governance(ctx, config)
    }

    /// Deposits the DAO's token, adding voting power.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        handlers::deposit(ctx, amount)
    }

    /// Withdraws a deposit once the proposals voted on have closed.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        handlers::withdraw(ctx, amount)
    }

    /// Proposes instructions for the treasury to run if the vote passes.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        handlers::propose(ctx, id, metadata_uri, instructions)
    }

    /// Votes for, against or abstain.
    pub fn cast_vote(ctx: Context<CastVote>, choice: VoteChoice) -> Result<()> {
        handlers::cast_vote(ctx, choice)
    }

    /// Queues a passed proposal once voting has ended.
    pub fn queue(ctx: Context<Queue>) -> Result<()> {
        handlers::queue(ctx)
    }

    /// The hub's check before it runs a proposal; only the hub can call it.
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        handlers::confirm_execution(ctx, epoch)
    }

    /// Cancels a proposal (its proposer, or the DAO through a proposal).
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        handlers::cancel(ctx)
    }

    /// Replaces the voting rules (only through a passed proposal).
    pub fn update_config(ctx: Context<UpdateConfig>, config: VotingConfig) -> Result<()> {
        handlers::update_config(ctx, config)
    }

    /// Returns a vote record's rent once voting has closed.
    pub fn close_vote_record(ctx: Context<CloseVoteRecord>) -> Result<()> {
        handlers::close_vote_record(ctx)
    }
}
