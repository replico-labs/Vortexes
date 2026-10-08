//! Vortexes conviction governance, the Solana counterpart of the EVM
//! `ConvictionGovernance.sol`: no voting rounds. Members back a proposal
//! with their deposit, support builds up "conviction" over time, and a
//! proposal can run once its conviction reaches the bar set when it was
//! made.
//!
//! - Members deposit the DAO's token into its vault.
//! - Anyone with at least `proposal_threshold` deposited proposes. The bar
//!   (`required_conviction`) is fixed then: the larger of `min_conviction`
//!   and `support_bps` of all deposits at that moment.
//! - `support` backs one proposal with the member's whole deposit; backing
//!   another moves it there. The backing amount is locked until withdrawn
//!   with `withdraw_support` (or moved). Tokens deposited later don't join
//!   automatically: support the same proposal again to add them.
//! - Conviction moves toward the proposal's current total support by at
//!   most `growth_rate` per second, and never overshoots: it rises while
//!   support holds and falls back when support leaves. So a short spike of
//!   support isn't enough; it has to last. (A linear ramp, as in the EVM
//!   contract, rather than the exponential curve of the original model.)
//! - Once conviction reaches the bar, anyone queues it; it runs through the
//!   hub after `timelock`, within `execution_period`.
//!
//! Spending budgets (as in the EVM contract's version 2):
//! - The DAO keeps a list of assets it protects: SOL and its own token from
//!   the start, more by proposal, each with a weight. A token is watched in
//!   the treasury's associated token account for it.
//! - Every proposal declares a budget: how much of each listed asset it may
//!   spend. Its bar rises by `weight x amount / treasury's holding` for each,
//!   fixed when proposed. Proposals that could loosen the rules or hand
//!   control away (any call to this program except `add_asset`, any call to
//!   the hub such as switching model, a token `SetAuthority`, or any call
//!   to the upgradeable loader) face the bar of spending everything instead.
//! - The program wraps each proposal's instructions in two of its own:
//!   `record_balances` first and `check_budget` last. The hub runs them with
//!   the rest, so if a watched balance dropped by more than its budget, or a
//!   watched account was closed or given a new owner, delegate or close
//!   authority, the whole execution fails and nothing changes.
//! - Weight cuts and removals wait `WEIGHT_CUT_DELAY` before they apply.
//!   Assets added after a proposal was made stop it from running, so a
//!   newly protected asset is never unwatched.
//! - Only listed assets are watched, as in the EVM contract.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::set_return_data;
use anchor_lang::Discriminator;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use vortex_core::{
    execution_expired, timelock_complete, validate_instructions, validate_uri, GovError, Lifecycle, ProposalCore,
    StoredAccountMeta, StoredInstruction, VotingConfig, GOVERNANCE_SEED, MAX_BPS, MAX_INSTRUCTIONS,
};
use vortex_hub::Dao as HubDao;

declare_id!("9sxWrySQv33tdf7WZQKW7zkDrjeq1EiphKqBgq2kxrWQ");

pub const VAULT_SEED: &[u8] = b"vault";
pub const VOTER_SEED: &[u8] = b"voter";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
/// Assets the list can hold, SOL included.
pub const MAX_ASSETS: usize = 10;
/// How long a weight cut or removal waits before it applies (7 days).
pub const WEIGHT_CUT_DELAY: i64 = 7 * 24 * 60 * 60;
/// The list's entry for SOL (the treasury's own balance).
pub const SOL: Pubkey = Pubkey::new_from_array([0; 32]);
/// Instructions the program adds to each proposal (record and check).
pub const BUDGET_STEPS: usize = 2;

#[program]
pub mod vortex_conviction {
    use super::*;

    /// Sets up a hub DAO in this program: rules, token, vault, and the asset
    /// list (SOL and the DAO's token, with these weights). The creator right
    /// after `vortex_hub::create_dao`, or the DAO's treasury (inside a
    /// passed proposal) when switching to this model.
    pub fn init_governance(ctx: Context<InitGovernance>, config: ConvictionConfig, sol_weight: u64, token_weight: u64) -> Result<()> {
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
        g.config = config;
        g.proposal_count = 0;
        g.total_deposited = 0;
        let token_program = ctx.accounts.token_program.key();
        g.assets = vec![
            ListedAsset { mint: SOL, token_program: Pubkey::default(), account: treasury, weight: sol_weight, pending: None },
            ListedAsset {
                mint: g.mint,
                token_program,
                account: treasury_token_account(&treasury, &token_program, &g.mint),
                weight: token_weight,
                pending: None,
            },
        ];
        g.bump = ctx.bumps.governance;
        emit!(GovernanceInitialized { hub_dao: hub_dao_key, governance: g.key(), mint: g.mint });
        Ok(())
    }

    /// Deposits the DAO's token. To back a proposal with it, `support`
    /// (again, if already supporting one).
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

