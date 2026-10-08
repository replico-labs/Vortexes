//! Vortexes delegate governance, the Solana counterpart of the EVM
//! `DelegateGovernance.sol`: members elect a council for a term; only the
//! council proposes and votes on what the treasury does. Members' direct
//! levers are elections and recalls.
//!
//! - Members deposit the DAO's token into its vault; the deposit is their
//!   weight in elections and recalls.
//! - **Elections:** once the council's term has ended, anyone starts one.
//!   For `candidacy_period`, members with `candidacy_threshold` deposited
//!   declare; then for `election_voting_period`, each member votes for up
//!   to `council_size` candidates, each getting the member's full deposit.
//!   Afterwards anyone finalizes: the top `council_size` candidates with
//!   votes form the new council for `term_length` (ties go to whoever
//!   declared first). If nobody got a vote, the old council stays and a new
//!   election can start straight away.
//! - **Council proposals:** council members propose; after `voting_delay`,
//!   members vote for, against or abstain (one vote per seat) for
//!   `voting_period`. A proposal passes with `council_quorum` votes and
//!   `council_approval_bps` of for / (for + against), then runs through the
//!   hub after `timelock`, within `execution_period`.
//! - **Recall:** a member with `candidacy_threshold` deposited starts a
//!   recall of one council member; a token vote decides it with
//!   `recall_quorum_bps` and `recall_approval_bps`. A removed member's seat
//!   stays empty until the next election.
//! - No snapshots on Solana: a member's deposit stays locked until the
//!   elections and recalls they voted in (or stood in) have closed.
//!
//! Unlike the EVM version: a proposal only counts votes from people who are
//! on the council when it's queued and run (a recalled member's votes stop
//! counting), and proposals made by one council can't be queued or run
//! once a new council is elected.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execution_expired, proposal_state, timelock_complete, validate_instructions, validate_uri, GovError, Lifecycle,
    ProposalCore, ProposalState, StoredInstruction, Tally, VotingConfig, GOVERNANCE_SEED, MAX_BPS,
};
use vortex_hub::Dao as HubDao;

declare_id!("6TVZxo8LeXTsgH8iC39SD1AUeNUYJx3kGf71bEmsUvk2");

pub const VAULT_SEED: &[u8] = b"vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const ELECTION_SEED: &[u8] = b"election";
pub const BALLOT_SEED: &[u8] = b"ballot";
pub const RECALL_SEED: &[u8] = b"recall";
pub const RECALL_VOTE_SEED: &[u8] = b"recall_vote";
/// Most seats a council can have.
pub const MAX_COUNCIL: usize = 15;
/// Most candidates one election can take.
pub const MAX_CANDIDATES: usize = 32;

#[program]
pub mod vortex_delegate {
    use super::*;

