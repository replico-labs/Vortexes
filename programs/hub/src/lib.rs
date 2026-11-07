//! Vortexes hub: one program, shared by every DAO.
//!
//! For each DAO the hub keeps:
//! - its **record**: name, creator, which governance program runs it, and
//!   the DAO's account in that program;
//! - its **treasury**: a PDA holding the DAO's SOL and owning its token
//!   accounts, which only the hub signs for.
//!
//! Governance programs only decide. When one of their proposals passes,
//! anyone calls [`vortex_hub::execute`]: the hub asks the DAO's governance
//! program to confirm it (`confirm_execution`, signed by the hub's
//! executor PDA so nobody else can ask), reads the confirmation from the
//! CPI return data, then runs the proposal's instructions signed by the
//! treasury.
//!
//! A DAO changes governance model by passing a proposal that calls
//! [`vortex_hub::propose_switch`]. After [`SWITCH_DELAY`] anyone applies
//! it; until then the current model can call
//! [`vortex_hub::cancel_switch`]. Each switch starts a new epoch, and a
//! proposal only runs in the epoch it was made in. The treasury never
//! moves.
//!
//! A DAO's account in a governance program is `[GOVERNANCE_SEED, dao,
//! epoch]`, so every switch sets up a fresh one: a DAO can switch to any
//! approved program, one it ran before or the one running now, and always
//! starts clean.
//!
//! The hub's admin keeps the list of approved governance programs: a DAO
//! can only be created with, or switched to, an approved one.
//!
//! The hub also records the DAO's token and its current stake pool
//! ([`vortex_hub::set_dao_token`]): members stake in that pool (vortex-stake),
//! and every token-based governance reads voting power from it, so stake
//! survives every switch. Changing the token takes its own instruction in a
//! passed proposal, never a side effect of a switch: the record moves to
//! the new token's pool, the governance stays, and the old pool only takes
//! withdrawals.
//!
//! A DAO can put a **security council** in front of its treasury
//! ([`vortex_hub::set_council`], from a passed proposal). The council
//! belongs to the DAO, not to a model, so it survives every switch. While
//! it's on, a passed proposal doesn't run straight away:
//! [`vortex_hub::submit_to_council`] confirms it with the governance
//! program and records it as waiting; council signers confirm or reject
//! it; once enough confirm, anyone runs it ([`vortex_hub::run_confirmed`]).
//! Turning the council off ([`vortex_hub::clear_council`]) is a proposal
//! like any other, so the council confirms it too. If the council won't,
//! the DAO can replace its signers once their tenure is over: a proposal
//! that only calls `set_council` runs without the council's confirmation
//! after that.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{get_return_data, invoke_signed},
};
use anchor_lang::system_program;
use anchor_lang::Discriminator;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execute_instructions, validate_name, GovError, ProposalCore, CONFIRM_EXECUTION_DISCRIMINATOR, EXECUTOR_SEED,
    GOVERNANCE_SEED, MAX_NAME_LEN, TREASURY_SEED,
};

declare_id!("5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe");

pub const HUB_SEED: &[u8] = b"hub";
pub const DAO_SEED: &[u8] = b"dao";
pub const MODEL_SEED: &[u8] = b"model";
pub const TOKEN_SEED: &[u8] = b"token";
pub const WAITING_SEED: &[u8] = b"waiting";
pub const ALLOWANCE_SEED: &[u8] = b"allowance";
/// One allowance period.
pub const ALLOWANCE_PERIOD: i64 = 30 * 24 * 60 * 60;
/// The longest an allowance runs before a new proposal must renew it.
pub const MAX_ALLOWANCE_DURATION: i64 = 365 * 24 * 60 * 60;
/// The most signers a council can have.
pub const MAX_COUNCIL: usize = 10;

/// How long a governance switch waits before it can be applied: time for
/// members to see it coming, and for the current model to cancel it.
pub const SWITCH_DELAY: i64 = 2 * 24 * 60 * 60;

#[program]
pub mod vortex_hub {
    use super::*;

    /// One-time setup; the caller becomes the admin of the approved list.
    pub fn init_hub(ctx: Context<InitHub>) -> Result<()> {
        let hub = &mut ctx.accounts.hub;
        hub.admin = ctx.accounts.admin.key();
        hub.bump = ctx.bumps.hub;
        Ok(())
    }

