//! The fill program: a DAO's order opened by a passed proposal, filled by a
//! keeper's take / swap / settle transaction, or cancelled. The swap is
//! stood in for by the keeper paying the output vault itself, which is all
//! the program sees of any route. Build first with `anchor build`.

use vortex_fill::{self as fill, FillError, Order};
use vortex_tests::*;

const SELL: u64 = 100;
const MIN_OUT: u64 = 250;

fn ferr(e: FillError) -> u32 {
    u32::from(e)
}

struct Market {
    env: Env,
    alice: Member,
    /// The DAO's token: what the treasury sells, from `source`.
    source: Pubkey,
    /// The token it buys (USDC, say), into `receive`.
    usdc: Pubkey,
    receive: Pubkey,
    keeper: Keypair,
    /// The keeper's accounts: it takes the DAO's token into one and pays the "swap's" output from the other.
    keeper_in: Pubkey,
    keeper_usdc: Pubkey,
}

/// A token-weighted DAO whose treasury holds 1,000 of its token, and a
/// keeper with 10,000 USDC to stand in for a swap's output.
fn market() -> Market {
    let (mut env, alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let source = env.treasury_ata();
    MintTo::new(&mut env.svm, &creator, &env.mint, &source, 1_000).send().unwrap();
    let usdc = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let receive = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &usdc).owner(&env.treasury).send().unwrap();
    let keeper = Keypair::new();
    env.svm.airdrop(&keeper.pubkey(), 1_000_000_000).unwrap();
    let keeper_in = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &env.mint).owner(&keeper.pubkey()).send().unwrap();
    let keeper_usdc = CreateAssociatedTokenAccount::new(&mut env.svm, &creator, &usdc).owner(&keeper.pubkey()).send().unwrap();
    MintTo::new(&mut env.svm, &creator, &usdc, &keeper_usdc, 10_000).send().unwrap();
    Market { env, alice, source, usdc, receive, keeper, keeper_in, keeper_usdc }
}

impl Market {
    fn order(&self, id: u64) -> Pubkey {
        fill::order_address(&self.env.treasury, id)
    }

