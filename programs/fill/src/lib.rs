//! Vortexes fill: a DAO swaps through any route (Jupiter's included)
//! without calling the router from a proposal and without trusting whoever
//! runs the swap.
//!
//! A passed proposal opens an order ([`vortex_fill::open_order`]): the
//! treasury moves `amount` of one token into the order's input vault, with
//! the least it accepts of another (`min_out`) and a deadline. A keeper
//! then fills it in one transaction of its own, with whatever route, lookup
//! tables and size it likes:
//!
//! 1. [`vortex_fill::take`] hands the keeper the input. It only runs as a
//!    top-level instruction, and only if a [`vortex_fill::settle`] for the
//!    same order comes later in the transaction (read from the
//!    instructions sysvar), so it can't happen on its own;
//! 2. the keeper swaps, sending the output to the order's output vault;
//! 3. `settle` checks that vault gained at least `min_out`, sends all of it
//!    to the treasury and closes the order. Less, and the whole
//!    transaction (the take included) reverts.
//!
//! The output vault belongs to the order alone, so nothing else that pays
//! the treasury (another order's refund, a proposal executed in the same
//! transaction) can count toward this fill. Past the deadline anyone can
//! cancel an unfilled order, returning the input to the treasury; the
//! treasury can cancel it any time through a proposal.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};
use anchor_spl::token_interface::{self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked};
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};

declare_id!("9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77");

pub const ORDER_SEED: &[u8] = b"order";
pub const INPUT_SEED: &[u8] = b"input";
pub const OUTPUT_SEED: &[u8] = b"output";

#[program]
pub mod vortex_fill {
    use super::*;

    /// Puts `amount` of `mint_in` up for at least `min_out` of `mint_out`
    /// until `deadline` (unix seconds). The owner (a DAO's treasury, through
    /// a passed proposal) signs, pays the order's rent and gets it back when
    /// the order closes. `id` picks the order's address: any number the
    /// owner hasn't got an open order under.
    pub fn open_order(ctx: Context<OpenOrder>, id: u64, amount: u64, min_out: u64, deadline: i64) -> Result<()> {
        require!(amount > 0 && min_out > 0, FillError::InvalidAmount);
        require!(deadline > Clock::get()?.unix_timestamp, FillError::DeadlinePassed);
        require_keys_neq!(ctx.accounts.mint_in.key(), ctx.accounts.mint_out.key(), FillError::SameMint);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program_in.key(),
                TransferChecked {
                    from: ctx.accounts.source.to_account_info(),
                    mint: ctx.accounts.mint_in.to_account_info(),
                    to: ctx.accounts.input_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint_in.decimals,
        )?;
        // What arrived, after any transfer fee the mint charges.
        ctx.accounts.input_vault.reload()?;
        let order = &mut ctx.accounts.order;
        order.owner = ctx.accounts.owner.key();
        order.id = id;
        order.mint_in = ctx.accounts.mint_in.key();
        order.mint_out = ctx.accounts.mint_out.key();
        order.amount_in = ctx.accounts.input_vault.amount;
        order.min_out = min_out;
        order.deadline = deadline;
        order.refund = ctx.accounts.source.key();
        order.receive = ctx.accounts.receive.key();
        order.output_before = None;
        order.bump = ctx.bumps.order;
        order.input_bump = ctx.bumps.input_vault;
        order.output_bump = ctx.bumps.output_vault;
        emit!(OrderOpened {
            order: order.key(),
            owner: order.owner,
            id,
            mint_in: order.mint_in,
            amount_in: order.amount_in,
            mint_out: order.mint_out,
            min_out,
            deadline,
        });
        Ok(())
    }