    pub fn set_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.hub.admin = new_admin;
        Ok(())
    }

    /// Approves (or disables) a governance program.
    pub fn set_model(ctx: Context<SetModel>, program_id: Pubkey, name: String, enabled: bool) -> Result<()> {
        validate_name(&name)?;
        let model = &mut ctx.accounts.model;
        model.program_id = program_id;
        model.name = name;
        model.enabled = enabled;
        model.bump = ctx.bumps.model;
        emit!(ModelSet { program_id, name: model.name.clone(), enabled });
        Ok(())
    }

    /// Creates a DAO run by `governance_program`. In the same transaction,
    /// the creator then sets up the DAO in that program (its rules,
    /// signers or token).
    pub fn create_dao(ctx: Context<CreateDao>, name: String, governance_program: Pubkey) -> Result<()> {
        validate_name(&name)?;
        require_keys_eq!(ctx.accounts.model.program_id, governance_program, GovError::ModelNotApproved);
        require!(ctx.accounts.model.enabled, GovError::ModelNotApproved);
        let dao_key = ctx.accounts.dao.key();
        let dao = &mut ctx.accounts.dao;
        dao.create_key = ctx.accounts.create_key.key();
        dao.creator = ctx.accounts.creator.key();
        dao.name = name;
        dao.governance_program = governance_program;
        dao.governance = governance_address(&dao_key, &governance_program, 1);
        dao.epoch = 1;
        dao.pending_switch = None;
        dao.council = None;
        dao.bump = ctx.bumps.dao;
        dao.treasury_bump = Pubkey::find_program_address(&[TREASURY_SEED, dao_key.as_ref()], &crate::ID).1;
        dao.executor_bump = Pubkey::find_program_address(&[EXECUTOR_SEED, dao_key.as_ref()], &crate::ID).1;
        emit!(DaoCreated { dao: dao_key, name: dao.name.clone(), creator: dao.creator, governance_program });
        Ok(())
    }

    /// Records the DAO's token and its stake pool (vortex-stake's
    /// `[POOL_SEED, dao, mint]`). The creator records it once, after
    /// create_dao and before setting up the first governance; after that
    /// only a passed proposal (the treasury) can change it, for a deliberate
    /// move to another token, with or without a switch.
    pub fn set_dao_token(ctx: Context<SetDaoToken>, mint: Pubkey) -> Result<()> {
        let dao = &ctx.accounts.dao;
        let dao_key = dao.key();
        let who = ctx.accounts.authority.key();
        let record = &mut ctx.accounts.dao_token;
        let unset = record.dao == Pubkey::default();
        let creator_at_setup = unset && who == dao.creator && dao.epoch == 1 && ctx.accounts.governance.data_is_empty();
        require!(who == treasury_address(&dao_key) || creator_at_setup, GovError::Unauthorized);
        let previous = record.mint;
        record.dao = dao_key;
        record.mint = mint;
        record.pool = vortex_core::pool_address(&dao_key, &mint);
        record.bump = ctx.bumps.dao_token;
        emit!(DaoTokenSet { dao: dao_key, previous, mint });
        Ok(())
    }

    /// Runs a passed proposal. Anyone can call it. Pass every account the
    /// proposal's instructions use (and the programs they call) as
    /// remaining accounts.
    pub fn execute<'info>(ctx: Context<'info, Execute<'info>>) -> Result<()> {
        let dao = &ctx.accounts.dao;
        require!(dao.council.is_none(), GovError::AwaitingCouncil);
        let dao_key = dao.key();
        let proposal = &ctx.accounts.proposal;

        // 1. Ask the governance program. It checks the proposal passed and
        //    can run now, marks it executed, and returns its address.
        confirm_with_governance(
            dao,
            &ctx.accounts.executor,
            &ctx.accounts.governance,
            proposal,
            &ctx.accounts.governance_program,
        )?;

        // 2. Read what to run from the proposal's standard prefix.
        let core = ProposalCore::read(&proposal.try_borrow_data()?)?;
        require_keys_eq!(core.hub_dao, dao_key, GovError::WrongDao);
        require!(core.epoch == dao.epoch, GovError::StaleProposal);

        // 3. Run it as the treasury.
        let treasury_seeds: &[&[u8]] = &[TREASURY_SEED, dao_key.as_ref(), &[dao.treasury_bump]];
        execute_instructions(&core.instructions, ctx.remaining_accounts, treasury_seeds)?;
        emit!(ProposalExecuted { dao: dao_key, governance_program: dao.governance_program, proposal: proposal.key() });
        Ok(())
    }

    /// While a council is on: confirms a passed proposal with the governance
    /// program (which marks it executed, as `execute` would) and records it
    /// as waiting for the council. Anyone can call it and pays for the record.
    pub fn submit_to_council(ctx: Context<SubmitToCouncil>) -> Result<()> {
        let dao = &ctx.accounts.dao;
        let council = dao.council.as_ref().ok_or(GovError::NoCouncil)?;
        let dao_key = dao.key();
        let proposal = &ctx.accounts.proposal;
        confirm_with_governance(
            dao,
            &ctx.accounts.executor,
            &ctx.accounts.governance,
            proposal,
            &ctx.accounts.governance_program,
        )?;
        let core = ProposalCore::read(&proposal.try_borrow_data()?)?;
        require_keys_eq!(core.hub_dao, dao_key, GovError::WrongDao);
        require!(core.epoch == dao.epoch, GovError::StaleProposal);

        let w = &mut ctx.accounts.waiting;
        w.dao = dao_key;
        w.proposal = proposal.key();
        w.governance_program = dao.governance_program;
        w.epoch = dao.epoch;
        w.instructions_hash = instructions_hash(&core.instructions)?;
        w.council_nonce = council.nonce;
        w.approvals = Vec::new();
        w.rejections = Vec::new();
        w.status = WaitingStatus::Waiting;
        w.bump = ctx.bumps.waiting;
        emit!(SubmittedToCouncil { dao: dao_key, proposal: w.proposal, waiting: w.key() });
        Ok(())
    }

    /// A council signer confirms a waiting proposal. Approvals given under
    /// an earlier council don't count: they start over.
    pub fn council_confirm(ctx: Context<CouncilVote>) -> Result<()> {
        let council = ctx.accounts.dao.council.as_ref().ok_or(GovError::NoCouncil)?;
        let who = ctx.accounts.signer.key();
        require!(council.signers.contains(&who), GovError::NotCouncilSigner);
        let w = &mut ctx.accounts.waiting;
        require!(w.status == WaitingStatus::Waiting, GovError::ProposalNotExecutable);
        w.catch_up(council.nonce);
        require!(!w.approvals.contains(&who) && !w.rejections.contains(&who), GovError::AlreadyConfirmed);
        w.approvals.push(who);
        emit!(CouncilVoted { waiting: w.key(), signer: who, confirm: true, approvals: w.approvals.len() as u8 });
        Ok(())
    }

    /// A council signer rejects a waiting proposal. Once confirming can no
    /// longer reach the required count, it's rejected for good.
    pub fn council_reject(ctx: Context<CouncilVote>) -> Result<()> {
        let council = ctx.accounts.dao.council.as_ref().ok_or(GovError::NoCouncil)?;
        let who = ctx.accounts.signer.key();
        require!(council.signers.contains(&who), GovError::NotCouncilSigner);
        let w = &mut ctx.accounts.waiting;
        require!(w.status == WaitingStatus::Waiting, GovError::ProposalNotExecutable);
        w.catch_up(council.nonce);
        require!(!w.approvals.contains(&who) && !w.rejections.contains(&who), GovError::AlreadyConfirmed);
        w.rejections.push(who);
        if w.rejections.len() > council.signers.len() - council.required as usize {
            w.status = WaitingStatus::Rejected;
        }
        emit!(CouncilVoted { waiting: w.key(), signer: who, confirm: false, approvals: w.approvals.len() as u8 });
        Ok(())
    }

    /// Runs a waiting proposal once the council has confirmed it. Anyone
    /// can call it, with the same remaining accounts as `execute`. It must
    /// still be the epoch the proposal was made in.
    ///
    /// Two cases need no confirmation: the council has been turned off
    /// since, or the proposal only replaces the council's signers and
    /// their tenure is over.
    pub fn run_confirmed<'info>(ctx: Context<'info, RunConfirmed<'info>>) -> Result<()> {
        let dao = &ctx.accounts.dao;
        let dao_key = dao.key();
        let w = &ctx.accounts.waiting;
        let governance_program = w.governance_program;
        require!(w.status != WaitingStatus::Done, GovError::ProposalAlreadyExecuted);
        require!(w.epoch == dao.epoch, GovError::StaleProposal);
        let proposal = &ctx.accounts.proposal;
        require!(proposal.owner == &w.governance_program, GovError::InvalidProposalAccount);
        let core = ProposalCore::read(&proposal.try_borrow_data()?)?;
        require!(instructions_hash(&core.instructions)? == w.instructions_hash, GovError::InvalidProposalAccount);

        if let Some(council) = &dao.council {
            let confirmed = w.status == WaitingStatus::Waiting
                && w.council_nonce == council.nonce
                && w.approvals.len() >= council.required as usize;
            if !confirmed {
                require!(only_replaces_council(&core.instructions), if w.status == WaitingStatus::Rejected { GovError::CouncilRejected } else { GovError::AwaitingCouncil });
                let now = Clock::get()?.unix_timestamp;
                require!(now >= council.seated_at.saturating_add(council.tenure), GovError::TenureNotOver);
            }
        } else {
            require!(w.status == WaitingStatus::Waiting, GovError::CouncilRejected);
        }
        ctx.accounts.waiting.status = WaitingStatus::Done;
        // Written now: the proposal may call the hub again (set_council).
        ctx.accounts.waiting.exit(&crate::ID)?;

        let treasury_seeds: &[&[u8]] = &[TREASURY_SEED, dao_key.as_ref(), &[dao.treasury_bump]];
        execute_instructions(&core.instructions, ctx.remaining_accounts, treasury_seeds)?;
        emit!(ProposalExecuted { dao: dao_key, governance_program, proposal: proposal.key() });
        Ok(())
    }

    /// Turns the council on, or replaces it: `signers`, `required` of them
    /// to confirm, and a `tenure` (seconds) before the DAO can replace them
    /// without their confirmation. Only a passed proposal (the treasury);
    /// with a council on, that proposal went through the council, or
    /// through the tenure rule.
    pub fn set_council(ctx: Context<TreasuryOnly>, signers: Vec<Pubkey>, required: u8, tenure: i64) -> Result<()> {
        require!(!signers.is_empty() && signers.len() <= MAX_COUNCIL, GovError::TooManySigners);
        require!(required >= 1 && required as usize <= signers.len(), GovError::InvalidApprovals);
        require!(tenure >= 0, GovError::InvalidConfig);
        for (i, s) in signers.iter().enumerate() {
            require!(!signers[..i].contains(s), GovError::AlreadySigner);
        }
        let dao = &mut ctx.accounts.dao;
        let nonce = dao.council.as_ref().map_or(1, |c| c.nonce.wrapping_add(1));
        let seated_at = Clock::get()?.unix_timestamp;
        dao.council = Some(Council { signers: signers.clone(), required, tenure, seated_at, nonce });
        emit!(CouncilSet { dao: dao.key(), signers, required, tenure });
        Ok(())
    }

    /// Grants `recipient` `amount` of `mint` (None: SOL) every 30 days from
    /// now until `ends_at`, at most a year away. Only a passed proposal (the
    /// treasury signs and pays the rent); with a council on, that proposal
    /// went through the council. Any number of allowances, for anyone.
    pub fn create_allowance(ctx: Context<CreateAllowance>, recipient: Pubkey, mint: Option<Pubkey>, amount: u64, ends_at: i64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        check_allowance_terms(amount, now, ends_at)?;
        let dao = &mut ctx.accounts.dao;
        dao.allowance_count += 1;
        let a = &mut ctx.accounts.allowance;
        a.dao = dao.key();
        a.id = dao.allowance_count;
        a.recipient = recipient;
        a.mint = mint;
        a.amount = amount;
        a.start = now;
        a.ends_at = ends_at;
        a.paid_through = 0;
        a.revoked = false;
        a.bump = ctx.bumps.allowance;
        emit!(AllowanceCreated { dao: a.dao, allowance: a.key(), id: a.id, recipient, mint, amount, ends_at });
        Ok(())
    }

    /// Changes an allowance's amount and end (still at most a year from its
    /// start). Periods already paid stay paid. Only a passed proposal.
    pub fn update_allowance(ctx: Context<ChangeAllowance>, amount: u64, ends_at: i64) -> Result<()> {
        let a = &mut ctx.accounts.allowance;
        require!(!a.revoked, GovError::AllowanceEnded);
        check_allowance_terms(amount, a.start, ends_at)?;
        require!(ends_at > Clock::get()?.unix_timestamp, GovError::InvalidAllowance);
        a.amount = amount;
        a.ends_at = ends_at;
        emit!(AllowanceUpdated { allowance: a.key(), amount, ends_at });
        Ok(())
    }

    /// Ends an allowance now. Only a passed proposal.
    pub fn revoke_allowance(ctx: Context<ChangeAllowance>) -> Result<()> {
        let a = &mut ctx.accounts.allowance;
        a.revoked = true;
        emit!(AllowanceRevoked { allowance: a.key() });
        Ok(())
    }

    /// The recipient claims this period's payment, paid by the treasury.
    /// A period not claimed lapses: this only pays the period it's in.
    pub fn claim_allowance(ctx: Context<ClaimAllowance>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let a = &mut ctx.accounts.allowance;
        require!(!a.revoked && now < a.ends_at, GovError::AllowanceEnded);
        let period = ((now - a.start) / ALLOWANCE_PERIOD) as u64 + 1;
        require!(period > a.paid_through, GovError::AllowanceAlreadyClaimed);
        a.paid_through = period;
        let amount = a.amount;
        let dao = &ctx.accounts.dao;
        let dao_key = dao.key();
        let seeds: &[&[u8]] = &[TREASURY_SEED, dao_key.as_ref(), &[dao.treasury_bump]];
        match a.mint {
            None => {
                system_program::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.system_program.key(),
                        system_program::Transfer { from: ctx.accounts.treasury.to_account_info(), to: ctx.accounts.recipient.to_account_info() },
                        &[seeds],
                    ),
                    amount,
                )?;
            }
            Some(mint_key) => {
                let mint = ctx.accounts.mint.as_ref().ok_or(GovError::WrongMint)?;
                require_keys_eq!(mint.key(), mint_key, GovError::WrongMint);
                let from = ctx.accounts.treasury_token.as_ref().ok_or(GovError::WrongMint)?;
                let to = ctx.accounts.recipient_token.as_ref().ok_or(GovError::WrongMint)?;
                let token_program = ctx.accounts.token_program.as_ref().ok_or(GovError::WrongMint)?;
                require_keys_eq!(from.owner, ctx.accounts.treasury.key(), GovError::Unauthorized);
                require_keys_eq!(from.mint, mint_key, GovError::WrongMint);
                require_keys_eq!(to.mint, mint_key, GovError::WrongMint);
                token_interface::transfer_checked(
                    CpiContext::new_with_signer(
                        token_program.key(),
                        TransferChecked {
                            from: from.to_account_info(),
                            mint: mint.to_account_info(),
                            to: to.to_account_info(),
                            authority: ctx.accounts.treasury.to_account_info(),
                        },
                        &[seeds],
                    ),
                    amount,
                    mint.decimals,
                )?;
            }
        }
        emit!(AllowanceClaimed { allowance: a.key(), recipient: a.recipient, period, amount });
        Ok(())
    }

    /// Turns the council off. Only a passed proposal, which the council
    /// confirmed.
    pub fn clear_council(ctx: Context<TreasuryOnly>) -> Result<()> {
        let dao = &mut ctx.accounts.dao;
        require!(dao.council.take().is_some(), GovError::NoCouncil);
        emit!(CouncilCleared { dao: dao.key() });
        Ok(())
    }

    /// Starts a switch to an approved governance program: another one, or a
    /// fresh setup of the one running now (new rules or signers). Needs the
    /// treasury's signature, so only a passed proposal can call it.
    pub fn propose_switch(ctx: Context<ProposeSwitch>, new_program: Pubkey) -> Result<()> {
        let model = &ctx.accounts.model;
        require_keys_eq!(model.program_id, new_program, GovError::ModelNotApproved);
        require!(model.enabled, GovError::ModelNotApproved);
        let dao = &mut ctx.accounts.dao;
        require!(dao.pending_switch.is_none(), GovError::SwitchPending);
        let ready_at = Clock::get()?.unix_timestamp + SWITCH_DELAY;
        dao.pending_switch = Some(PendingSwitch { program: new_program, ready_at });
        emit!(SwitchProposed { dao: dao.key(), from: dao.governance_program, to: new_program, ready_at });
        Ok(())
    }

    /// Calls off a pending switch (again, only through a passed proposal).
    pub fn cancel_switch(ctx: Context<TreasuryOnly>) -> Result<()> {
        let dao = &mut ctx.accounts.dao;
        let pending = dao.pending_switch.take().ok_or(GovError::NoSwitchPending)?;
        emit!(SwitchCancelled { dao: dao.key(), to: pending.program });
        Ok(())
    }

    /// Applies a pending switch once its delay has passed. Anyone can call
    /// it; the DAO must already be set up in the new program.
    pub fn apply_switch(ctx: Context<ApplySwitch>) -> Result<()> {
        let dao_key = ctx.accounts.dao.key();
        let dao = &mut ctx.accounts.dao;
        let pending = dao.pending_switch.clone().ok_or(GovError::NoSwitchPending)?;
        require!(Clock::get()?.unix_timestamp >= pending.ready_at, GovError::SwitchNotReady);
        let model = &ctx.accounts.model;
        require_keys_eq!(model.program_id, pending.program, GovError::ModelNotApproved);
        require!(model.enabled, GovError::ModelNotApproved);
        let governance = &ctx.accounts.new_governance;
        let next = dao.epoch.checked_add(1).ok_or(GovError::Overflow)?;
        require_keys_eq!(governance.key(), governance_address(&dao_key, &pending.program, next), GovError::GovernanceNotInitialized);
        require!(governance.owner == &pending.program && !governance.data_is_empty(), GovError::GovernanceNotInitialized);

        let from = dao.governance_program;
        dao.governance_program = pending.program;
        dao.governance = governance.key();
        dao.epoch = next;
        dao.pending_switch = None;
        emit!(SwitchApplied { dao: dao_key, from, to: pending.program, epoch: dao.epoch });
        Ok(())
    }
}