    fn open_ix(&self, id: u64, amount: u64, min_out: u64, deadline: i64) -> StoredInstruction {
        let order = self.order(id);
        stored(Instruction {
            program_id: FILL,
            accounts: fill::accounts::OpenOrder {
                owner: self.env.treasury,
                mint_in: self.env.mint,
                mint_out: self.usdc,
                source: self.source,
                receive: self.receive,
                order,
                input_vault: fill::input_vault_address(&order),
                output_vault: fill::output_vault_address(&order),
                token_program_in: litesvm_token::TOKEN_ID,
                token_program_out: litesvm_token::TOKEN_ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: fill::instruction::OpenOrder { id, amount, min_out, deadline }.data(),
        })
    }

    /// Passes and runs a proposal opening order `id`: SELL of the DAO's token for at least MIN_OUT USDC, for a day.
    fn open(&mut self, id: u64) -> Pubkey {
        let deadline = self.env.now() + 10_000;
        let p = self.env.pass(TW, &self.alice, vec![self.open_ix(id, SELL, MIN_OUT, deadline + 1_000)]);
        self.env.execute(TW, p).unwrap();
        self.order(id)
    }

    fn take_ix(&self, order: Pubkey) -> Instruction {
        Instruction {
            program_id: FILL,
            accounts: fill::accounts::Take {
                keeper: self.keeper.pubkey(),
                order,
                input_vault: fill::input_vault_address(&order),
                output_vault: fill::output_vault_address(&order),
                mint_in: self.env.mint,
                keeper_account: self.keeper_in,
                token_program_in: litesvm_token::TOKEN_ID,
                instructions: solana_instructions_sysvar::ID,
            }
            .to_account_metas(None),
            data: fill::instruction::Take {}.data(),
        }
    }

    fn settle_ix(&self, order: Pubkey) -> Instruction {
        Instruction {
            program_id: FILL,
            accounts: fill::accounts::Settle {
                order,
                owner: self.env.treasury,
                input_vault: fill::input_vault_address(&order),
                output_vault: fill::output_vault_address(&order),
                mint_in: self.env.mint,
                mint_out: self.usdc,
                refund: self.source,
                receive: self.receive,
                token_program_in: litesvm_token::TOKEN_ID,
                token_program_out: litesvm_token::TOKEN_ID,
            }
            .to_account_metas(None),
            data: fill::instruction::Settle {}.data(),
        }
    }

    fn cancel_ix(&self, caller: &Pubkey, order: Pubkey) -> Instruction {
        Instruction {
            program_id: FILL,
            accounts: fill::accounts::Cancel {
                caller: *caller,
                order,
                owner: self.env.treasury,
                input_vault: fill::input_vault_address(&order),
                output_vault: fill::output_vault_address(&order),
                mint_in: self.env.mint,
                mint_out: self.usdc,
                refund: self.source,
                receive: self.receive,
                token_program_in: litesvm_token::TOKEN_ID,
                token_program_out: litesvm_token::TOKEN_ID,
            }
            .to_account_metas(None),
            data: fill::instruction::Cancel {}.data(),
        }
    }

    /// The stand-in for a swap: the keeper sends `amount` USDC to the order's output vault.
    fn swap_ix(&self, order: Pubkey, amount: u64) -> Instruction {
        spl_token_interface::instruction::transfer(&litesvm_token::TOKEN_ID, &self.keeper_usdc, &fill::output_vault_address(&order), &self.keeper.pubkey(), &[], amount).unwrap()
    }

    fn keeper_sends(&mut self, ixs: &[Instruction]) -> Result<(), FailedTransactionMetadata> {
        let keeper = self.keeper.insecure_clone();
        self.env.send(ixs, &[&keeper])
    }

    fn fill(&mut self, order: Pubkey, out: u64) -> Result<(), FailedTransactionMetadata> {
        let ixs = [self.take_ix(order), self.swap_ix(order, out), self.settle_ix(order)];
        self.keeper_sends(&ixs)
    }

    fn order_account(&self, order: Pubkey) -> Order {
        Order::try_deserialize(&mut self.env.svm.get_account(&order).unwrap().data.as_slice()).unwrap()
    }

    fn exists(&self, account: &Pubkey) -> bool {
        self.env.svm.get_account(account).is_some_and(|a| a.lamports > 0)
    }
}

#[test]
fn a_proposal_opens_an_order_and_a_keeper_fills_it() {
    let mut m = market();
    let lamports_before = m.env.lamports(&m.env.treasury);
    let order = m.open(1);
    let o = m.order_account(order);
    assert_eq!((o.owner, o.amount_in, o.min_out, o.receive, o.refund), (m.env.treasury, SELL, MIN_OUT, m.receive, m.source));
    assert_eq!(m.env.token_balance(&m.source), 900);
    assert_eq!(m.env.token_balance(&fill::input_vault_address(&order)), SELL);
    assert!(m.env.lamports(&m.env.treasury) < lamports_before, "the treasury pays the order's rent");

    m.fill(order, 260).unwrap();
    assert_eq!(m.env.token_balance(&m.receive), 260, "all the output, not just the minimum");
    assert_eq!(m.env.token_balance(&m.keeper_in), SELL);
    assert!(!m.exists(&order) && !m.exists(&fill::input_vault_address(&order)) && !m.exists(&fill::output_vault_address(&order)));
    assert_eq!(m.env.lamports(&m.env.treasury), lamports_before, "rent back when it closes");
}

#[test]
fn a_fill_below_the_minimum_reverts_entirely() {
    let mut m = market();
    let order = m.open(1);
    assert_eq!(code(m.fill(order, MIN_OUT - 1)), ferr(FillError::BelowMinimum));
    assert_eq!(m.env.token_balance(&m.keeper_in), 0, "the take reverted with it");
    assert_eq!(m.env.token_balance(&fill::input_vault_address(&order)), SELL);
    assert!(m.order_account(order).output_before.is_none());
    m.fill(order, MIN_OUT).unwrap();
    assert_eq!(m.env.token_balance(&m.receive), MIN_OUT);
}

#[test]
fn a_take_needs_its_own_settle_next() {
    let mut m = market();
    let a = m.open(1);
    let b = m.open(2);
    let take_a = m.take_ix(a);
    assert_eq!(code(m.keeper_sends(&[take_a.clone()])), ferr(FillError::SettleMissing), "no settle");
    let (swap, settle_b) = (m.swap_ix(a, 300), m.settle_ix(b));
    assert_eq!(code(m.keeper_sends(&[take_a.clone(), swap.clone(), settle_b])), ferr(FillError::SettleMissing), "another order's settle");
    // Two takes sharing one swap: each take must reach its own settle first.
    let (take_b, settle_a, settle_b) = (m.take_ix(b), m.settle_ix(a), m.settle_ix(b));
    assert_eq!(code(m.keeper_sends(&[take_a, take_b, swap, settle_a, settle_b])), ferr(FillError::SettleMissing));
    // A settle alone does nothing.
    let settle_a = m.settle_ix(a);
    assert_eq!(code(m.keeper_sends(&[settle_a])), ferr(FillError::NotTaken));
    assert_eq!(m.env.token_balance(&m.source), 800, "both orders still hold their input");
}

#[test]
fn only_output_after_the_take_counts() {
    let mut m = market();
    let order = m.open(1);
    // Sent to the output vault ahead of the fill: the treasury gets it, but it isn't the swap's output.
    let early = m.swap_ix(order, 1_000);
    m.keeper_sends(&[early]).unwrap();
    assert_eq!(code(m.fill(order, 10)), ferr(FillError::BelowMinimum));
    m.fill(order, MIN_OUT).unwrap();
    assert_eq!(m.env.token_balance(&m.receive), 1_000 + MIN_OUT);
}

#[test]
fn expired_orders_refuse_takes_and_anyone_can_cancel_them() {
    let mut m = market();
    let order = m.open(1);
    let deadline = m.order_account(order).deadline;
    let stranger = m.keeper.pubkey();
    let cancel = m.cancel_ix(&stranger, order);
    assert_eq!(code(m.keeper_sends(&[cancel.clone()])), ferr(FillError::NotExpired), "only the owner, before the deadline");

    m.env.warp(deadline + 1);
    assert_eq!(code(m.fill(order, 1_000)), ferr(FillError::OrderExpired));
    let donation = m.swap_ix(order, 7);
    m.keeper_sends(&[donation, cancel]).unwrap();
    assert_eq!(m.env.token_balance(&m.source), 1_000, "the input went back");
    assert_eq!(m.env.token_balance(&m.receive), 7, "and whatever reached the output vault");
    assert!(!m.exists(&order));
}

#[test]
fn the_treasury_cancels_through_a_proposal_any_time() {
    let mut m = market();
    let order = m.open(1);
    let treasury = m.env.treasury;
    let cancel = stored(m.cancel_ix(&treasury, order));
    let p = m.env.pass(TW, &m.alice, vec![cancel]);
    m.env.execute(TW, p).unwrap();
    assert!(!m.exists(&order));
    assert_eq!(m.env.token_balance(&m.source), 1_000);
}

#[test]
fn orders_are_checked_when_opened() {
    let mut m = market();
    let now = m.env.now();
    let p = m.env.pass(TW, &m.alice, vec![m.open_ix(1, SELL, 0, now + 100_000)]);
    assert_eq!(code(m.env.execute(TW, p)), ferr(FillError::InvalidAmount));
    let p = m.env.pass(TW, &m.alice, vec![m.open_ix(2, SELL, MIN_OUT, now + 1)]);
    assert_eq!(code(m.env.execute(TW, p)), ferr(FillError::DeadlinePassed), "passing took longer than that");
}