    /// A keeper takes the order's input to swap. Top-level only, and the
    /// transaction must settle this order afterwards.
    pub fn take(ctx: Context<Take>) -> Result<()> {
        require!(get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT, FillError::NotTopLevel);
        let order_key = ctx.accounts.order.key();
        require_settle_follows(&ctx.accounts.instructions, &order_key)?;
        let order = &mut ctx.accounts.order;
        require!(order.output_before.is_none(), FillError::AlreadyTaken);
        require!(Clock::get()?.unix_timestamp <= order.deadline, FillError::OrderExpired);
        // Measured from here, so tokens sent to the vault earlier (by
        // anyone) go to the treasury but don't count toward this fill.
        order.output_before = Some(ctx.accounts.output_vault.amount);
        let amount = order.amount_in;
        let seeds: &[&[u8]] = &[ORDER_SEED, order.owner.as_ref(), &order.id.to_le_bytes(), &[order.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program_in.key(),
                TransferChecked {
                    from: ctx.accounts.input_vault.to_account_info(),
                    mint: ctx.accounts.mint_in.to_account_info(),
                    to: ctx.accounts.keeper_account.to_account_info(),
                    authority: order.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            ctx.accounts.mint_in.decimals,
        )?;
        Ok(())
    }

    /// Ends a fill: the output vault must have gained at least `min_out`
    /// since the take. Everything in it goes to the owner's account and the
    /// order closes. Anyone can call it; the keeper does, in the take's
    /// transaction.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let before = ctx.accounts.order.output_before.ok_or(FillError::NotTaken)?;
        let held = ctx.accounts.output_vault.amount;
        let received = held.checked_sub(before).ok_or(FillError::BelowMinimum)?;
        require!(received >= ctx.accounts.order.min_out, FillError::BelowMinimum);
        let a = &ctx.accounts;
        emit!(OrderFilled { order: a.order.key(), owner: a.order.owner, id: a.order.id, amount_in: a.order.amount_in, amount_out: received });
        close_order(
            &a.order,
            &a.owner,
            &a.input_vault,
            &a.mint_in,
            &a.token_program_in,
            &a.refund,
            &a.output_vault,
            &a.mint_out,
            &a.token_program_out,
            &a.receive,
        )
    }

    /// Calls off an order that hasn't filled: its input goes back to the
    /// owner's account it came from (and anything sent to its output vault
    /// to the owner's receiving account). The owner can cancel any time;
    /// past the deadline, anyone can.
    pub fn cancel(ctx: Context<Cancel>) -> Result<()> {
        let a = &ctx.accounts;
        require!(a.order.output_before.is_none(), FillError::AlreadyTaken);
        let by_owner = a.caller.key() == a.order.owner;
        require!(by_owner || Clock::get()?.unix_timestamp > a.order.deadline, FillError::NotExpired);
        emit!(OrderCancelled { order: a.order.key(), owner: a.order.owner, id: a.order.id, by_owner });
        close_order(
            &a.order,
            &a.owner,
            &a.input_vault,
            &a.mint_in,
            &a.token_program_in,
            &a.refund,
            &a.output_vault,
            &a.mint_out,
            &a.token_program_out,
            &a.receive,
        )
    }
}

/// Checks that the next instruction for this program after the current one
/// is a `settle` of `order`: nothing of ours in between, and no take
/// without its settle.
fn require_settle_follows(instructions: &UncheckedAccount, order: &Pubkey) -> Result<()> {
    let sysvar = instructions.to_account_info();
    let current = load_current_index_checked(&sysvar).map_err(|_| error!(FillError::SettleMissing))?;
    let mut index = current as usize + 1;
    loop {
        let ix = load_instruction_at_checked(index, &sysvar).map_err(|_| error!(FillError::SettleMissing))?;
        if ix.program_id == crate::ID {
            let is_settle = ix.data.get(..8) == Some(instruction::Settle::DISCRIMINATOR);
            let same_order = ix.accounts.first().is_some_and(|m| m.pubkey == *order);
            require!(is_settle && same_order, FillError::SettleMissing);
            return Ok(());
        }
        index += 1;
    }
}

/// Empties both vaults to the owner's accounts (input to `refund`, output
/// to `receive`) and closes them and the order, their rent going to the
/// owner.
#[allow(clippy::too_many_arguments)]
fn close_order<'info>(
    order: &Account<'info, Order>,
    owner: &SystemAccount<'info>,
    input_vault: &InterfaceAccount<'info, TokenAccount>,
    mint_in: &InterfaceAccount<'info, Mint>,
    token_program_in: &Interface<'info, TokenInterface>,
    refund: &InterfaceAccount<'info, TokenAccount>,
    output_vault: &InterfaceAccount<'info, TokenAccount>,
    mint_out: &InterfaceAccount<'info, Mint>,
    token_program_out: &Interface<'info, TokenInterface>,
    receive: &InterfaceAccount<'info, TokenAccount>,
) -> Result<()> {
    let seeds: &[&[u8]] = &[ORDER_SEED, order.owner.as_ref(), &order.id.to_le_bytes(), &[order.bump]];
    for (vault, mint, program, to) in [(input_vault, mint_in, token_program_in, refund), (output_vault, mint_out, token_program_out, receive)] {
        if vault.amount > 0 {
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    program.key(),
                    TransferChecked {
                        from: vault.to_account_info(),
                        mint: mint.to_account_info(),
                        to: to.to_account_info(),
                        authority: order.to_account_info(),
                    },
                    &[seeds],
                ),
                vault.amount,
                mint.decimals,
            )?;
        }
        token_interface::close_account(CpiContext::new_with_signer(
            program.key(),
            CloseAccount { account: vault.to_account_info(), destination: owner.to_account_info(), authority: order.to_account_info() },
            &[seeds],
        ))?;
    }
    order.close(owner.to_account_info())
}