/// A DAO's account in governance program `program` for `epoch`.
pub fn governance_address(dao: &Pubkey, program: &Pubkey, epoch: u32) -> Pubkey {
    Pubkey::find_program_address(&[GOVERNANCE_SEED, dao.as_ref(), &epoch.to_le_bytes()], program).0
}

/// Asks the DAO's governance program to confirm `proposal` (it checks the
/// proposal passed and can run now, marks it executed, and returns its
/// address), signed by the DAO's executor PDA.
fn confirm_with_governance<'info>(
    dao: &Account<'info, Dao>,
    executor: &UncheckedAccount<'info>,
    governance: &UncheckedAccount<'info>,
    proposal: &UncheckedAccount<'info>,
    governance_program: &UncheckedAccount<'info>,
) -> Result<()> {
    let dao_key = dao.key();
    let mut data = CONFIRM_EXECUTION_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&dao.epoch.to_le_bytes());
    let confirm = Instruction {
        program_id: dao.governance_program,
        accounts: vec![
            AccountMeta::new_readonly(executor.key(), true),
            AccountMeta::new_readonly(governance.key(), false),
            AccountMeta::new(proposal.key(), false),
        ],
        data,
    };
    let executor_seeds: &[&[u8]] = &[EXECUTOR_SEED, dao_key.as_ref(), &[dao.executor_bump]];
    invoke_signed(
        &confirm,
        &[executor.to_account_info(), governance.to_account_info(), proposal.to_account_info(), governance_program.to_account_info()],
        &[executor_seeds],
    )?;
    let (from, answer) = get_return_data().ok_or(GovError::NotConfirmed)?;
    require_keys_eq!(from, dao.governance_program, GovError::NotConfirmed);
    require!(answer.as_slice() == proposal.key().as_ref(), GovError::NotConfirmed);
    Ok(())
}