    /// Sets up a hub DAO in this program: rules, token, vault and the first
    /// council (exactly `council_size` members), whose term starts now. The
    /// creator right after `vortex_hub::create_dao`, or the DAO's treasury
    /// (inside a passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, council: Vec<Pubkey>, config: DelegateConfig) -> Result<()> {
        config.validate()?;
        require!(council.len() == config.council_size as usize, GovError::InvalidConfig);
        for (i, m) in council.iter().enumerate() {
            require!(*m != Pubkey::default(), GovError::InvalidConfig);
            require!(!council[..i].contains(m), GovError::DuplicateCandidate);
        }
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
        g.council = council;
        g.council_term = 1;
        g.term_ends_at = Clock::get()?.unix_timestamp + config.term_length as i64;
        g.election_count = 0;
        g.election_open = false;
        g.proposal_count = 0;
        g.recall_count = 0;
        g.total_deposited = 0;
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint, council: g.council.clone(), term_ends_at: g.term_ends_at });
        Ok(())
    }

    /// Deposits the DAO's token, adding election and recall weight.
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

    /// Withdraws a deposit once the elections and recalls the owner voted
    /// or stood in have closed.
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

    // ---- elections ----

    /// Opens an election once the council's term is over. Anyone.
    /// `id` must be election_count + 1.
    pub fn start_election(ctx: Context<StartElection>, id: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        require!(!g.election_open, GovError::ElectionInProgress);
        require!(now >= g.term_ends_at, GovError::TermNotOver);
        require!(id == g.election_count + 1, GovError::WrongDao);
        g.election_count = id;
        g.election_open = true;
        let e = &mut ctx.accounts.election;
        e.governance = g.key();
        e.id = id;
        e.candidacy_ends_at = now + g.config.candidacy_period as i64;
        e.voting_ends_at = e.candidacy_ends_at + g.config.election_voting_period as i64;
        e.candidates = Vec::new();
        e.finalized = false;
        e.bump = ctx.bumps.election;
        emit!(ElectionStarted { hub_dao: g.hub_dao, election: e.key(), id, candidacy_ends_at: e.candidacy_ends_at, voting_ends_at: e.voting_ends_at });
        Ok(())
    }

    /// Stands in an open election, during its candidacy window. Needs
    /// `candidacy_threshold` deposited, which stays locked until voting ends.
    pub fn declare_candidacy(ctx: Context<DeclareCandidacy>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let e = &mut ctx.accounts.election;
        let voter = &mut ctx.accounts.voter;
        require!(now < e.candidacy_ends_at, GovError::CandidacyClosed);
        require!(voter.amount >= g.config.candidacy_threshold, GovError::ProposalThresholdNotMet);
        let key = voter.owner;
        require!(!e.candidates.iter().any(|c| c.key == key), GovError::AlreadyCandidate);
        require!(e.candidates.len() < MAX_CANDIDATES, GovError::TooManyCandidates);
        e.candidates.push(Candidate { key, votes: 0 });
        voter.locked_until = voter.locked_until.max(e.voting_ends_at);
        emit!(CandidacyDeclared { hub_dao: g.hub_dao, election: e.key(), id: e.id, candidate: key });
        Ok(())
    }

    /// Votes for 1 to `council_size` different candidates; each gets the
    /// member's whole deposit. Once per election; locks the deposit until
    /// voting ends.
    pub fn vote_in_election(ctx: Context<VoteInElection>, candidates: Vec<Pubkey>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let e = &mut ctx.accounts.election;
        require!(now >= e.candidacy_ends_at && now < e.voting_ends_at, GovError::ElectionVotingNotOpen);
        require!(!candidates.is_empty() && candidates.len() <= g.config.council_size as usize, GovError::InvalidBallot);
        let voter = &mut ctx.accounts.voter;
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);
        for (i, c) in candidates.iter().enumerate() {
            require!(!candidates[..i].contains(c), GovError::DuplicateCandidate);
            let entry = e.candidates.iter_mut().find(|x| x.key == *c).ok_or(GovError::NotCandidate)?;
            entry.votes = entry.votes.checked_add(weight).ok_or(GovError::Overflow)?;
        }
        voter.locked_until = voter.locked_until.max(e.voting_ends_at);
        let ballot = &mut ctx.accounts.ballot;
        ballot.election = e.key();
        ballot.voter = voter.owner;
        ballot.weight = weight;
        ballot.bump = ctx.bumps.ballot;
        emit!(ElectionVoteCast { hub_dao: g.hub_dao, election: e.key(), id: e.id, voter: voter.owner, candidates, weight });
        Ok(())
    }

    /// Closes an election once voting ends and seats the winners. Anyone.
    pub fn finalize_election(ctx: Context<FinalizeElection>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let e = &mut ctx.accounts.election;
        require!(now >= e.voting_ends_at, GovError::VotingNotEnded);
        require!(!e.finalized, GovError::AlreadyFinalized);
        e.finalized = true;
        g.election_open = false;

        // Stable sort: equal votes keep declaration order.
        let mut ranked: Vec<&Candidate> = e.candidates.iter().filter(|c| c.votes > 0).collect();
        ranked.sort_by(|a, b| b.votes.cmp(&a.votes));
        let winners: Vec<Pubkey> = ranked.iter().take(g.config.council_size as usize).map(|c| c.key).collect();
        if winners.is_empty() {
            // Nobody got a vote: the current council carries on until a
            // new election (which can start right away) seats someone.
            emit!(ElectionFinalized { hub_dao: g.hub_dao, election: e.key(), id: e.id, council: g.council.clone(), term_ends_at: g.term_ends_at, seated: false });
            return Ok(());
        }
        g.council = winners;
        g.council_term += 1;
        g.term_ends_at = now + g.config.term_length as i64;
        emit!(ElectionFinalized { hub_dao: g.hub_dao, election: e.key(), id: e.id, council: g.council.clone(), term_ends_at: g.term_ends_at, seated: true });
        Ok(())
    }

    // ---- council proposals ----

    /// A council member proposes instructions for the treasury to run.
    /// `id` must be proposal_count + 1.
    pub fn propose(ctx: Context<Propose>, id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let g = &mut ctx.accounts.governance;
        let proposer = ctx.accounts.proposer.key();
        require!(g.is_council(&proposer), GovError::NotCouncilMember);
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
        p.council_term = g.council_term;
        p.voting_starts_at = now + g.config.voting_delay as i64;
        p.voting_ends_at = p.voting_starts_at + g.config.voting_period as i64;
        p.votes = Vec::new();
        p.queued_at = 0;
        p.executed = false;
        p.cancelled = false;
        p.bump = ctx.bumps.proposal;
        emit!(ProposalCreated {
            hub_dao: g.hub_dao,
            proposal: p.key(),
            id,
            proposer,
            metadata_uri: p.metadata_uri.clone(),
            voting_starts_at: p.voting_starts_at,
            voting_ends_at: p.voting_ends_at,
        });
        Ok(())
    }

    /// A council member votes on a proposal while voting is open. One vote
    /// per seat.
    pub fn cast_vote(ctx: Context<CouncilAction>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let member = ctx.accounts.member.key();
        require!(g.is_council(&member), GovError::NotCouncilMember);
        let p = &mut ctx.accounts.proposal;
        require!(!p.cancelled && !p.executed, GovError::ProposalNotActive);
        require!(p.council_term == g.council_term, GovError::CouncilChanged);
        require!(now >= p.voting_starts_at && now < p.voting_ends_at, GovError::ProposalNotActive);
        require!(!p.votes.iter().any(|v| v.member == member), GovError::AlreadyVoted);
        p.votes.push(CouncilVote { member, choice });
        emit!(VoteCast { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, member, choice });
        Ok(())
    }

    /// Queues a proposal that passed among current council members, once
    /// voting has ended. Anyone.
    pub fn queue(ctx: Context<Queue>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.queued_at == 0, GovError::ProposalAlreadyQueued);
        require!(p.council_term == g.council_term, GovError::CouncilChanged);
        require!(now >= p.voting_ends_at, GovError::VotingNotEnded);
        g.check_passed(p)?;
        p.queued_at = now;
        emit!(ProposalQueued { hub_dao: g.hub_dao, proposal: p.key(), id: p.id, executable_at: now + g.config.timelock as i64 });
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
        require!(p.council_term == g.council_term, GovError::CouncilChanged);
        // A recall since queueing may have removed some of its votes.
        g.check_passed(p)?;
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

    // ---- recall ----

    /// Starts a token vote on removing `member` from the council. Needs
    /// `candidacy_threshold` deposited. `id` must be recall_count + 1.
    pub fn initiate_recall(ctx: Context<InitiateRecall>, id: u64, member: Pubkey) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        require!(g.is_council(&member), GovError::NotCouncilMember);
        require!(ctx.accounts.voter.amount >= g.config.candidacy_threshold, GovError::ProposalThresholdNotMet);
        require!(id == g.recall_count + 1, GovError::WrongDao);
        g.recall_count = id;
        let r = &mut ctx.accounts.recall;
        r.governance = g.key();
        r.id = id;
        r.member = member;
        r.initiator = ctx.accounts.initiator.key();
        r.council_term = g.council_term;
        r.ends_at = now + g.config.recall_voting_period as i64;
        r.quorum_base = g.total_deposited;
        r.tally = Tally::default();
        r.finalized = false;
        r.removed = false;
        r.bump = ctx.bumps.recall;
        emit!(RecallStarted { hub_dao: g.hub_dao, recall: r.key(), id, member, initiator: r.initiator, ends_at: r.ends_at });
        Ok(())
    }

    /// Votes on a recall, weighted by deposit. Once per recall; locks the
    /// deposit until it ends.
    pub fn vote_recall(ctx: Context<VoteRecall>, choice: VoteChoice) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.recall;
        require!(now < r.ends_at, GovError::ProposalNotActive);
        let voter = &mut ctx.accounts.voter;
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);
        let total = match choice {
            VoteChoice::For => &mut r.tally.for_votes,
            VoteChoice::Against => &mut r.tally.against_votes,
            VoteChoice::Abstain => &mut r.tally.abstain_votes,
        };
        *total = total.checked_add(weight).ok_or(GovError::Overflow)?;
        voter.locked_until = voter.locked_until.max(r.ends_at);
        let record = &mut ctx.accounts.recall_vote;
        record.recall = r.key();
        record.voter = voter.owner;
        record.choice = choice;
        record.weight = weight;
        record.bump = ctx.bumps.recall_vote;
        emit!(RecallVoteCast { hub_dao: ctx.accounts.governance.hub_dao, recall: r.key(), id: r.id, voter: voter.owner, choice, weight });
        Ok(())
    }

    /// Closes a recall once voting ends; if it passed and the member still
    /// sits on the same council, they're removed. Anyone.
    pub fn finalize_recall(ctx: Context<FinalizeRecall>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let r = &mut ctx.accounts.recall;
        require!(now >= r.ends_at, GovError::VotingNotEnded);
        require!(!r.finalized, GovError::AlreadyFinalized);
        r.finalized = true;
        let passed = r.quorum_base > 0 && vortex_core::passed(&r.tally, r.quorum_base, &g.config.recall_voting());
        if passed && r.council_term == g.council_term {
            if let Some(at) = g.council.iter().position(|m| *m == r.member) {
                g.council.remove(at);
                r.removed = true;
            }
        }
        emit!(RecallFinalized { hub_dao: g.hub_dao, recall: r.key(), id: r.id, member: r.member, removed: r.removed });
        Ok(())
    }

    /// Replaces the rules (only through a passed proposal). A new council
    /// size applies from the next election.
    pub fn update_config(ctx: Context<UpdateConfig>, config: DelegateConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

/// A DAO's rules. Times in seconds; amounts in raw tokens.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct DelegateConfig {
    /// Seats on the council.
    pub council_size: u8,
    /// How long a council serves before an election can start.
    pub term_length: u32,
    /// Deposit needed to stand for election or start a recall.
    pub candidacy_threshold: u64,
    /// How long candidates can declare after an election starts.
    pub candidacy_period: u32,
    /// How long election voting lasts, after candidacy closes.
    pub election_voting_period: u32,
    /// Council votes (for + against + abstain) a proposal needs.
    pub council_quorum: u8,
    /// Share of for / (for + against) among council votes needed to pass.
    pub council_approval_bps: u16,
    pub voting_delay: u32,
    pub voting_period: u32,
    pub timelock: u32,
    pub execution_period: u32,
    /// Share of all deposits (when the recall started) that must vote.
    pub recall_quorum_bps: u16,
    pub recall_approval_bps: u16,
    pub recall_voting_period: u32,
}