/// An order's address: `[ORDER_SEED, owner, id]`.
pub fn order_address(owner: &Pubkey, id: u64) -> Pubkey {
    Pubkey::find_program_address(&[ORDER_SEED, owner.as_ref(), &id.to_le_bytes()], &crate::ID).0
}

/// An order's input vault: `[INPUT_SEED, order]`.
pub fn input_vault_address(order: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[INPUT_SEED, order.as_ref()], &crate::ID).0
}

/// An order's output vault: `[OUTPUT_SEED, order]`.
pub fn output_vault_address(order: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[OUTPUT_SEED, order.as_ref()], &crate::ID).0
}

// ---------------------------------------------------------------
// ACCOUNTS
// ---------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Order {
    pub owner: Pubkey,
    pub id: u64,
    pub mint_in: Pubkey,
    pub mint_out: Pubkey,
    /// What the input vault received (after any transfer fee).
    pub amount_in: u64,
    pub min_out: u64,
    pub deadline: i64,
    /// The owner's account the input returns to on a cancel.
    pub refund: Pubkey,
    /// The owner's account the output goes to.
    pub receive: Pubkey,
    /// The output vault's balance at the take; set only inside a fill
    /// transaction (it either settles or reverts).
    pub output_before: Option<u64>,
    pub bump: u8,
    pub input_bump: u8,
    pub output_bump: u8,
}

// ---------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// ---------------------------------------------------------------

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct OpenOrder<'info> {
    /// Owns the tokens, pays the rent.
    #[account(mut)]
    pub owner: Signer<'info>,
    pub mint_in: InterfaceAccount<'info, Mint>,
    pub mint_out: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint_in, token::authority = owner, token::token_program = token_program_in)]
    pub source: InterfaceAccount<'info, TokenAccount>,
    #[account(token::mint = mint_out, token::authority = owner, token::token_program = token_program_out)]
    pub receive: InterfaceAccount<'info, TokenAccount>,
    #[account(init, payer = owner, space = 8 + Order::INIT_SPACE, seeds = [ORDER_SEED, owner.key().as_ref(), &id.to_le_bytes()], bump)]
    pub order: Account<'info, Order>,
    #[account(
        init, payer = owner, seeds = [INPUT_SEED, order.key().as_ref()], bump,
        token::mint = mint_in, token::authority = order, token::token_program = token_program_in,
    )]
    pub input_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = owner, seeds = [OUTPUT_SEED, order.key().as_ref()], bump,
        token::mint = mint_out, token::authority = order, token::token_program = token_program_out,
    )]
    pub output_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program_in: Interface<'info, TokenInterface>,
    pub token_program_out: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Take<'info> {
    pub keeper: Signer<'info>,
    #[account(mut, has_one = mint_in @ FillError::WrongAccount)]
    pub order: Account<'info, Order>,
    #[account(mut, seeds = [INPUT_SEED, order.key().as_ref()], bump = order.input_bump)]
    pub input_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(seeds = [OUTPUT_SEED, order.key().as_ref()], bump = order.output_bump)]
    pub output_vault: InterfaceAccount<'info, TokenAccount>,
    pub mint_in: InterfaceAccount<'info, Mint>,
    /// Where the keeper takes the input, to swap it.
    #[account(mut, token::mint = mint_in, token::token_program = token_program_in)]
    pub keeper_account: InterfaceAccount<'info, TokenAccount>,
    pub token_program_in: Interface<'info, TokenInterface>,
    /// CHECK: the instructions sysvar (its loaders check the address).
    #[account(address = solana_instructions_sysvar::ID)]
    pub instructions: UncheckedAccount<'info>,
}