/// What a waiting proposal will run, pinned when it was submitted.
pub fn instructions_hash(ixs: &[vortex_core::StoredInstruction]) -> Result<[u8; 32]> {
    let mut bytes = Vec::new();
    ixs.serialize(&mut bytes)?;
    Ok(solana_sha256_hasher::hashv(&[&bytes]).to_bytes())
}

/// True when every instruction is the hub's own `set_council`: a signer
/// replacement, which can skip the council once its tenure is over.
pub fn only_replaces_council(ixs: &[vortex_core::StoredInstruction]) -> bool {
    !ixs.is_empty() && ixs.iter().all(|ix| ix.program_id == crate::ID && ix.data.starts_with(crate::instruction::SetCouncil::DISCRIMINATOR))
}

impl Waiting {
    /// Approvals and rejections from an earlier council don't count.
    fn catch_up(&mut self, nonce: u32) {
        if self.council_nonce != nonce {
            self.council_nonce = nonce;
            self.approvals.clear();
            self.rejections.clear();
        }
    }
}

impl Dao {
    /// Checks that `who` may set this DAO up in `program` for `epoch` now:
    /// - the next epoch, by the treasury (a passed proposal), while a
    ///   switch to `program` is pending;
    /// - epoch 1, by the creator, for the DAO's first governance.
    ///
    /// Every governance program's `init_governance` calls this.
    pub fn check_setup(&self, dao_key: &Pubkey, program: &Pubkey, who: &Pubkey, epoch: u32) -> Result<()> {
        if let Some(pending) = &self.pending_switch {
            if pending.program == *program && Some(epoch) == self.epoch.checked_add(1) {
                require_keys_eq!(*who, treasury_address(dao_key), GovError::Unauthorized);
                return Ok(());
            }
        }
        require!(self.governance_program == *program && self.epoch == 1 && epoch == 1, GovError::NotActiveGovernance);
        require_keys_eq!(*who, self.creator, GovError::Unauthorized);
        Ok(())
    }
}