    /// Withdraws deposited tokens that aren't backing a proposal.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, GovError::InvalidAmount);
        let voter = &mut ctx.accounts.voter;
        require!(amount <= voter.amount, GovError::InsufficientDeposit);
        require!(voter.amount - amount >= voter.support_weight, GovError::TokensLocked);
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

    /// Proposes instructions for the treasury to run (at most
    /// MAX_INSTRUCTIONS - BUDGET_STEPS) with a spending budget, and fixes
    /// the conviction they need. `id` must be proposal_count + 1. Remaining
    /// accounts: each listed asset's account, in list order (the treasury
    /// for SOL), to read today's holdings.
    pub fn propose<'info>(
        ctx: Context<'info, Propose<'info>>,
        id: u64,
        metadata_uri: String,
        instructions: Vec<StoredInstruction>,
        budget: Vec<AssetAmount>,
    ) -> Result<()> {
        let hub_dao = &ctx.accounts.hub_dao;
        require_keys_eq!(hub_dao.governance_program, crate::ID, GovError::NotActiveGovernance);
        let proposal_key = ctx.accounts.proposal.key();
        let g = &mut ctx.accounts.governance;
        require!(id == g.proposal_count + 1, GovError::WrongDao);
        validate_uri(&metadata_uri)?;
        require!(instructions.len() + BUDGET_STEPS <= MAX_INSTRUCTIONS, GovError::TooManyInstructions);
        validate_instructions(&instructions, &g.treasury)?;
        require!(!instructions.iter().any(is_budget_step), GovError::ReservedInstruction);
        let power = ctx.accounts.voter.as_ref().map_or(0, |v| v.amount);
        require!(power >= g.config.proposal_threshold, GovError::ProposalThresholdNotMet);
        for (i, b) in budget.iter().enumerate() {
            require!(b.amount > 0, GovError::InvalidAmount);
            require!(g.asset(&b.mint).is_some(), GovError::AssetNotListed);
            require!(!budget[..i].iter().any(|x| x.mint == b.mint), GovError::DuplicateBudgetAsset);
        }

        // The bar: deposits-based base, plus the budget's share of holdings
        // (or everything, for a proposal that could loosen the rules).
        let holdings = read_holdings(g, ctx.remaining_accounts)?;
        let weakens = instructions.iter().any(weakens_rules);
        let extra = if weakens { g.total_weight() } else { budget_cost(&g.assets, &holdings, &budget) };
        let required = g.config.required_conviction(g.total_deposited).saturating_add(extra);

        // Wrap the instructions in the balance record and check.
        let watched: Vec<Watched> = g
            .assets
            .iter()
            .map(|a| Watched { mint: a.mint, token_program: a.token_program, account: a.account, before: AssetState::default() })
            .collect();
        let mut wrapped = Vec::with_capacity(instructions.len() + BUDGET_STEPS);
        wrapped.push(budget_step(&g.treasury, &g.key(), &proposal_key, &watched, crate::instruction::RecordBalances::DISCRIMINATOR));
        wrapped.extend(instructions);
        wrapped.push(budget_step(&g.treasury, &g.key(), &proposal_key, &watched, crate::instruction::CheckBudget::DISCRIMINATOR));

        let now = Clock::get()?.unix_timestamp;
        g.proposal_count = id;
        let p = &mut ctx.accounts.proposal;
        p.core = ProposalCore { hub_dao: g.hub_dao, epoch: hub_dao.epoch, instructions: wrapped };
        p.governance = g.key();
        p.id = id;
        p.proposer = ctx.accounts.proposer.key();
        p.metadata_uri = metadata_uri;
        p.created_at = now;
        p.required_conviction = required;
        p.conviction = 0;
        p.total_support = 0;
        p.last_update = now;
        p.queued_at = 0;
        p.executed = false;
        p.cancelled = false;
        p.bump = ctx.bumps.proposal;
        p.budget = budget;
        p.weakens_rules = weakens;
        p.watched = watched;
        p.budget_stage = 0;
        emit!(ProposalCreated {
            hub_dao: g.hub_dao,
            proposal: p.key(),
            id,
            proposer: p.proposer,
            metadata_uri: p.metadata_uri.clone(),
            budget: p.budget.clone(),
            weakens_rules: weakens,
            required_conviction: p.required_conviction,
        });
        Ok(())
    }

    /// Backs `proposal` with the member's whole deposit, locking it. If they
    /// were backing another proposal, pass it as `previous`: their support
    /// moves from there to here. Supporting the same proposal again tops
    /// it up with tokens deposited since.
    pub fn support(ctx: Context<Support>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let rate = ctx.accounts.governance.config.growth_rate;
        let voter = &mut ctx.accounts.voter;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        let weight = voter.amount;
        require!(weight > 0, GovError::NoVotingPower);

        if voter.supporting == p.key() {
            require!(weight != voter.support_weight, GovError::AlreadySupporting);
            p.settle(rate, now);
            p.total_support = p.total_support - voter.support_weight + weight;
        } else if voter.supporting != Pubkey::default() {
            let prev = ctx.accounts.previous.as_mut().ok_or(GovError::NotSupporting)?;
            require_keys_eq!(prev.key(), voter.supporting, GovError::NotSupporting);
            prev.settle(rate, now);
            prev.total_support -= voter.support_weight;
            emit!(SupportWithdrawn { hub_dao: prev.core.hub_dao, proposal: prev.key(), id: prev.id, supporter: voter.owner, weight: voter.support_weight });
        }
        if voter.supporting != p.key() {
            p.settle(rate, now);
            p.total_support = p.total_support.checked_add(weight).ok_or(GovError::Overflow)?;
        }
        voter.supporting = p.key();
        voter.support_weight = weight;
        emit!(Supported { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, supporter: voter.owner, weight });
        Ok(())
    }

    /// Stops backing the member's current proposal, unlocking their deposit.
    pub fn withdraw_support(ctx: Context<WithdrawSupport>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let rate = ctx.accounts.governance.config.growth_rate;
        let voter = &mut ctx.accounts.voter;
        let p = &mut ctx.accounts.proposal;
        require!(voter.supporting != Pubkey::default() && voter.supporting == p.key(), GovError::NotSupporting);
        p.settle(rate, now);
        p.total_support -= voter.support_weight;
        emit!(SupportWithdrawn { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, supporter: voter.owner, weight: voter.support_weight });
        voter.supporting = Pubkey::default();
        voter.support_weight = 0;
        Ok(())
    }

    /// Queues a proposal whose conviction has reached its bar. Anyone.
    pub fn queue(ctx: Context<Queue>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config = ctx.accounts.governance.config;
        let p = &mut ctx.accounts.proposal;
        require!(!p.executed, GovError::ProposalAlreadyExecuted);
        require!(!p.cancelled, GovError::ProposalAlreadyCancelled);
        require!(p.queued_at == 0, GovError::ProposalAlreadyQueued);
        p.settle(config.growth_rate, now);
        require!(p.conviction >= p.required_conviction, GovError::ConvictionNotReached);
        p.queued_at = now;
        emit!(ProposalQueued { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id, executable_at: now + config.timelock as i64 });
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
        // Every asset listed now must be one this proposal watches.
        for a in &g.assets {
            require!(p.watched.iter().any(|w| w.account == a.account), GovError::AssetListChanged);
        }
        let life = p.lifecycle();
        require!(timelock_complete(&life, &timing, now), GovError::ProposalNotExecutable);
        require!(!execution_expired(&life, &timing, now), GovError::ProposalExpired);
        p.executed = true;
        set_return_data(p.key().as_ref());
        emit!(ProposalExecuted { hub_dao: p.core.hub_dao, proposal: p.key(), id: p.id });
        Ok(())
    }

    /// Cancels a proposal that hasn't executed: its proposer, or the DAO
    /// through a proposal. Supporters then withdraw their support.
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

    /// Budget step 1, added to every proposal: records the watched balances.
    /// Runs only inside the hub's execution of this proposal (the treasury
    /// signs), right after `confirm_execution`. Remaining accounts: the
    /// watched token accounts, in order.
    pub fn record_balances<'info>(ctx: Context<'info, BudgetStep<'info>>) -> Result<()> {
        let treasury = ctx.accounts.treasury.to_account_info();
        let p = &mut ctx.accounts.proposal;
        require!(p.executed && p.budget_stage == 0, GovError::BudgetStepOutOfOrder);
        let mut rest = ctx.remaining_accounts.iter();
        for w in p.watched.iter_mut() {
            w.before = watched_state(w, &treasury, &mut rest)?;
        }
        p.budget_stage = 1;
        Ok(())
    }

    /// Budget step 2, the proposal's last instruction: fails the whole
    /// execution if a watched balance dropped by more than its budget, or a
    /// watched account was closed or given a new owner, delegate or close
    /// authority.
    pub fn check_budget<'info>(ctx: Context<'info, BudgetStep<'info>>) -> Result<()> {
        let treasury = ctx.accounts.treasury.to_account_info();
        let p = &mut ctx.accounts.proposal;
        require!(p.executed && p.budget_stage == 1, GovError::BudgetStepOutOfOrder);
        let mut rest = ctx.remaining_accounts.iter();
        for w in &p.watched {
            let now = watched_state(w, &treasury, &mut rest)?;
            let b = &w.before;
            let intact = if w.mint == SOL {
                *treasury.owner == anchor_lang::system_program::ID && treasury.data_is_empty()
            } else if b.exists {
                now.exists && now.owner == b.owner && now.delegate == b.delegate && now.close_authority == b.close_authority
            } else {
                !now.exists || (now.owner == treasury.key() && now.delegate.is_none() && now.close_authority.is_none())
            };
            require!(intact, GovError::WatchedAccountChanged);
            let spent = b.balance.saturating_sub(now.balance);
            let allowed = p.budget.iter().find(|x| x.mint == w.mint).map_or(0, |x| x.amount);
            require!(spent <= allowed, GovError::BudgetExceeded);
        }
        p.budget_stage = 2;
        Ok(())
    }

    /// Lists a token (only through a passed proposal). Applies at once.
    pub fn add_asset(ctx: Context<AddAsset>, weight: u64) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let token_program = *ctx.accounts.mint.to_account_info().owner;
        let g = &mut ctx.accounts.governance;
        require!(g.asset(&mint).is_none(), GovError::AssetAlreadyListed);
        require!(g.assets.len() < MAX_ASSETS, GovError::TooManyAssets);
        let account = treasury_token_account(&g.treasury, &token_program, &mint);
        g.assets.push(ListedAsset { mint, token_program, account, weight, pending: None });
        emit!(AssetListed { hub_dao: g.hub_dao, mint, account, weight });
        Ok(())
    }

    /// Changes an asset's weight (only through a passed proposal). A raise
    /// applies at once; a cut waits WEIGHT_CUT_DELAY.
    pub fn set_asset_weight(ctx: Context<TreasuryOnly>, mint: Pubkey, weight: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let a = g.asset_mut(&mint).ok_or(GovError::AssetNotListed)?;
        if weight >= a.weight {
            emit!(AssetWeightChanged { hub_dao, mint, from: a.weight, to: weight });
            a.weight = weight;
            a.pending = None;
        } else {
            let effective_at = now + WEIGHT_CUT_DELAY;
            a.pending = Some(PendingAssetChange { weight, remove: false, effective_at });
            emit!(AssetChangeScheduled { hub_dao, mint, weight, remove: false, effective_at });
        }
        Ok(())
    }

    /// Schedules taking a token off the list (only through a passed
    /// proposal); it applies after WEIGHT_CUT_DELAY. SOL stays listed.
    pub fn remove_asset(ctx: Context<TreasuryOnly>, mint: Pubkey) -> Result<()> {
        require!(mint != SOL, GovError::CantRemoveSol);
        let effective_at = Clock::get()?.unix_timestamp + WEIGHT_CUT_DELAY;
        let g = &mut ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let a = g.asset_mut(&mint).ok_or(GovError::AssetNotListed)?;
        a.pending = Some(PendingAssetChange { weight: 0, remove: true, effective_at });
        emit!(AssetChangeScheduled { hub_dao, mint, weight: 0, remove: true, effective_at });
        Ok(())
    }

    /// Applies a scheduled weight cut or removal once it's due. Anyone.
    pub fn apply_asset_change(ctx: Context<ApplyAssetChange>, mint: Pubkey) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let g = &mut ctx.accounts.governance;
        let hub_dao = g.hub_dao;
        let at = g.assets.iter().position(|a| a.mint == mint).ok_or(GovError::AssetNotListed)?;
        let change = g.assets[at].pending.ok_or(GovError::NoPendingAssetChange)?;
        require!(now >= change.effective_at, GovError::AssetChangeNotDue);
        if change.remove {
            g.assets.remove(at);
            emit!(AssetRemoved { hub_dao, mint });
        } else {
            let a = &mut g.assets[at];
            emit!(AssetWeightChanged { hub_dao, mint, from: a.weight, to: change.weight });
            a.weight = change.weight;
            a.pending = None;
        }
        Ok(())
    }

    /// Replaces the rules (only through a passed proposal). Bars already
    /// set on existing proposals don't change.
    pub fn update_config(ctx: Context<TreasuryOnly>, config: ConvictionConfig) -> Result<()> {
        config.validate()?;
        let g = &mut ctx.accounts.governance;
        g.config = config;
        emit!(ConfigUpdated { hub_dao: g.hub_dao, config });
        Ok(())
    }
}

