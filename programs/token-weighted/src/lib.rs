//! Vortexes token-weighted governance: one deposited token, one vote. The
//! Solana counterpart of the EVM `Governance.sol`.
//!
//! The voting itself lives in `token_voting.rs`, shared with
//! vortex-quadratic; this file only sets the program's ID and how a deposit
//! turns into votes. Money and execution live in vortex-hub.

declare_id!("Hf9MbqsbSDXGugmKYAVSLe3Urt3tTUPq5wNEsJgrikV8");

/// Votes for a deposit of `deposit` raw tokens: one each.
pub fn vote_weight(deposit: u64) -> u64 {
    deposit
}

include!("token_voting.rs");

#[program]
pub mod vortex_token_weighted {
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