/// A DAO's treasury.
fn check_allowance_terms(amount: u64, start: i64, ends_at: i64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    require!(amount > 0 && ends_at > now && ends_at <= start.saturating_add(MAX_ALLOWANCE_DURATION), GovError::InvalidAllowance);
    Ok(())
}

pub fn allowance_address(dao: &Pubkey, id: u64) -> Pubkey {
    Pubkey::find_program_address(&[ALLOWANCE_SEED, dao.as_ref(), &id.to_le_bytes()], &crate::ID).0
}

pub fn treasury_address(dao: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[TREASURY_SEED, dao.as_ref()], &crate::ID).0
}

/// The PDA the hub signs `confirm_execution` with, for one DAO.
pub fn executor_address(dao: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[EXECUTOR_SEED, dao.as_ref()], &crate::ID).0
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Hub {
    /// Manages the approved governance programs.
    pub admin: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Model {
    pub program_id: Pubkey,
    #[max_len(MAX_NAME_LEN)]
    pub name: String,
    pub enabled: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Dao {
    pub create_key: Pubkey,
    pub creator: Pubkey,
    #[max_len(MAX_NAME_LEN)]
    pub name: String,
    /// The governance program running the DAO now.
    pub governance_program: Pubkey,
    /// The DAO's account in that program: `[GOVERNANCE_SEED, dao, epoch]` there.
    pub governance: Pubkey,
    /// Starts at 1 and goes up with every switch. Proposals only run in
    /// the epoch they were made in.
    pub epoch: u32,
    pub pending_switch: Option<PendingSwitch>,
    pub bump: u8,
    pub treasury_bump: u8,
    pub executor_bump: u8,
    /// The security council, when on.
    pub council: Option<Council>,
    /// How many allowances this DAO has granted (the latest id).
    pub allowance_count: u64,
}

/// A monthly allowance: `[ALLOWANCE_SEED, dao, id]`. The recipient claims
/// `amount` of `mint` (None: SOL) once per 30-day period from `start` until
/// `ends_at`; a period not claimed lapses.
#[account]
#[derive(InitSpace)]
pub struct Allowance {
    pub dao: Pubkey,
    pub id: u64,
    pub recipient: Pubkey,
    pub mint: Option<Pubkey>,
    pub amount: u64,
    pub start: i64,
    pub ends_at: i64,
    /// The last period paid, from 1 (0: none yet).
    pub paid_through: u64,
    pub revoked: bool,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq, InitSpace)]
pub struct Council {
    #[max_len(MAX_COUNCIL)]
    pub signers: Vec<Pubkey>,
    /// How many signers must confirm.
    pub required: u8,
    /// Seconds after `seated_at` before the DAO can replace the signers
    /// without their confirmation.
    pub tenure: i64,
    pub seated_at: i64,
    /// Goes up with every change, so approvals given to an earlier council
    /// don't count.
    pub nonce: u32,
}

/// A passed proposal waiting for the council: `[WAITING_SEED, dao, proposal]`.
#[account]
#[derive(InitSpace)]
pub struct Waiting {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub governance_program: Pubkey,
    pub epoch: u32,
    /// Hash of the proposal's instructions when it was submitted.
    pub instructions_hash: [u8; 32],
    /// The council the votes below were given under.
    pub council_nonce: u32,
    #[max_len(MAX_COUNCIL)]
    pub approvals: Vec<Pubkey>,
    #[max_len(MAX_COUNCIL)]
    pub rejections: Vec<Pubkey>,
    pub status: WaitingStatus,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum WaitingStatus {
    Waiting,
    Rejected,
    Done,
}

/// A waiting proposal's record.
pub fn waiting_address(dao: &Pubkey, proposal: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[WAITING_SEED, dao.as_ref(), proposal.as_ref()], &crate::ID).0
}

/// The DAO's token: `[TOKEN_SEED, dao]`. Members stake in `pool`, the
/// only pool that takes new stake, and token-based governance reads voting
/// power from it.
#[account]
#[derive(InitSpace)]
pub struct DaoToken {
    pub dao: Pubkey,
    pub mint: Pubkey,
    /// vortex-stake's pool for `mint`.
    pub pool: Pubkey,
    pub bump: u8,
}

/// A DAO's token record.
pub fn dao_token_address(dao: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[TOKEN_SEED, dao.as_ref()], &crate::ID).0
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq, InitSpace)]
pub struct PendingSwitch {
    pub program: Pubkey,
    pub ready_at: i64,
}

