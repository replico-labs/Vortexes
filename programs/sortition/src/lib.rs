//! Vortexes sortition governance, the Solana counterpart of the EVM
//! `SortitionGovernance.sol`: the council is drawn at random from members
//! who opted in, not elected or appointed. Nobody can buy a seat or
//! campaign for one. Once seated, the council governs one seat, one vote,
//! like vortex-delegate's.
//!
//! - Members deposit the DAO's token. With at least `eligibility_threshold`
//!   deposited they can join the **draw pool**; their deposit stays locked
//!   while they're in it.
//! - **Draws.** Once the council's term is over, anyone starts a draw. The
//!   randomness comes from ORAO VRF (`VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`,
//!   the same on devnet and mainnet), requested in the same transaction:
//!   the caller's transaction first asks ORAO for randomness with the seed
//!   `sha256("vortex-sortition", governance, draw number, nonce)`, then calls
//!   `start_draw`, which checks that request is ORAO's, at that seed's
//!   address, and still unanswered. The pool is frozen from then on, so
//!   nobody can join or leave knowing the result. Once ORAO answers (within
//!   seconds), anyone calls `finalize_draw`: a partial Fisher-Yates shuffle
//!   seeded by the 64 random bytes seats `council_size` members (all of the
//!   pool if it's smaller) for `term_length`.
//! - If ORAO hasn't answered after `randomness_timeout`, anyone can start
//!   the draw again with a new request - only while the old one is still
//!   unanswered, so a result can't be seen first and then thrown away.
//! - **Council proposals:** council members propose and vote; a proposal
//!   needs `council_quorum` votes (capped at the council's size, in case the
//!   pool was smaller than the seats) and `council_approval_bps` of
//!   for / (for + against). It dies when a new council is drawn.
//! - No recall, as in the EVM version: short terms are the check.
//!
//! The program never calls ORAO itself, so it doesn't depend on ORAO's
//! Anchor version; it only reads ORAO's request account.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execution_expired, proposal_state, timelock_complete, validate_instructions, validate_uri, GovError, Lifecycle,
    ProposalCore, ProposalState, StoredInstruction, Tally, VotingConfig, GOVERNANCE_SEED, MAX_BPS,
};
use vortex_hub::{Dao as HubDao, DaoToken};

declare_id!("FjuZX6dakUUY2GxqnHiqkpDUADBJyA3GkmoT5gqvp6U5");

pub const VAULT_SEED: &[u8] = b"vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const POOL_SEED: &[u8] = b"pool";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
/// Most seats a council can have.
pub const MAX_COUNCIL: usize = 15;
/// Most members the draw pool can hold.
pub const MAX_POOL: usize = 256;

/// ORAO VRF on Solana (orao-solana-vrf 0.7.0: `declare_id!`).
pub const ORAO_VRF: Pubkey = pubkey!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
/// ORAO's seed for a request account: `[RANDOMNESS_ACCOUNT_SEED, seed]`.
pub const ORAO_REQUEST_SEED: &[u8] = b"orao-vrf-randomness-request";
/// Anchor discriminator of ORAO's `RandomnessV2` account: sha256("account:RandomnessV2")[..8].
pub const ORAO_RANDOMNESS_V2: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];

#[program]
pub mod vortex_sortition {
    use super::*;