/// Settle and cancel close the same accounts. The order comes first: take
/// finds its settle by it.
#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(
        mut,
        has_one = owner @ FillError::WrongAccount, has_one = mint_in @ FillError::WrongAccount, has_one = mint_out @ FillError::WrongAccount,
        has_one = refund @ FillError::WrongAccount, has_one = receive @ FillError::WrongAccount,
    )]
    pub order: Account<'info, Order>,
    #[account(mut)]
    pub owner: SystemAccount<'info>,
    #[account(mut, seeds = [INPUT_SEED, order.key().as_ref()], bump = order.input_bump)]
    pub input_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [OUTPUT_SEED, order.key().as_ref()], bump = order.output_bump)]
    pub output_vault: InterfaceAccount<'info, TokenAccount>,
    pub mint_in: InterfaceAccount<'info, Mint>,
    pub mint_out: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub refund: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub receive: InterfaceAccount<'info, TokenAccount>,
    pub token_program_in: Interface<'info, TokenInterface>,
    pub token_program_out: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Cancel<'info> {
    /// The owner (any time) or anyone (past the deadline).
    pub caller: Signer<'info>,
    #[account(
        mut,
        has_one = owner @ FillError::WrongAccount, has_one = mint_in @ FillError::WrongAccount, has_one = mint_out @ FillError::WrongAccount,
        has_one = refund @ FillError::WrongAccount, has_one = receive @ FillError::WrongAccount,
    )]
    pub order: Account<'info, Order>,
    #[account(mut)]
    pub owner: SystemAccount<'info>,
    #[account(mut, seeds = [INPUT_SEED, order.key().as_ref()], bump = order.input_bump)]
    pub input_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [OUTPUT_SEED, order.key().as_ref()], bump = order.output_bump)]
    pub output_vault: InterfaceAccount<'info, TokenAccount>,
    pub mint_in: InterfaceAccount<'info, Mint>,
    pub mint_out: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub refund: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub receive: InterfaceAccount<'info, TokenAccount>,
    pub token_program_in: Interface<'info, TokenInterface>,
    pub token_program_out: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------
// EVENTS
// ---------------------------------------------------------------

#[event]
pub struct OrderOpened {
    pub order: Pubkey,
    pub owner: Pubkey,
    pub id: u64,
    pub mint_in: Pubkey,
    pub amount_in: u64,
    pub mint_out: Pubkey,
    pub min_out: u64,
    pub deadline: i64,
}

#[event]
pub struct OrderFilled {
    pub order: Pubkey,
    pub owner: Pubkey,
    pub id: u64,
    pub amount_in: u64,
    pub amount_out: u64,
}

#[event]
pub struct OrderCancelled {
    pub order: Pubkey,
    pub owner: Pubkey,
    pub id: u64,
    pub by_owner: bool,
}

// ---------------------------------------------------------------
// ERRORS
// ---------------------------------------------------------------

#[error_code]
pub enum FillError {
    #[msg("The amount and the minimum to receive must both be above 0")]
    InvalidAmount,
    #[msg("The deadline has already passed")]
    DeadlinePassed,
    #[msg("The order must swap one token for another")]
    SameMint,
    #[msg("Take only runs as a top-level instruction")]
    NotTopLevel,
    #[msg("A take must be followed by a settle of the same order, with no other instruction of this program in between")]
    SettleMissing,
    #[msg("The order has already been taken in this transaction")]
    AlreadyTaken,
    #[msg("The order is past its deadline")]
    OrderExpired,
    #[msg("The order hasn't been taken")]
    NotTaken,
    #[msg("The swap returned less than the order's minimum")]
    BelowMinimum,
    #[msg("Only the owner can cancel before the deadline")]
    NotExpired,
    #[msg("That account doesn't belong to this order")]
    WrongAccount,
}