// ---------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
pub struct InitHub<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Hub::INIT_SPACE, seeds = [HUB_SEED], bump)]
    pub hub: Account<'info, Hub>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [HUB_SEED], bump = hub.bump, has_one = admin @ GovError::Unauthorized)]
    pub hub: Account<'info, Hub>,
}

#[derive(Accounts)]
#[instruction(program_id: Pubkey)]
pub struct SetModel<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [HUB_SEED], bump = hub.bump, has_one = admin @ GovError::Unauthorized)]
    pub hub: Account<'info, Hub>,
    #[account(init_if_needed, payer = admin, space = 8 + Model::INIT_SPACE, seeds = [MODEL_SEED, program_id.as_ref()], bump)]
    pub model: Account<'info, Model>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(name: String, governance_program: Pubkey)]
pub struct CreateDao<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    pub create_key: Signer<'info>,
    #[account(seeds = [MODEL_SEED, governance_program.as_ref()], bump = model.bump)]
    pub model: Account<'info, Model>,
    #[account(init, payer = creator, space = 8 + Dao::INIT_SPACE, seeds = [DAO_SEED, create_key.key().as_ref()], bump)]
    pub dao: Account<'info, Dao>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetDaoToken<'info> {
    /// The creator (first record, before the first governance is set up)
    /// or the treasury (a passed proposal).
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub dao: Account<'info, Dao>,
    /// CHECK: the DAO's current governance account; only read for whether
    /// it has been set up yet.
    #[account(address = dao.governance @ GovError::WrongDao)]
    pub governance: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, space = 8 + DaoToken::INIT_SPACE, seeds = [TOKEN_SEED, dao.key().as_ref()], bump)]
    pub dao_token: Account<'info, DaoToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Execute<'info> {
    /// Not `mut`: a proposal may change it (propose_switch), and this
    /// instruction must not write a stale copy back over that.
    pub dao: Account<'info, Dao>,
    /// CHECK: the DAO's executor PDA; signs the confirm_execution call.
    #[account(seeds = [EXECUTOR_SEED, dao.key().as_ref()], bump = dao.executor_bump)]
    pub executor: UncheckedAccount<'info>,
    /// CHECK: must be the DAO's current governance program.
    #[account(executable, address = dao.governance_program @ GovError::NotActiveGovernance)]
    pub governance_program: UncheckedAccount<'info>,
    /// CHECK: the DAO's account in that program; that program checks it.
    #[account(address = dao.governance @ GovError::NotActiveGovernance)]
    pub governance: UncheckedAccount<'info>,
    /// CHECK: owned by the governance program, which confirms it; the hub
    /// reads only its standard ProposalCore prefix.
    #[account(mut, owner = dao.governance_program @ GovError::InvalidProposalAccount)]
    pub proposal: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SubmitToCouncil<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub dao: Account<'info, Dao>,
    /// CHECK: the DAO's executor PDA; signs the confirm_execution call.
    #[account(seeds = [EXECUTOR_SEED, dao.key().as_ref()], bump = dao.executor_bump)]
    pub executor: UncheckedAccount<'info>,
    /// CHECK: must be the DAO's current governance program.
    #[account(executable, address = dao.governance_program @ GovError::NotActiveGovernance)]
    pub governance_program: UncheckedAccount<'info>,
    /// CHECK: the DAO's account in that program; that program checks it.
    #[account(address = dao.governance @ GovError::NotActiveGovernance)]
    pub governance: UncheckedAccount<'info>,
    /// CHECK: owned by the governance program, which confirms it.
    #[account(mut, owner = dao.governance_program @ GovError::InvalidProposalAccount)]
    pub proposal: UncheckedAccount<'info>,
    #[account(
        init, payer = payer, space = 8 + Waiting::INIT_SPACE,
        seeds = [WAITING_SEED, dao.key().as_ref(), proposal.key().as_ref()], bump,
    )]
    pub waiting: Account<'info, Waiting>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CouncilVote<'info> {
    pub signer: Signer<'info>,
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub waiting: Account<'info, Waiting>,
}