    /// Sets up a hub DAO in this program: rules, token, vault, an empty
    /// draw pool and the first council (exactly `council_size` members),
    /// whose term starts now. The creator right after
    /// `vortex_hub::create_dao`, or the DAO's treasury when switching here.
    pub fn init_governance(ctx: Context<InitGovernance>, council: Vec<Pubkey>, config: SortitionConfig) -> Result<()> {
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
        g.proposal_count = 0;
        g.total_deposited = 0;
        g.draw_count = 0;
        g.draw_open = false;
        g.draw_seed = [0; 32];
        g.draw_started_at = 0;
        g.bump = ctx.bumps.governance;
        ctx.accounts.pool.governance = g.key();
        ctx.accounts.pool.bump = ctx.bumps.pool;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint, council: g.council.clone(), term_ends_at: g.term_ends_at });
        Ok(())
    }

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

    /// Withdraws a deposit; members in the draw pool leave it first.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let voter = &mut ctx.accounts.voter;
        require!(!voter.eligible, GovError::LeavePoolFirst);
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

    /// Joins the draw pool. Needs `eligibility_threshold` deposited; not
    /// while a draw is under way.
    pub fn join_pool(ctx: Context<PoolAction>) -> Result<()> {
        let g = &ctx.accounts.governance;
        require!(!g.draw_open, GovError::DrawInProgress);
        let voter = &mut ctx.accounts.voter;
        require!(!voter.eligible, GovError::AlreadyEligible);
        require!(voter.amount >= g.config.eligibility_threshold, GovError::ProposalThresholdNotMet);
        let pool = &mut ctx.accounts.pool;
        require!(pool.members.len() < MAX_POOL, GovError::PoolFull);
        pool.members.push(voter.owner);
        voter.eligible = true;
        emit!(PoolJoined { governance: g.key(), member: voter.owner, pool_size: pool.members.len() as u16 });
        Ok(())
    }

    /// Leaves the draw pool (a council seat already held is kept). Not
    /// while a draw is under way.
    pub fn leave_pool(ctx: Context<PoolAction>) -> Result<()> {
        let g = &ctx.accounts.governance;
        require!(!g.draw_open, GovError::DrawInProgress);
        let voter = &mut ctx.accounts.voter;
        require!(voter.eligible, GovError::NotEligible);
        let pool = &mut ctx.accounts.pool;
        let i = pool.members.iter().position(|m| *m == voter.owner).ok_or(GovError::NotEligible)?;
        pool.members.swap_remove(i);
        voter.eligible = false;
        emit!(PoolLeft { governance: g.key(), member: voter.owner, pool_size: pool.members.len() as u16 });
        Ok(())
    }

    /// Starts a draw (anyone, once the term is over), or starts it again
    /// when ORAO hasn't answered within `randomness_timeout`. The same
    /// transaction must first request ORAO randomness with
    /// `draw_seed(governance, draw number, nonce)`; this checks that request
    /// is still unanswered, then freezes the pool.
    pub fn start_draw(ctx: Context<StartDraw>, nonce: [u8; 32]) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        require!(now >= g.term_ends_at, GovError::TermNotOver);
        require!(!ctx.accounts.pool.members.is_empty(), GovError::PoolEmpty);
        let restart = g.draw_open;
        if restart {
            require!(now >= g.draw_started_at + g.config.randomness_timeout as i64, GovError::DrawInProgress);
            // The old request must still be unanswered, so nobody saw its result before replacing it.
            require!(read_randomness(&ctx.accounts.previous_randomness, &g.draw_seed)?.is_none(), GovError::RandomnessAlreadyFulfilled);
        }
        let number = if restart { g.draw_count } else { g.draw_count + 1 };
        let seed = draw_seed(&g.key(), number, &nonce);
        require!(read_randomness(&ctx.accounts.randomness, &seed)?.is_none(), GovError::RandomnessAlreadyFulfilled);
        g.draw_count = number;
        g.draw_open = true;
        g.draw_seed = seed;
        g.draw_started_at = now;
        emit!(DrawStarted { hub_dao: g.hub_dao, number, seed, randomness: ctx.accounts.randomness.key(), pool_size: ctx.accounts.pool.members.len() as u16, restart });
        Ok(())
    }

    /// Seats the new council from ORAO's answer. Anyone.
    pub fn finalize_draw(ctx: Context<FinalizeDraw>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        require!(g.draw_open, GovError::NoDrawInProgress);
        let randomness = read_randomness(&ctx.accounts.randomness, &g.draw_seed)?.ok_or(GovError::RandomnessNotReady)?;
        let pool = &mut ctx.accounts.pool;
        let n = pool.members.len();
        let seats = (g.config.council_size as usize).min(n);
        // Partial Fisher-Yates: position i takes a uniformly chosen member from i..n.
        for i in 0..seats {
            let h = solana_sha256_hasher::hashv(&[&randomness, &(i as u64).to_le_bytes()]).to_bytes();
            let r = u64::from_le_bytes(h[..8].try_into().unwrap());
            let j = i + (r % (n - i) as u64) as usize;
            pool.members.swap(i, j);
        }
        g.council = pool.members[..seats].to_vec();
        g.council_term += 1;
        g.term_ends_at = now + g.config.term_length as i64;
        g.draw_open = false;
        emit!(DrawFinalized { hub_dao: g.hub_dao, number: g.draw_count, council: g.council.clone(), term_ends_at: g.term_ends_at });
        Ok(())
    }

    /// A council member proposes `instructions`. `id` must be proposal_count + 1.
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

    /// A council member votes while voting is open; one vote per seat.
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

    /// Queues a proposal the council passed, once voting has ended. Anyone.
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

    /// The hub's check before it runs a proposal (vortex-core's CONFIRM_EXECUTION).
    pub fn confirm_execution(ctx: Context<ConfirmExecution>, epoch: u32) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &ctx.accounts.governance;
        let timing = g.config.timing();
        let p = &mut ctx.accounts.proposal;
        require!(p.core.epoch == epoch, GovError::StaleProposal);
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.council_term == g.council_term, GovError::CouncilChanged);
        let life = p.lifecycle();
        require!(timelock_complete(&life, &timing, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &timing, now), GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: g.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed: its proposer, or the DAO through a proposal.
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

    /// Replaces the rules (only through a passed proposal). Council size
    /// changes take effect at the next draw.
    pub fn update_config(ctx: Context<UpdateConfig>, config: SortitionConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }
}