/// The treasury's associated token account for `mint`.
pub fn treasury_token_account(treasury: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[treasury.as_ref(), token_program.as_ref(), mint.as_ref()], &anchor_spl::associated_token::ID).0
}

fn is_budget_step(ix: &StoredInstruction) -> bool {
    ix.program_id == crate::ID
        && (ix.data.starts_with(crate::instruction::RecordBalances::DISCRIMINATOR)
            || ix.data.starts_with(crate::instruction::CheckBudget::DISCRIMINATOR))
}

/// SPL Token and Token-2022's `SetAuthority` instruction tag.
const TOKEN_SET_AUTHORITY: u8 = 6;

/// Whether an instruction could loosen the rules or hand control away: any
/// call to this program except `add_asset`; any call to the hub (switching
/// model included); a token `SetAuthority` (say, giving away a mint
/// authority the treasury holds); or any call to the upgradeable loader.
pub fn weakens_rules(ix: &StoredInstruction) -> bool {
    let token = ix.program_id == anchor_spl::token::ID || ix.program_id == anchor_spl::token_2022::ID;
    ix.program_id == vortex_hub::ID
        || ix.program_id == anchor_lang::solana_program::bpf_loader_upgradeable::ID
        || (token && ix.data.first() == Some(&TOKEN_SET_AUTHORITY))
        || (ix.program_id == crate::ID && !ix.data.starts_with(crate::instruction::AddAsset::DISCRIMINATOR))
}