impl DelegateConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.council_size >= 1 && self.council_size as usize <= MAX_COUNCIL, GovError::InvalidConfig);
        require!(self.term_length > 0 && self.candidacy_period > 0 && self.election_voting_period > 0, GovError::InvalidConfig);
        require!(self.council_quorum >= 1 && self.council_quorum <= self.council_size, GovError::InvalidQuorum);
        require!(self.council_approval_bps >= 1 && self.council_approval_bps <= MAX_BPS, GovError::InvalidApprovalThreshold);
        require!(self.voting_period > 0, GovError::InvalidVotingPeriod);
        self.recall_voting().validate()
    }

    /// Council proposals' timing, in vortex-core's shape.
    pub fn timing(&self) -> VotingConfig {
        VotingConfig {
            quorum_bps: 1,
            approval_bps: self.council_approval_bps,
            voting_delay: self.voting_delay,
            voting_period: self.voting_period,
            timelock: self.timelock,
            execution_period: self.execution_period,
            proposal_threshold: 0,
        }
    }

    /// The recall vote's rules, in vortex-core's shape.
    pub fn recall_voting(&self) -> VotingConfig {
        VotingConfig {
            quorum_bps: self.recall_quorum_bps,
            approval_bps: self.recall_approval_bps,
            voting_delay: 0,
            voting_period: self.recall_voting_period,
            timelock: 0,
            execution_period: 0,
            proposal_threshold: self.candidacy_threshold,
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
    pub config: DelegateConfig,
    #[max_len(MAX_COUNCIL)]
    pub council: Vec<Pubkey>,
    /// Goes up each time an election seats a council.
    pub council_term: u32,
    pub term_ends_at: i64,
    pub election_count: u64,
    pub election_open: bool,
    pub proposal_count: u64,
    pub recall_count: u64,
    pub total_deposited: u64,
    pub bump: u8,
}

impl Governance {
    pub fn is_council(&self, key: &Pubkey) -> bool {
        self.council.contains(key)
    }

    /// Votes on `p` from people on the council now.
    pub fn current_tally(&self, p: &Proposal) -> Tally {
        let mut t = Tally::default();
        for v in p.votes.iter().filter(|v| self.is_council(&v.member)) {
            match v.choice {
                VoteChoice::For => t.for_votes += 1,
                VoteChoice::Against => t.against_votes += 1,
                VoteChoice::Abstain => t.abstain_votes += 1,
            }
        }
        t
    }

    pub fn council_passed(&self, p: &Proposal) -> bool {
        let t = self.current_tally(p);
        t.participation() >= self.config.council_quorum as u128 && vortex_core::has_approval(&t, &self.config.timing())
    }

    pub fn check_passed(&self, p: &Proposal) -> Result<()> {
        let t = self.current_tally(p);
        require!(t.participation() >= self.config.council_quorum as u128, GovError::QuorumNotReached);
        require!(vortex_core::has_approval(&t, &self.config.timing()), GovError::ApprovalThresholdNotMet);
        Ok(())
    }
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

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Candidate {
    pub key: Pubkey,
    pub votes: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Election {
    pub governance: Pubkey,
    pub id: u64,
    pub candidacy_ends_at: i64,
    pub voting_ends_at: i64,
    #[max_len(MAX_CANDIDATES)]
    pub candidates: Vec<Candidate>,
    pub finalized: bool,
    pub bump: u8,
}

/// One per member per election, so nobody votes twice.
#[account]
#[derive(InitSpace)]
pub struct Ballot {
    pub election: Pubkey,
    pub voter: Pubkey,
    pub weight: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Recall {
    pub governance: Pubkey,
    pub id: u64,
    pub member: Pubkey,
    pub initiator: Pubkey,
    /// Only removes the member from this council, not a later one.
    pub council_term: u32,
    pub ends_at: i64,
    /// All deposits when it started: what quorum is measured against.
    pub quorum_base: u64,
    pub tally: Tally,
    pub finalized: bool,
    pub removed: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct RecallVote {
    pub recall: Pubkey,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct CouncilVote {
    pub member: Pubkey,
    pub choice: VoteChoice,
}

#[account]
pub struct Proposal {
    /// Standard prefix the hub reads.
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    /// The council that made it; it dies when a new one is elected.
    pub council_term: u32,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
    /// One per council member who voted (at most MAX_COUNCIL).
    pub votes: Vec<CouncilVote>,
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
            + 4 + 8 + 8
            + (4 + CouncilVote::INIT_SPACE * MAX_COUNCIL)
            + 8 + 1 + 1 + 1
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

    /// State at `now` (what clients show). A proposal from an earlier
    /// council that never ran shows as Expired.
    pub fn state(&self, g: &Governance, now: i64) -> ProposalState {
        if !self.executed && !self.cancelled && self.council_term != g.council_term {
            return ProposalState::Expired;
        }
        proposal_state(&self.lifecycle(), &g.config.timing(), g.council_passed(self), now)
    }
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
#[instruction(id: u64)]
pub struct StartElection<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(
        init, payer = payer, space = 8 + Election::INIT_SPACE,
        seeds = [ELECTION_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub election: Account<'info, Election>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DeclareCandidacy<'info> {
    pub candidate: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub election: Account<'info, Election>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), candidate.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
}

#[derive(Accounts)]
pub struct VoteInElection<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub election: Account<'info, Election>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = owner, space = 8 + Ballot::INIT_SPACE,
        seeds = [BALLOT_SEED, election.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub ballot: Account<'info, Ballot>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeElection<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub election: Account<'info, Election>,
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
pub struct CouncilAction<'info> {
    pub member: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
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
#[instruction(id: u64)]
pub struct InitiateRecall<'info> {
    #[account(mut)]
    pub initiator: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), initiator.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = initiator, space = 8 + Recall::INIT_SPACE,
        seeds = [RECALL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub recall: Account<'info, Recall>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct VoteRecall<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub recall: Account<'info, Recall>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(
        init, payer = owner, space = 8 + RecallVote::INIT_SPACE,
        seeds = [RECALL_VOTE_SEED, recall.key().as_ref(), owner.key().as_ref()], bump,
    )]
    pub recall_vote: Account<'info, RecallVote>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeRecall<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub recall: Account<'info, Recall>,
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
    pub council: Vec<Pubkey>,
    pub term_ends_at: i64,
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
pub struct ElectionStarted {
    pub hub_dao: Pubkey,
    pub election: Pubkey,
    pub id: u64,
    pub candidacy_ends_at: i64,
    pub voting_ends_at: i64,
}

#[event]
pub struct CandidacyDeclared {
    pub hub_dao: Pubkey,
    pub election: Pubkey,
    pub id: u64,
    pub candidate: Pubkey,
}

#[event]
pub struct ElectionVoteCast {
    pub hub_dao: Pubkey,
    pub election: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub candidates: Vec<Pubkey>,
    pub weight: u64,
}

#[event]
pub struct ElectionFinalized {
    pub hub_dao: Pubkey,
    pub election: Pubkey,
    pub id: u64,
    pub council: Vec<Pubkey>,
    pub term_ends_at: i64,
    /// False if nobody got a vote and the old council stayed.
    pub seated: bool,
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
    pub member: Pubkey,
    pub choice: VoteChoice,
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
pub struct RecallStarted {
    pub hub_dao: Pubkey,
    pub recall: Pubkey,
    pub id: u64,
    pub member: Pubkey,
    pub initiator: Pubkey,
    pub ends_at: i64,
}

#[event]
pub struct RecallVoteCast {
    pub hub_dao: Pubkey,
    pub recall: Pubkey,
    pub id: u64,
    pub voter: Pubkey,
    pub choice: VoteChoice,
    pub weight: u64,
}

#[event]
pub struct RecallFinalized {
    pub hub_dao: Pubkey,
    pub recall: Pubkey,
    pub id: u64,
    pub member: Pubkey,
    pub removed: bool,
}

#[event]
pub struct ConfigUpdated {
    pub hub_dao: Pubkey,
    pub config: DelegateConfig,
}
