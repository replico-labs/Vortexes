//! Shared rules for the Vortexes governance programs.
//!
//! Vortexes splits a DAO in two:
//! - **vortex-hub** holds the DAO's record and its treasury, runs passed
//!   proposals, and records which governance program is in charge. One
//!   hub serves every DAO.
//! - **Governance programs** (token-weighted, quadratic, optimistic,
//!   board, ...) only decide: proposals, votes, signers. Any approved one
//!   can run a DAO, and a DAO can vote to switch to another without its
//!   treasury moving.
//!
//! What they agree on lives here: the PDA seeds, the [`ProposalCore`] every
//! proposal account starts with, the `confirm_execution` interface, plus
//! the shared voting rules ([`VotingConfig`], [`proposal_state`],
//! [`passed`]) and [`execute_instructions`].

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

pub const MAX_BPS: u16 = 10_000;
/// Instructions one proposal may carry.
pub const MAX_INSTRUCTIONS: usize = 8;
/// Longest metadata URI / description a proposal stores.
pub const MAX_URI_LEN: usize = 200;
/// Longest DAO name.
pub const MAX_NAME_LEN: usize = 32;

/// Hub PDA `[TREASURY_SEED, hub_dao]`: holds the DAO's SOL (a
/// system-owned address with no data) and owns its token accounts. Only
/// the hub signs for it, and only while running a passed proposal.
pub const TREASURY_SEED: &[u8] = b"treasury";
/// Hub PDA `[EXECUTOR_SEED, hub_dao]`: signs the hub's
/// `confirm_execution` call, so a governance program knows the hub, and
/// nobody else, is asking.
pub const EXECUTOR_SEED: &[u8] = b"executor";
/// Governance-program PDA `[GOVERNANCE_SEED, hub_dao]`: a DAO's state in
/// that governance program (rules, counts, vault...).
pub const GOVERNANCE_SEED: &[u8] = b"governance";

/// The instruction every governance program implements for the hub.
/// Accounts, in order: the hub's executor PDA (signer), the governance
/// account, the proposal (writable). Argument: the hub's current epoch
/// (u32). It must check the proposal can run now, mark it executed, and
/// `set_return_data` the proposal's address.
pub const CONFIRM_EXECUTION: &str = "confirm_execution";

/// Anchor's 8-byte instruction discriminator for `confirm_execution`:
/// the first 8 bytes of sha256("global:confirm_execution").
pub const CONFIRM_EXECUTION_DISCRIMINATOR: [u8; 8] = {
    let h = const_crypto::sha2::Sha256::new().update(b"global:confirm_execution").finalize();
    [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]
};

/// The first field of every governance program's proposal account, right
/// after Anchor's 8-byte discriminator, so the hub can read what to run
/// from any model.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProposalCore {
    /// The hub DAO this proposal belongs to.
    pub hub_dao: Pubkey,
    /// The hub epoch it was made in; only runs while that epoch lasts.
    pub epoch: u32,
    /// What it does when it executes, signed by the treasury.
    pub instructions: Vec<StoredInstruction>,
}

impl ProposalCore {
    pub fn serialized_len(&self) -> usize {
        32 + 4 + instructions_len(&self.instructions)
    }

    /// Reads the core from a proposal account's raw data.
    pub fn read(account_data: &[u8]) -> Result<Self> {
        require!(account_data.len() > 8, GovError::InvalidProposalAccount);
        let mut rest = &account_data[8..];
        ProposalCore::deserialize(&mut rest).map_err(|_| error!(GovError::InvalidProposalAccount))
    }
}