/// One budget step instruction: [treasury (signer), governance, proposal
/// (writable), watched token accounts...].
fn budget_step(treasury: &Pubkey, governance: &Pubkey, proposal: &Pubkey, watched: &[Watched], discriminator: &[u8]) -> StoredInstruction {
    let mut accounts = vec![
        StoredAccountMeta { pubkey: *treasury, is_signer: true, is_writable: false },
        StoredAccountMeta { pubkey: *governance, is_signer: false, is_writable: false },
        StoredAccountMeta { pubkey: *proposal, is_signer: false, is_writable: true },
    ];
    for w in watched.iter().filter(|w| w.mint != SOL) {
        accounts.push(StoredAccountMeta { pubkey: w.account, is_signer: false, is_writable: false });
    }
    StoredInstruction { program_id: crate::ID, accounts, data: discriminator.to_vec() }
}

/// Serialized size of one budget step watching `assets` assets (SOL included).
fn budget_step_len(assets: usize) -> usize {
    32 + 4 + (3 + assets.saturating_sub(1)) * (32 + 1 + 1) + 4 + 8
}

/// A token account's state; `exists` is false if it isn't a token account.
fn token_state(info: &AccountInfo, token_program: &Pubkey) -> Result<AssetState> {
    if info.owner != token_program || info.data_is_empty() {
        return Ok(AssetState::default());
    }
    let t = TokenAccount::try_deserialize(&mut &info.try_borrow_data()?[..])?;
    Ok(AssetState { exists: true, balance: t.amount, owner: t.owner, delegate: t.delegate.into(), close_authority: t.close_authority.into() })
}