#[derive(Accounts)]
pub struct RunConfirmed<'info> {
    /// Not `mut`: the proposal may change it (set_council, propose_switch).
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao, has_one = proposal @ GovError::InvalidProposalAccount)]
    pub waiting: Account<'info, Waiting>,
    /// CHECK: the proposal; its owner and instructions are checked against the record.
    pub proposal: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(new_program: Pubkey)]
pub struct ProposeSwitch<'info> {
    #[account(seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub dao: Account<'info, Dao>,
    #[account(seeds = [MODEL_SEED, new_program.as_ref()], bump = model.bump)]
    pub model: Account<'info, Model>,
}

#[derive(Accounts)]
pub struct CreateAllowance<'info> {
    /// The DAO's treasury: a passed proposal. It pays the rent.
    #[account(mut, seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub dao: Account<'info, Dao>,
    #[account(
        init,
        payer = treasury,
        space = 8 + Allowance::INIT_SPACE,
        seeds = [ALLOWANCE_SEED, dao.key().as_ref(), &(dao.allowance_count + 1).to_le_bytes()],
        bump,
    )]
    pub allowance: Account<'info, Allowance>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ChangeAllowance<'info> {
    #[account(seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: Signer<'info>,
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao)]
    pub allowance: Account<'info, Allowance>,
}

#[derive(Accounts)]
pub struct ClaimAllowance<'info> {
    #[account(mut)]
    pub recipient: Signer<'info>,
    pub dao: Account<'info, Dao>,
    #[account(mut, has_one = dao @ GovError::WrongDao, has_one = recipient @ GovError::Unauthorized)]
    pub allowance: Account<'info, Allowance>,
    /// CHECK: the DAO's treasury PDA (seeds checked); pays SOL allowances
    /// and owns the token account for token ones.
    #[account(mut, seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    pub mint: Option<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub treasury_token: Option<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub recipient_token: Option<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Option<Interface<'info, TokenInterface>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct TreasuryOnly<'info> {
    #[account(seeds = [TREASURY_SEED, dao.key().as_ref()], bump = dao.treasury_bump)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub dao: Account<'info, Dao>,
}