// ---------------------------------------------------------------
// RANDOMNESS
// ---------------------------------------------------------------

/// The ORAO seed for draw `number` of `governance`.
pub fn draw_seed(governance: &Pubkey, number: u64, nonce: &[u8; 32]) -> [u8; 32] {
    solana_sha256_hasher::hashv(&[b"vortex-sortition", governance.as_ref(), &number.to_le_bytes(), nonce]).to_bytes()
}

/// ORAO's request account for `seed`: None while unanswered, the 64
/// random bytes once fulfilled. Refuses anything that isn't ORAO's
/// account at that seed's address.
fn read_randomness(account: &UncheckedAccount, seed: &[u8; 32]) -> Result<Option<[u8; 64]>> {
    let expected = Pubkey::find_program_address(&[ORAO_REQUEST_SEED, seed], &ORAO_VRF).0;
    require_keys_eq!(account.key(), expected, GovError::WrongRandomnessAccount);
    require_keys_eq!(*account.owner, ORAO_VRF, GovError::WrongRandomnessAccount);
    let data = account.try_borrow_data()?;
    // RandomnessV2 { request: RequestAccount::Pending(..) | Fulfilled { client, seed, randomness } }
    require!(data.len() >= 9 && data[..8] == ORAO_RANDOMNESS_V2, GovError::WrongRandomnessAccount);
    match data[8] {
        0 => Ok(None),
        1 => {
            require!(data.len() >= 9 + 32 + 32 + 64, GovError::WrongRandomnessAccount);
            require!(data[9 + 32..9 + 64] == seed[..], GovError::WrongRandomnessAccount);
            let mut r = [0u8; 64];
            r.copy_from_slice(&data[9 + 64..9 + 128]);
            Ok(Some(r))
        }
        _ => err!(GovError::WrongRandomnessAccount),
    }
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct SortitionConfig {
    /// Seats on the council.
    pub council_size: u8,
    /// How long a council serves before a draw can start.
    pub term_length: u32,
    /// Deposit needed to join the draw pool.
    pub eligibility_threshold: u64,
    /// Council votes (for + against + abstain) a proposal needs, at most the council's size.
    pub council_quorum: u8,
    /// Share of for / (for + against) among council votes needed to pass.
    pub council_approval_bps: u16,
    pub voting_delay: u32,
    pub voting_period: u32,
    pub timelock: u32,
    pub execution_period: u32,
    /// How long to wait for ORAO before a draw can be started again.
    pub randomness_timeout: u32,
}

impl SortitionConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.council_size >= 1 && self.council_size as usize <= MAX_COUNCIL, GovError::InvalidConfig);
        require!(self.term_length > 0 && self.randomness_timeout > 0, GovError::InvalidConfig);
        require!(self.council_quorum >= 1 && self.council_quorum <= self.council_size, GovError::InvalidQuorum);
        require!(self.council_approval_bps >= 1 && self.council_approval_bps <= MAX_BPS, GovError::InvalidApprovalThreshold);
        require!(self.voting_period > 0, GovError::InvalidVotingPeriod);
        Ok(())
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
}