/// The state of watched asset `w`: the treasury's lamports for SOL, else
/// the next remaining account, which must be its token account.
fn watched_state<'a, 'info: 'a>(
    w: &Watched,
    treasury: &AccountInfo<'info>,
    rest: &mut impl Iterator<Item = &'a AccountInfo<'info>>,
) -> Result<AssetState> {
    if w.mint == SOL {
        return Ok(AssetState { exists: true, balance: treasury.lamports(), owner: *treasury.owner, delegate: None, close_authority: None });
    }
    let info = rest.next().ok_or(GovError::MissingProposalAccount)?;
    require_keys_eq!(info.key(), w.account, GovError::WrongDao);
    token_state(info, &w.token_program)
}

/// Today's holding of each listed asset; `accounts` are their accounts in
/// list order.
fn read_holdings(g: &Governance, accounts: &[AccountInfo]) -> Result<Vec<u64>> {
    require!(accounts.len() >= g.assets.len(), GovError::MissingProposalAccount);
    g.assets
        .iter()
        .zip(accounts)
        .map(|(a, info)| {
            require_keys_eq!(info.key(), a.account, GovError::WrongDao);
            if a.mint == SOL {
                Ok(info.lamports())
            } else {
                Ok(token_state(info, &a.token_program)?.balance)
            }
        })
        .collect()
}

/// Extra conviction a budget needs: for each asset, weight x amount /
/// holding (the full weight if the treasury holds none of it).
pub fn budget_cost(assets: &[ListedAsset], holdings: &[u64], budget: &[AssetAmount]) -> u64 {
    let mut cost: u128 = 0;
    for b in budget {
        if let Some(i) = assets.iter().position(|a| a.mint == b.mint) {
            let (w, h) = (assets[i].weight as u128, holdings[i] as u128);
            cost += if h == 0 { w } else { w * (b.amount as u128).min(h) / h };
        }
    }
    cost.min(u64::MAX as u128) as u64
}

// ---------------------------------------------------------------
// STATE
// ---------------------------------------------------------------

/// A DAO's rules. Times in seconds; amounts in raw tokens.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct ConvictionConfig {
    /// How far conviction can move per second, toward current support.
    pub growth_rate: u64,
    /// The lowest bar any proposal faces.
    pub min_conviction: u64,
    /// Share of all deposits (when proposed) a proposal's bar is set to,
    /// if that's above `min_conviction`.
    pub support_bps: u16,
    /// Deposit needed to propose.
    pub proposal_threshold: u64,
    pub timelock: u32,
    pub execution_period: u32,
}

