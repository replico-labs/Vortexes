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
//! The hub's admin keeps the list of approved governance programs: a DAO
//! can only be created with, or switched to, an approved one.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{get_return_data, invoke_signed},
};
use vortex_core::{
    execute_instructions, validate_name, GovError, ProposalCore, CONFIRM_EXECUTION_DISCRIMINATOR, EXECUTOR_SEED,
    GOVERNANCE_SEED, MAX_NAME_LEN, TREASURY_SEED,
};

declare_id!("5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe");

pub const HUB_SEED: &[u8] = b"hub";
pub const DAO_SEED: &[u8] = b"dao";
pub const MODEL_SEED: &[u8] = b"model";

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
        dao.governance = governance_address(&dao_key, &governance_program);
        dao.epoch = 1;
        dao.pending_switch = None;
        dao.bump = ctx.bumps.dao;
        dao.treasury_bump = Pubkey::find_program_address(&[TREASURY_SEED, dao_key.as_ref()], &crate::ID).1;
        dao.executor_bump = Pubkey::find_program_address(&[EXECUTOR_SEED, dao_key.as_ref()], &crate::ID).1;
        emit!(DaoCreated { dao: dao_key, name: dao.name.clone(), creator: dao.creator, governance_program });
        Ok(())
    }

    /// Runs a passed proposal. Anyone can call it. Pass every account the
    /// proposal's instructions use (and the programs they call) as
    /// remaining accounts.
    pub fn execute<'info>(ctx: Context<'info, Execute<'info>>) -> Result<()> {
        let dao = &ctx.accounts.dao;
        let dao_key = dao.key();
        let proposal = &ctx.accounts.proposal;

        // 1. Ask the governance program. It checks the proposal passed and
        //    can run now, marks it executed, and returns its address.
        let mut data = CONFIRM_EXECUTION_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&dao.epoch.to_le_bytes());
        let confirm = Instruction {
            program_id: dao.governance_program,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.executor.key(), true),
                AccountMeta::new_readonly(ctx.accounts.governance.key(), false),
                AccountMeta::new(proposal.key(), false),
            ],
            data,
        };
        let executor_seeds: &[&[u8]] = &[EXECUTOR_SEED, dao_key.as_ref(), &[dao.executor_bump]];
        invoke_signed(
            &confirm,
            &[
                ctx.accounts.executor.to_account_info(),
                ctx.accounts.governance.to_account_info(),
                proposal.to_account_info(),
                ctx.accounts.governance_program.to_account_info(),
            ],
            &[executor_seeds],
        )?;
        let (from, answer) = get_return_data().ok_or(GovError::NotConfirmed)?;
        require_keys_eq!(from, dao.governance_program, GovError::NotConfirmed);
        require!(answer.as_slice() == proposal.key().as_ref(), GovError::NotConfirmed);

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

    /// Starts a switch to another approved governance program. Needs the
    /// treasury's signature, so only a passed proposal can call it.
    pub fn propose_switch(ctx: Context<ProposeSwitch>, new_program: Pubkey) -> Result<()> {
        let model = &ctx.accounts.model;
        require_keys_eq!(model.program_id, new_program, GovError::ModelNotApproved);
        require!(model.enabled, GovError::ModelNotApproved);
        let dao = &mut ctx.accounts.dao;
        require_keys_neq!(new_program, dao.governance_program, GovError::AlreadyActive);
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
        require_keys_eq!(governance.key(), governance_address(&dao_key, &pending.program), GovError::GovernanceNotInitialized);
        require!(governance.owner == &pending.program && !governance.data_is_empty(), GovError::GovernanceNotInitialized);

        let from = dao.governance_program;
        dao.governance_program = pending.program;
        dao.governance = governance.key();
        dao.epoch = dao.epoch.checked_add(1).ok_or(GovError::Overflow)?;
        dao.pending_switch = None;
        emit!(SwitchApplied { dao: dao_key, from, to: pending.program, epoch: dao.epoch });
        Ok(())
    }
}

/// A DAO's account in governance program `program`.
pub fn governance_address(dao: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[GOVERNANCE_SEED, dao.as_ref()], program).0
}

/// A DAO's treasury.
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
    /// The DAO's account in that program: `[GOVERNANCE_SEED, dao]` there.
    pub governance: Pubkey,
    /// Starts at 1 and goes up with every switch. Proposals only run in
    /// the epoch they were made in.
    pub epoch: u32,
    pub pending_switch: Option<PendingSwitch>,
    pub bump: u8,
    pub treasury_bump: u8,
    pub executor_bump: u8,
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