#[error_code]
pub enum GovError {
    #[msg("Quorum must be between 1 and 10000 basis points")]
    InvalidQuorum,
    #[msg("Approval threshold must be between 1 and 10000 basis points")]
    InvalidApprovalThreshold,
    #[msg("Voting period must be longer than zero")]
    InvalidVotingPeriod,
    #[msg("Name is empty or too long")]
    InvalidName,
    #[msg("Description is empty or too long")]
    InvalidMetadataUri,
    #[msg("A proposal needs at least one instruction")]
    EmptyProposalActions,
    #[msg("Too many instructions in one proposal")]
    TooManyInstructions,
    #[msg("Only the DAO treasury can sign a proposal's instructions")]
    InvalidProposalSigner,
    #[msg("An account the proposal needs wasn't passed in")]
    MissingProposalAccount,
    #[msg("Not enough voting power to propose")]
    ProposalThresholdNotMet,
    #[msg("Voting isn't open for this proposal")]
    ProposalNotActive,
    #[msg("Voting hasn't ended yet")]
    VotingNotEnded,
    #[msg("No voting power: deposit tokens first")]
    NoVotingPower,
    #[msg("Quorum not reached")]
    QuorumNotReached,
    #[msg("Approval threshold not met")]
    ApprovalThresholdNotMet,
    #[msg("Proposal already queued")]
    ProposalAlreadyQueued,
    #[msg("Proposal isn't queued, or its timelock hasn't passed")]
    ProposalNotExecutable,
    #[msg("Proposal's execution window has passed")]
    ProposalExpired,
    #[msg("Proposal already executed")]
    ProposalAlreadyExecuted,
    #[msg("Proposal already cancelled")]
    ProposalAlreadyCancelled,
    #[msg("Only the proposer or the DAO itself can do this")]
    Unauthorized,
    #[msg("Tokens are locked until the proposals you voted on finish")]
    TokensLocked,
    #[msg("Amount must be greater than zero")]
    InvalidAmount,
    #[msg("Not enough deposited")]
    InsufficientDeposit,
    #[msg("Wrong account for this DAO")]
    WrongDao,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Not a proposal account this hub can read")]
    InvalidProposalAccount,
    #[msg("This proposal was made under an earlier governance setup and can no longer run")]
    StaleProposal,
    #[msg("This governance program isn't the one running the DAO")]
    NotActiveGovernance,
    #[msg("That governance program isn't approved")]
    ModelNotApproved,
    #[msg("The governance program didn't confirm this proposal")]
    NotConfirmed,
    #[msg("A switch is already pending")]
    SwitchPending,
    #[msg("No switch is pending")]
    NoSwitchPending,
    #[msg("The switch delay hasn't passed yet")]
    SwitchNotReady,
    #[msg("The new governance program hasn't been set up for this DAO")]
    GovernanceNotInitialized,
    #[msg("Already the DAO's governance program")]
    AlreadyActive,
    #[msg("Only the board's signers can do this")]
    NotSigner,
    #[msg("Already a signer")]
    AlreadySigner,
    #[msg("Too many signers")]
    TooManySigners,
    #[msg("Required approvals must be between 1 and the number of signers")]
    InvalidApprovals,
    #[msg("Already confirmed")]
    AlreadyConfirmed,
    #[msg("You haven't confirmed this proposal")]
    NotConfirmedBySigner,
    #[msg("Not enough current signers have confirmed")]
    ThresholdNotMet,
    #[msg("The challenge window has closed")]
    ChallengeWindowClosed,
    #[msg("The challenge window is still open")]
    ChallengeWindowOpen,
    #[msg("Already challenged")]
    AlreadyChallenged,
    #[msg("This proposal wasn't challenged")]
    NotChallenged,
    #[msg("The bond has already been settled")]
    BondAlreadyResolved,
    #[msg("Challenge period must be longer than zero")]
    InvalidChallengePeriod,
}

/// A DAO's voting rules. Times are in seconds.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct VotingConfig {
    /// Share of the voting base that must take part (for + against + abstain).
    pub quorum_bps: u16,
    /// Share of for / (for + against) needed to pass.
    pub approval_bps: u16,
    /// Wait between proposing and voting opening.
    pub voting_delay: u32,
    /// How long voting stays open.
    pub voting_period: u32,
    /// Wait between queueing a passed proposal and executing it.
    pub timelock: u32,
    /// How long after the timelock a queued proposal can still execute.
    pub execution_period: u32,
    /// Deposited tokens (raw units) needed to propose.
    pub proposal_threshold: u64,
}

impl VotingConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.quorum_bps >= 1 && self.quorum_bps <= MAX_BPS, GovError::InvalidQuorum);
        require!(self.approval_bps >= 1 && self.approval_bps <= MAX_BPS, GovError::InvalidApprovalThreshold);
        require!(self.voting_period > 0, GovError::InvalidVotingPeriod);
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    require!(!name.is_empty() && name.len() <= MAX_NAME_LEN, GovError::InvalidName);
    Ok(())
}

pub fn validate_uri(uri: &str) -> Result<()> {
    require!(!uri.is_empty() && uri.len() <= MAX_URI_LEN, GovError::InvalidMetadataUri);
    Ok(())
}

/// Where a proposal is in its life. Same states as the EVM contracts.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalState {
    Pending,
    Active,
    Succeeded,
    Queued,
    Defeated,
    Executed,
    Cancelled,
    Expired,
}

/// Running vote totals, in voting weight.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace)]
pub struct Tally {
    pub for_votes: u64,
    pub against_votes: u64,
    pub abstain_votes: u64,
}

impl Tally {
    pub fn participation(&self) -> u128 {
        self.for_votes as u128 + self.against_votes as u128 + self.abstain_votes as u128
    }
}

/// Weight needed for quorum: `base * quorum_bps / 10000`, rounded down.
pub fn quorum_votes(base: u64, quorum_bps: u16) -> u128 {
    base as u128 * quorum_bps as u128 / MAX_BPS as u128
}