impl ConvictionConfig {
    pub fn validate(&self) -> Result<()> {
        require!(self.growth_rate > 0, GovError::InvalidConfig);
        require!(self.support_bps <= MAX_BPS, GovError::InvalidConfig);
        require!(self.min_conviction > 0 || self.support_bps > 0, GovError::InvalidConfig);
        Ok(())
    }

    /// The bar for a proposal made while `total_deposited` is deposited.
    pub fn required_conviction(&self, total_deposited: u64) -> u64 {
        let share = (total_deposited as u128 * self.support_bps as u128 / MAX_BPS as u128) as u64;
        self.min_conviction.max(share).max(1)
    }

    /// Timelock and execution window, in vortex-core's shape.
    pub fn timing(&self) -> VotingConfig {
        VotingConfig {
            quorum_bps: 1,
            approval_bps: 1,
            voting_delay: 0,
            voting_period: 1,
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
    pub config: ConvictionConfig,
    pub proposal_count: u64,
    pub total_deposited: u64,
    pub bump: u8,
    /// Assets budgets protect; SOL first.
    #[max_len(MAX_ASSETS)]
    pub assets: Vec<ListedAsset>,
}

impl Governance {
    pub fn asset(&self, mint: &Pubkey) -> Option<&ListedAsset> {
        self.assets.iter().find(|a| a.mint == *mint)
    }

    pub fn asset_mut(&mut self, mint: &Pubkey) -> Option<&mut ListedAsset> {
        self.assets.iter_mut().find(|a| a.mint == *mint)
    }

    /// The bar of spending everything.
    pub fn total_weight(&self) -> u64 {
        self.assets.iter().fold(0u64, |t, a| t.saturating_add(a.weight))
    }
}

/// An asset on the list.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct ListedAsset {
    /// The token's mint, or SOL.
    pub mint: Pubkey,
    /// Token or Token-2022 (default for SOL).
    pub token_program: Pubkey,
    /// Where the treasury holds it: its associated token account (the
    /// treasury itself for SOL).
    pub account: Pubkey,
    /// Conviction added for spending all of the holding.
    pub weight: u64,
    pub pending: Option<PendingAssetChange>,
}

/// A weight cut or removal waiting out WEIGHT_CUT_DELAY.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct PendingAssetChange {
    pub weight: u64,
    pub remove: bool,
    pub effective_at: i64,
}

/// One line of a proposal's budget.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct AssetAmount {
    /// A listed mint, or SOL.
    pub mint: Pubkey,
    /// Most the treasury may lose of it (raw units; lamports for SOL).
    pub amount: u64,
}

/// A watched account's state.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace)]
pub struct AssetState {
    pub exists: bool,
    pub balance: u64,
    pub owner: Pubkey,
    pub delegate: Option<Pubkey>,
    pub close_authority: Option<Pubkey>,
}

/// An asset a proposal watches, and its state before the proposal ran.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Watched {
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub account: Pubkey,
    pub before: AssetState,
}

#[account]
#[derive(InitSpace)]
pub struct Voter {
    pub governance: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// The proposal this member backs (default = none).
    pub supporting: Pubkey,
    /// How much of `amount` backs it (locked).
    pub support_weight: u64,
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
    /// Fixed when proposed.
    pub required_conviction: u64,
    /// As of `last_update`; see [`Proposal::conviction_at`] for now.
    pub conviction: u64,
    /// Deposits backing it now.
    pub total_support: u64,
    pub last_update: i64,
    pub queued_at: i64,
    pub executed: bool,
    pub cancelled: bool,
    pub bump: u8,
    /// Most it may spend of each listed asset; anything else listed: 0.
    pub budget: Vec<AssetAmount>,
    /// Whether it faced the bar of spending everything.
    pub weakens_rules: bool,
    /// The assets listed when it was made, and their state when it ran.
    pub watched: Vec<Watched>,
    /// 0 before running, 1 once balances are recorded, 2 once checked.
    pub budget_stage: u8,
}

/// Where a conviction proposal is. Same states as the EVM contract.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvictionState {
    /// Gathering conviction.
    Active,
    Queued,
    Executed,
    Cancelled,
    Expired,
}

impl Proposal {
    /// Account size for `instructions` (before wrapping), a budget of
    /// `budget` lines, and `assets` listed assets.
    pub fn space(metadata_uri: &str, instructions: &[StoredInstruction], budget: usize, assets: usize) -> usize {
        8 + (32 + 4 + vortex_core::instructions_len(instructions) + BUDGET_STEPS * budget_step_len(assets))
            + 32 + 8 + 32
            + (4 + metadata_uri.len())
            + 8 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 1
            + (4 + AssetAmount::INIT_SPACE * budget)
            + 1
            + (4 + Watched::INIT_SPACE * assets)
            + 1
    }