#[account]
#[derive(InitSpace)]
pub struct Governance {
    pub hub_dao: Pubkey,
    pub treasury: Pubkey,
    pub executor: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub config: SortitionConfig,
    #[max_len(MAX_COUNCIL)]
    pub council: Vec<Pubkey>,
    /// Goes up with every draw; proposals die with their council.
    pub council_term: u32,
    pub term_ends_at: i64,
    pub proposal_count: u64,
    pub total_deposited: u64,
    /// Draws started so far (the current one's number while one is open).
    pub draw_count: u64,
    pub draw_open: bool,
    /// The ORAO seed of the draw under way.
    pub draw_seed: [u8; 32],
    pub draw_started_at: i64,
    pub bump: u8,
}

impl Governance {
    pub fn is_council(&self, key: &Pubkey) -> bool {
        self.council.contains(key)
    }

    pub fn tally(p: &Proposal) -> Tally {
        let mut t = Tally::default();
        for v in &p.votes {
            match v.choice {
                VoteChoice::For => t.for_votes += 1,
                VoteChoice::Against => t.against_votes += 1,
                VoteChoice::Abstain => t.abstain_votes += 1,
            }
        }
        t
    }

    /// The quorum in force: the configured one, or the whole council if it's smaller.
    pub fn quorum(&self) -> u128 {
        (self.config.council_quorum as usize).min(self.council.len()).max(1) as u128
    }

    pub fn council_passed(&self, p: &Proposal) -> bool {
        let t = Self::tally(p);
        t.participation() >= self.quorum() && vortex_core::has_approval(&t, &self.config.timing())
    }

    pub fn check_passed(&self, p: &Proposal) -> Result<()> {
        let t = Self::tally(p);
        require!(t.participation() >= self.quorum(), GovError::QuorumNotReached);
        require!(vortex_core::has_approval(&t, &self.config.timing()), GovError::ApprovalThresholdNotMet);
        Ok(())
    }
}

/// Everyone who can be drawn.
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub governance: Pubkey,
    #[max_len(MAX_POOL)]
    pub members: Vec<Pubkey>,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub governance: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// In the draw pool (deposit locked meanwhile).
    pub eligible: bool,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct CouncilVote {
    pub member: Pubkey,
    pub choice: VoteChoice,
}

#[account]
pub struct Proposal {
    pub core: ProposalCore,
    pub governance: Pubkey,
    pub id: u64,
    pub proposer: Pubkey,
    pub metadata_uri: String,
    /// The council that made it; it dies when a new one is drawn.
    pub council_term: u32,
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
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

    /// State at `now`. A proposal from an earlier council that never ran shows as Expired.
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
    #[account(init, payer = payer, space = 8 + Pool::INIT_SPACE, seeds = [POOL_SEED, governance.key().as_ref()], bump)]
    pub pool: Account<'info, Pool>,
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
pub struct PoolAction<'info> {
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [POOL_SEED, governance.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
}

#[derive(Accounts)]
pub struct StartDraw<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(seeds = [POOL_SEED, governance.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
    /// ORAO's request account for the new seed.
    /// CHECK: address, owner and contents checked by read_randomness.
    pub randomness: UncheckedAccount<'info>,
    /// When restarting: ORAO's request account for the draw's current seed (must still be unanswered).
    /// Pass `randomness` again for a first start.
    /// CHECK: checked by read_randomness when restarting.
    pub previous_randomness: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct FinalizeDraw<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [POOL_SEED, governance.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address, owner and contents checked by read_randomness.
    pub randomness: UncheckedAccount<'info>,
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
pub struct PoolJoined {
    pub governance: Pubkey,
    pub member: Pubkey,
    pub pool_size: u16,
}

#[event]
pub struct PoolLeft {
    pub governance: Pubkey,
    pub member: Pubkey,
    pub pool_size: u16,
}

#[event]
pub struct DrawStarted {
    pub hub_dao: Pubkey,
    pub number: u64,
    pub seed: [u8; 32],
    /// ORAO's request account to watch.
    pub randomness: Pubkey,
    pub pool_size: u16,
    pub restart: bool,
}

#[event]
pub struct DrawFinalized {
    pub hub_dao: Pubkey,
    pub number: u64,
    pub council: Vec<Pubkey>,
    pub term_ends_at: i64,
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
pub struct ConfigUpdated {
    pub hub_dao: Pubkey,
    pub config: SortitionConfig,
}