/// for / (for + against) in basis points, rounded down; 0 with no votes.
pub fn approval_bps(tally: &Tally) -> u128 {
    let counted = tally.for_votes as u128 + tally.against_votes as u128;
    if counted == 0 {
        return 0;
    }
    tally.for_votes as u128 * MAX_BPS as u128 / counted
}

pub fn has_quorum(tally: &Tally, base: u64, config: &VotingConfig) -> bool {
    tally.participation() >= quorum_votes(base, config.quorum_bps)
}

pub fn has_approval(tally: &Tally, config: &VotingConfig) -> bool {
    approval_bps(tally) >= config.approval_bps as u128
}

/// Quorum and approval both met.
pub fn passed(tally: &Tally, base: u64, config: &VotingConfig) -> bool {
    has_quorum(tally, base, config) && has_approval(tally, config)
}

/// The timing and flags a proposal's state depends on.
#[derive(Clone, Copy, Debug)]
pub struct Lifecycle {
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
    /// 0 until queued.
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
}

pub fn timelock_complete(l: &Lifecycle, config: &VotingConfig, now: i64) -> bool {
    l.queued_at != 0 && now >= l.queued_at + config.timelock as i64
}

pub fn execution_expired(l: &Lifecycle, config: &VotingConfig, now: i64) -> bool {
    l.queued_at != 0 && now > l.queued_at + config.timelock as i64 + config.execution_period as i64
}

/// The proposal's state at `now`, given whether its votes pass.
pub fn proposal_state(l: &Lifecycle, config: &VotingConfig, did_pass: bool, now: i64) -> ProposalState {
    if l.cancelled {
        return ProposalState::Cancelled;
    }
    if l.executed {
        return ProposalState::Executed;
    }
    if now < l.voting_starts_at {
        return ProposalState::Pending;
    }
    if now < l.voting_ends_at {
        return ProposalState::Active;
    }
    if !did_pass {
        return ProposalState::Defeated;
    }
    if l.queued_at == 0 {
        return ProposalState::Succeeded;
    }
    if execution_expired(l, config, now) {
        return ProposalState::Expired;
    }
    ProposalState::Queued
}

/// One account an instruction touches.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct StoredAccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

/// One instruction a proposal runs when it executes.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct StoredInstruction {
    pub program_id: Pubkey,
    pub accounts: Vec<StoredAccountMeta>,
    pub data: Vec<u8>,
}

impl StoredInstruction {
    /// Borsh-serialized size.
    pub fn serialized_len(&self) -> usize {
        32 + 4 + self.accounts.len() * (32 + 1 + 1) + 4 + self.data.len()
    }
}

/// Serialized size of a `Vec<StoredInstruction>`.
pub fn instructions_len(ixs: &[StoredInstruction]) -> usize {
    4 + ixs.iter().map(StoredInstruction::serialized_len).sum::<usize>()
}

/// A proposal's instructions are runnable: at least one, not too many,
/// and the treasury is the only signer they ask for (nobody else can sign
/// at execution time).
pub fn validate_instructions(ixs: &[StoredInstruction], treasury: &Pubkey) -> Result<()> {
    require!(!ixs.is_empty(), GovError::EmptyProposalActions);
    require!(ixs.len() <= MAX_INSTRUCTIONS, GovError::TooManyInstructions);
    for ix in ixs {
        for meta in &ix.accounts {
            if meta.is_signer {
                require_keys_eq!(meta.pubkey, *treasury, GovError::InvalidProposalSigner);
            }
        }
    }
    Ok(())
}