    /// Conviction at `now`: moved from the last settled value toward
    /// `total_support` by at most `growth_rate` per second, never past it.
    pub fn conviction_at(&self, growth_rate: u64, now: i64) -> u64 {
        let elapsed = now.saturating_sub(self.last_update).max(0) as u128;
        let max_delta = (growth_rate as u128 * elapsed).min(u64::MAX as u128) as u64;
        let (c, target) = (self.conviction, self.total_support);
        if c < target {
            c + max_delta.min(target - c)
        } else {
            c - max_delta.min(c - target)
        }
    }

    /// Records conviction up to `now`. Must run before `total_support`
    /// changes, so each stretch of time counts at the support it really had.
    pub fn settle(&mut self, growth_rate: u64, now: i64) {
        self.conviction = self.conviction_at(growth_rate, now);
        self.last_update = now;
    }

    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle {
            voting_starts_at: self.created_at,
            voting_ends_at: self.created_at,
            queued_at: self.queued_at,
            executed: self.executed,
            cancelled: self.cancelled,
        }
    }

    /// State at `now` (what clients show).
    pub fn state(&self, config: &ConvictionConfig, now: i64) -> ConvictionState {
        if self.cancelled {
            ConvictionState::Cancelled
        } else if self.executed {
            ConvictionState::Executed
        } else if self.queued_at == 0 {
            ConvictionState::Active
        } else if execution_expired(&self.lifecycle(), &config.timing(), now) {
            ConvictionState::Expired
        } else {
            ConvictionState::Queued
        }
    }
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
#[instruction(id: u64, metadata_uri: String, instructions: Vec<StoredInstruction>, budget: Vec<AssetAmount>)]
pub struct Propose<'info> {
    #[account(mut)]
    pub proposer: Signer<'info>,
    #[account(mut, has_one = hub_dao @ GovError::WrongDao)]
    pub governance: Account<'info, Governance>,
    pub hub_dao: Account<'info, HubDao>,
    #[account(seeds = [VOTER_SEED, governance.key().as_ref(), proposer.key().as_ref()], bump)]
    pub voter: Option<Account<'info, Voter>>,
    #[account(
        init, payer = proposer, space = Proposal::space(&metadata_uri, &instructions, budget.len(), governance.assets.len()),
        seeds = [PROPOSAL_SEED, governance.key().as_ref(), &id.to_le_bytes()], bump,
    )]
    pub proposal: Account<'info, Proposal>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Support<'info> {
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
    /// The proposal the member backs now, if any.
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub previous: Option<Account<'info, Proposal>>,
}

#[derive(Accounts)]
pub struct WithdrawSupport<'info> {
    pub owner: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, seeds = [VOTER_SEED, governance.key().as_ref(), owner.key().as_ref()], bump = voter.bump)]
    pub voter: Account<'info, Voter>,
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

/// The budget steps; only the treasury signs, so only a proposal runs them.
#[derive(Accounts)]
pub struct BudgetStep<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    pub governance: Account<'info, Governance>,
    #[account(mut, has_one = governance @ GovError::WrongDao)]
    pub proposal: Account<'info, Proposal>,
}

/// Rule and asset-list changes: only a passed proposal (the treasury signs).
#[derive(Accounts)]
pub struct TreasuryOnly<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
}

#[derive(Accounts)]
pub struct AddAsset<'info> {
    #[account(address = governance.treasury @ GovError::Unauthorized)]
    pub treasury: Signer<'info>,
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    pub mint: InterfaceAccount<'info, Mint>,
}

#[derive(Accounts)]
pub struct ApplyAssetChange<'info> {
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
    pub budget: Vec<AssetAmount>,
    pub weakens_rules: bool,
    pub required_conviction: u64,
}

#[event]
pub struct Supported {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub supporter: Pubkey,
    pub weight: u64,
}

#[event]
pub struct SupportWithdrawn {
    pub hub_dao: Pubkey,
    pub proposal: Pubkey,
    pub id: u64,
    pub supporter: Pubkey,
    pub weight: u64,
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
pub struct AssetListed {
    pub hub_dao: Pubkey,
    pub mint: Pubkey,
    pub account: Pubkey,
    pub weight: u64,
}

#[event]
pub struct AssetWeightChanged {
    pub hub_dao: Pubkey,
    pub mint: Pubkey,
    pub from: u64,
    pub to: u64,
}

#[event]
pub struct AssetChangeScheduled {
    pub hub_dao: Pubkey,
    pub mint: Pubkey,
    pub weight: u64,
    pub remove: bool,
    pub effective_at: i64,
}

#[event]
pub struct AssetRemoved {
    pub hub_dao: Pubkey,
    pub mint: Pubkey,
}

#[event]
pub struct ConfigUpdated {
    pub hub_dao: Pubkey,
    pub config: ConvictionConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(conviction: u64, total_support: u64) -> Proposal {
        Proposal {
            core: ProposalCore { hub_dao: Pubkey::default(), epoch: 1, instructions: vec![] },
            governance: Pubkey::default(),
            id: 1,
            proposer: Pubkey::default(),
            metadata_uri: String::new(),
            created_at: 0,
            required_conviction: 0,
            conviction,
            total_support,
            last_update: 100,
            queued_at: 0,
            executed: false,
            cancelled: false,
            bump: 0,
            budget: vec![],
            weakens_rules: false,
            watched: vec![],
            budget_stage: 0,
        }
    }