#[derive(Accounts)]
pub struct ApplySwitch<'info> {
    #[account(mut)]
    pub dao: Account<'info, Dao>,
    pub model: Account<'info, Model>,
    /// CHECK: the DAO's account in the new program; checked in the handler.
    pub new_governance: UncheckedAccount<'info>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct ModelSet {
    pub program_id: Pubkey,
    pub name: String,
    pub enabled: bool,
}

#[event]
pub struct DaoCreated {
    pub dao: Pubkey,
    pub name: String,
    pub creator: Pubkey,
    pub governance_program: Pubkey,
}

#[event]
pub struct ProposalExecuted {
    pub dao: Pubkey,
    pub governance_program: Pubkey,
    pub proposal: Pubkey,
}

#[event]
pub struct SwitchProposed {
    pub dao: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
    pub ready_at: i64,
}

#[event]
pub struct DaoTokenSet {
    pub dao: Pubkey,
    /// `Pubkey::default()` when first recorded.
    pub previous: Pubkey,
    pub mint: Pubkey,
}

#[event]
pub struct SwitchCancelled {
    pub dao: Pubkey,
    pub to: Pubkey,
}

#[event]
pub struct SwitchApplied {
    pub dao: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
    pub epoch: u32,
}

#[event]
pub struct SubmittedToCouncil {
    pub dao: Pubkey,
    pub proposal: Pubkey,
    pub waiting: Pubkey,
}

#[event]
pub struct CouncilVoted {
    pub waiting: Pubkey,
    pub signer: Pubkey,
    pub confirm: bool,
    pub approvals: u8,
}

#[event]
pub struct CouncilSet {
    pub dao: Pubkey,
    pub signers: Vec<Pubkey>,
    pub required: u8,
    pub tenure: i64,
}

#[event]
pub struct AllowanceCreated {
    pub dao: Pubkey,
    pub allowance: Pubkey,
    pub id: u64,
    pub recipient: Pubkey,
    pub mint: Option<Pubkey>,
    pub amount: u64,
    pub ends_at: i64,
}

#[event]
pub struct AllowanceUpdated {
    pub allowance: Pubkey,
    pub amount: u64,
    pub ends_at: i64,
}

#[event]
pub struct AllowanceRevoked {
    pub allowance: Pubkey,
}

#[event]
pub struct AllowanceClaimed {
    pub allowance: Pubkey,
    pub recipient: Pubkey,
    pub period: u64,
    pub amount: u64,
}

#[event]
pub struct CouncilCleared {
    pub dao: Pubkey,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dao(program: Pubkey, epoch: u32, pending: Option<Pubkey>, creator: Pubkey) -> Dao {
        Dao {
            create_key: Pubkey::new_unique(),
            creator,
            name: "t".into(),
            governance_program: program,
            governance: Pubkey::default(),
            epoch,
            pending_switch: pending.map(|p| PendingSwitch { program: p, ready_at: 0 }),
            bump: 0,
            treasury_bump: 0,
            executor_bump: 0,
            council: None,
            allowance_count: 0,
        }
    }

    #[test]
    fn who_sets_up_which_epoch() {
        let (key, creator, a, b) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let treasury = treasury_address(&key);

        // First governance: epoch 1, by the creator only.
        let d = dao(a, 1, None, creator);
        assert!(d.check_setup(&key, &a, &creator, 1).is_ok());
        assert!(d.check_setup(&key, &a, &treasury, 1).is_err());
        assert!(d.check_setup(&key, &a, &creator, 2).is_err());
        assert!(d.check_setup(&key, &b, &creator, 1).is_err(), "not the DAO's program");

        // A pending switch to b: epoch 2, by the treasury only.
        let d = dao(a, 1, Some(b), creator);
        assert!(d.check_setup(&key, &b, &treasury, 2).is_ok());
        assert!(d.check_setup(&key, &b, &creator, 2).is_err());
        assert!(d.check_setup(&key, &b, &treasury, 1).is_err());
        assert!(d.check_setup(&key, &b, &treasury, 3).is_err());

        // A fresh copy of the running program: the next epoch, by the treasury.
        let d = dao(a, 4, Some(a), creator);
        assert!(d.check_setup(&key, &a, &treasury, 5).is_ok());
        assert!(d.check_setup(&key, &a, &treasury, 4).is_err(), "the running epoch already exists");
        assert!(d.check_setup(&key, &a, &creator, 5).is_err());

        // No pending switch after epoch 1: nobody sets anything up.
        let d = dao(a, 2, None, creator);
        assert!(d.check_setup(&key, &a, &treasury, 3).is_err());
        assert!(d.check_setup(&key, &a, &creator, 1).is_err());
    }

    #[test]
    fn every_epoch_has_its_own_address() {
        let (key, program) = (Pubkey::new_unique(), Pubkey::new_unique());
        let one = governance_address(&key, &program, 1);
        assert_ne!(one, governance_address(&key, &program, 2));
        assert_eq!(one, Pubkey::find_program_address(&[GOVERNANCE_SEED, key.as_ref(), &1u32.to_le_bytes()], &program).0);
    }
}