/// Runs a proposal's instructions in order, each signed by the treasury
/// PDA (`treasury_seeds`, including the bump). Every account the
/// instructions reference, and each program they call, must be among
/// `accounts` (the transaction's remaining accounts). Any failure fails
/// the whole execution.
pub fn execute_instructions<'info>(
    ixs: &[StoredInstruction],
    accounts: &[AccountInfo<'info>],
    treasury_seeds: &[&[u8]],
) -> Result<()> {
    let find = |key: &Pubkey| accounts.iter().find(|a| a.key == key).cloned();
    for ix in ixs {
        let mut infos = Vec::with_capacity(ix.accounts.len() + 1);
        for meta in &ix.accounts {
            infos.push(find(&meta.pubkey).ok_or(GovError::MissingProposalAccount)?);
        }
        infos.push(find(&ix.program_id).ok_or(GovError::MissingProposalAccount)?);
        let instruction = Instruction {
            program_id: ix.program_id,
            accounts: ix
                .accounts
                .iter()
                .map(|m| AccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable })
                .collect(),
            data: ix.data.clone(),
        };
        invoke_signed(&instruction, &infos, &[treasury_seeds])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VotingConfig {
        VotingConfig { quorum_bps: 1000, approval_bps: 6000, voting_delay: 10, voting_period: 100, timelock: 50, execution_period: 200, proposal_threshold: 0 }
    }

    #[test]
    fn config_limits() {
        assert!(cfg().validate().is_ok());
        assert!(VotingConfig { quorum_bps: 0, ..cfg() }.validate().is_err());
        assert!(VotingConfig { quorum_bps: 10_001, ..cfg() }.validate().is_err());
        assert!(VotingConfig { approval_bps: 0, ..cfg() }.validate().is_err());
        assert!(VotingConfig { voting_period: 0, ..cfg() }.validate().is_err());
    }

    #[test]
    fn pass_rule_matches_evm() {
        // 10% of 1000 = 100 needed; abstain counts toward quorum only.
        let t = Tally { for_votes: 60, against_votes: 40, abstain_votes: 0 };
        assert!(passed(&t, 1000, &cfg()));
        let t = Tally { for_votes: 59, against_votes: 41, abstain_votes: 0 };
        assert!(!passed(&t, 1000, &cfg()), "59% < 60%");
        let t = Tally { for_votes: 30, against_votes: 0, abstain_votes: 69 };
        assert!(!passed(&t, 1000, &cfg()), "99 < quorum of 100");
        let t = Tally { for_votes: 30, against_votes: 0, abstain_votes: 70 };
        assert!(passed(&t, 1000, &cfg()));
        assert_eq!(approval_bps(&Tally::default()), 0);
        assert!(passed(&Tally { for_votes: u64::MAX, against_votes: u64::MAX, abstain_votes: u64::MAX }, u64::MAX, &VotingConfig { approval_bps: 5000, ..cfg() }));
    }

    #[test]
    fn states() {
        let c = cfg();
        let l = Lifecycle { voting_starts_at: 10, voting_ends_at: 110, queued_at: 0, executed: false, cancelled: false };
        assert_eq!(proposal_state(&l, &c, true, 5), ProposalState::Pending);
        assert_eq!(proposal_state(&l, &c, true, 10), ProposalState::Active);
        assert_eq!(proposal_state(&l, &c, false, 110), ProposalState::Defeated);
        assert_eq!(proposal_state(&l, &c, true, 110), ProposalState::Succeeded);
        let q = Lifecycle { queued_at: 120, ..l };
        assert_eq!(proposal_state(&q, &c, true, 150), ProposalState::Queued);
        assert!(!timelock_complete(&q, &c, 169) && timelock_complete(&q, &c, 170));
        assert!(!execution_expired(&q, &c, 370) && execution_expired(&q, &c, 371));
        assert_eq!(proposal_state(&q, &c, true, 371), ProposalState::Expired);
        assert_eq!(proposal_state(&Lifecycle { executed: true, ..q }, &c, true, 371), ProposalState::Executed);
        assert_eq!(proposal_state(&Lifecycle { cancelled: true, ..q }, &c, true, 0), ProposalState::Cancelled);
    }

    #[test]
    fn only_treasury_may_sign() {
        let treasury = Pubkey::new_unique();
        let ix = |signer: Pubkey| StoredInstruction { program_id: Pubkey::new_unique(), accounts: vec![StoredAccountMeta { pubkey: signer, is_signer: true, is_writable: true }], data: vec![] };
        assert!(validate_instructions(&[ix(treasury)], &treasury).is_ok());
        assert!(validate_instructions(&[ix(Pubkey::new_unique())], &treasury).is_err());
        assert!(validate_instructions(&[], &treasury).is_err());
        assert!(validate_instructions(&vec![ix(treasury); MAX_INSTRUCTIONS + 1], &treasury).is_err());
    }

    #[test]
    fn core_reads_from_account_prefix() {
        let core = ProposalCore {
            hub_dao: Pubkey::new_unique(),
            epoch: 3,
            instructions: vec![StoredInstruction { program_id: Pubkey::new_unique(), accounts: vec![], data: vec![9, 9] }],
        };
        let mut data = vec![1u8; 8];
        data.extend(anchor_lang::prelude::borsh::to_vec(&core).unwrap());
        assert_eq!(core.serialized_len(), data.len() - 8);
        // Model-specific fields after the core don't matter.
        data.extend([7u8; 40]);
        assert_eq!(ProposalCore::read(&data).unwrap(), core);
        assert!(ProposalCore::read(&[0u8; 8]).is_err());
        assert_ne!(CONFIRM_EXECUTION_DISCRIMINATOR, [0u8; 8]);
    }

    #[test]
    fn serialized_len_matches_borsh() {
        let ixs = vec![StoredInstruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![StoredAccountMeta { pubkey: Pubkey::new_unique(), is_signer: false, is_writable: true }; 3],
            data: vec![1, 2, 3, 4, 5],
        }];
        assert_eq!(instructions_len(&ixs), anchor_lang::prelude::borsh::to_vec(&ixs).unwrap().len());
    }
}