    #[test]
    fn conviction_ramps_linearly_and_never_overshoots() {
        let p = proposal(0, 1_000);
        assert_eq!(p.conviction_at(10, 100), 0);
        assert_eq!(p.conviction_at(10, 150), 500);
        assert_eq!(p.conviction_at(10, 200), 1_000);
        assert_eq!(p.conviction_at(10, 10_000), 1_000, "capped at support");
        let p = proposal(1_000, 200);
        assert_eq!(p.conviction_at(10, 130), 700, "falls back when support leaves");
        assert_eq!(p.conviction_at(10, 10_000), 200);
        assert_eq!(p.conviction_at(u64::MAX, i64::MAX), 200, "no overflow");
        assert_eq!(p.conviction_at(10, 0), 1_000, "time going backwards changes nothing");
    }

    #[test]
    fn budget_cost_scales_with_the_holding() {
        let a = |mint: Pubkey, weight: u64| ListedAsset { mint, token_program: Pubkey::default(), account: Pubkey::default(), weight, pending: None };
        let usdc = Pubkey::new_unique();
        let assets = [a(SOL, 1_000), a(usdc, 500)];
        let spend = |mint: Pubkey, amount: u64| AssetAmount { mint, amount };
        assert_eq!(budget_cost(&assets, &[5_000, 100], &[]), 0);
        assert_eq!(budget_cost(&assets, &[5_000, 100], &[spend(SOL, 1_000)]), 200, "a fifth of the SOL");
        assert_eq!(budget_cost(&assets, &[5_000, 100], &[spend(SOL, 1_000), spend(usdc, 30)]), 350);
        assert_eq!(budget_cost(&assets, &[5_000, 100], &[spend(usdc, 1_000)]), 500, "capped at all of it");
        assert_eq!(budget_cost(&assets, &[5_000, 0], &[spend(usdc, 1)]), 500, "none held: full weight");
        let heavy = [a(SOL, u64::MAX), a(usdc, u64::MAX)];
        assert_eq!(budget_cost(&heavy, &[1, 1], &[spend(SOL, 1), spend(usdc, 1)]), u64::MAX, "saturates");
    }

    #[test]
    fn what_counts_as_loosening() {
        let ix = |program_id: Pubkey, data: Vec<u8>| StoredInstruction { program_id, accounts: vec![], data };
        let add = crate::instruction::AddAsset::DISCRIMINATOR.to_vec();
        let update = crate::instruction::UpdateConfig::DISCRIMINATOR.to_vec();
        assert!(!weakens_rules(&ix(crate::ID, add)));
        assert!(weakens_rules(&ix(crate::ID, update)));
        assert!(weakens_rules(&ix(vortex_hub::ID, vec![1])));
        assert!(weakens_rules(&ix(anchor_spl::token::ID, vec![TOKEN_SET_AUTHORITY])));
        assert!(weakens_rules(&ix(anchor_spl::token_2022::ID, vec![TOKEN_SET_AUTHORITY, 0])));
        assert!(!weakens_rules(&ix(anchor_spl::token::ID, vec![12])), "a transfer is budgeted, not loosening");
        assert!(weakens_rules(&ix(anchor_lang::solana_program::bpf_loader_upgradeable::ID, vec![4])));
        assert!(!weakens_rules(&ix(anchor_lang::system_program::ID, vec![2])));
    }

    #[test]
    fn bar_and_rules() {
        let c = ConvictionConfig { growth_rate: 1, min_conviction: 100, support_bps: 2_000, proposal_threshold: 0, timelock: 0, execution_period: 0 };
        assert_eq!(c.required_conviction(0), 100);
        assert_eq!(c.required_conviction(1_000), 200);
        assert_eq!(c.required_conviction(u64::MAX), u64::MAX / 5);
        assert_eq!(ConvictionConfig { min_conviction: 0, support_bps: 1, ..c }.required_conviction(0), 1, "never zero");
        assert!(c.validate().is_ok());
        assert!(ConvictionConfig { growth_rate: 0, ..c }.validate().is_err());
        assert!(ConvictionConfig { support_bps: 10_001, ..c }.validate().is_err());
        assert!(ConvictionConfig { min_conviction: 0, support_bps: 0, ..c }.validate().is_err());
    }
}
