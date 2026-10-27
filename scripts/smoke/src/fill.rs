//! Fill orders on a live cluster (`vortex-smoke fill`): a board DAO's
//! proposal opens an order selling 100 of its token for at least 250 of a
//! second token, and this wallet fills it as a keeper would, in one
//! transaction: take, then a stand-in for the swap (paying the order's
//! output vault itself, which is all the program sees of any route), then
//! settle. Before that, a take without its settle and a short fill must
//! fail. A second order runs out its deadline and is cancelled by this
//! wallet, returning its tokens to the treasury.

use super::*;
use vortex_fill::{self as fill, FillError};

const SELL: u64 = 100;
const MIN_OUT: u64 = 250;
const TOKEN: Pubkey = spl_token_interface::ID;

/// The order's accounts, for the instructions after it's open.
struct Order {
    key: Pubkey,
    input_vault: Pubkey,
    output_vault: Pubkey,
}

impl Order {
    fn new(owner: &Pubkey, id: u64) -> Self {
        let key = fill::order_address(owner, id);
        Order { key, input_vault: fill::input_vault_address(&key), output_vault: fill::output_vault_address(&key) }
    }
}

/// The DAO's side and the keeper's (this wallet's) accounts.
struct Market {
    dao: Dao,
    usdc: Pubkey,
    source: Pubkey,
    receive: Pubkey,
    keeper_in: Pubkey,
    keeper_usdc: Pubkey,
}

impl Smoke {
    pub(crate) fn fill(&mut self) {
        let result = self.fill_flow();
        self.record("fill orders", result);
    }

    fn fill_flow(&mut self) -> Result<(), String> {
        if !self.is_deployed(&fill::ID) {
            return Err(format!("vortex-fill isn't deployed at {} on this cluster", fill::ID));
        }
        let deployed = self.deployed_models();
        if !deployed.iter().any(|(id, _)| *id == BOARD) {
            return Err("the board model isn't deployed".into());
        }
        self.setup_hub(&deployed);
        let mint = self.create_mint();
        let alice_bob = (self.member(mint), self.member(mint));
        let m = self.fill_market(mint, &alice_bob.0, &alice_bob.1)?;

        // 1. A passed proposal opens order 1.
        println!("== order 1: opened by a proposal, filled by this wallet");
        let o = Order::new(&m.dao.treasury, 1);
        let deadline = self.now() + 3_600;
        self.board_pass(&m.dao, &alice_bob, vec![open_ix(&m, &o, 1, deadline)])?;
        expect(self.token_balance(&m.source) == 900 && self.token_balance(&o.input_vault) == SELL, "the order should hold 100 and the treasury 900")?;
        println!("  order {} holds {SELL}", o.key);

        // 2. What must fail.
        self.expect_error(&[take_ix(&m, &o, &self.payer.pubkey())], FillError::SettleMissing, "a take without its settle")?;
        let short = [take_ix(&m, &o, &self.payer.pubkey()), swap_ix(&m, &o, &self.payer.pubkey(), MIN_OUT - 1), settle_ix(&m, &o)];
        self.expect_error(&short, FillError::BelowMinimum, "a fill below the minimum")?;

        // 3. The fill.
        let lamports_before = self.balance(&m.dao.treasury);
        let ixs = [take_ix(&m, &o, &self.payer.pubkey()), swap_ix(&m, &o, &self.payer.pubkey(), 260), settle_ix(&m, &o)];
        let sig = self.send(&ixs, &[]).map_err(|e| format!("fill: {e}"))?;
        println!("  filled ({sig})");
        expect(self.token_balance(&m.receive) == 260, "the treasury should have received all 260")?;
        expect(self.token_balance(&m.keeper_in) == SELL, "the keeper should hold the 100 it took")?;
        expect(self.rpc.get_account(&o.key).is_err(), "the order should be closed")?;
        println!("  treasury received 260 (minimum {MIN_OUT}); order closed, {} lamports of rent back", self.balance(&m.dao.treasury) - lamports_before);

        // 4. Order 2 expires and anyone cancels it.
        println!("== order 2: expires, cancelled by this wallet");
        let o2 = Order::new(&m.dao.treasury, 2);
        let deadline = self.now() + 60;
        self.board_pass(&m.dao, &alice_bob, vec![open_ix(&m, &o2, 2, deadline)])?;
        self.expect_error(&[cancel_ix(&m, &o2, &self.payer.pubkey())], FillError::NotExpired, "a stranger's cancel before the deadline")?;
        self.wait_until(deadline + 2);
        let fill2 = [take_ix(&m, &o2, &self.payer.pubkey()), swap_ix(&m, &o2, &self.payer.pubkey(), 1_000), settle_ix(&m, &o2)];
        self.expect_error(&fill2, FillError::OrderExpired, "a fill past the deadline")?;
        self.send_ix(cancel_ix(&m, &o2, &self.payer.pubkey()), &[], "cancel")?;
        expect(self.token_balance(&m.source) == 900, "the cancelled order's 100 should be back in the treasury")?;
        expect(self.rpc.get_account(&o2.key).is_err(), "the cancelled order should be closed")?;
        println!("  the treasury has its 100 back");
        Ok(())
    }

    /// A board DAO (Alice and Bob, 2 of 2) whose treasury holds 1,000 of its
    /// token and a token account for a second one ("USDC"); this wallet,
    /// the keeper, holds 10,000 of the second to pay the stand-in swaps.
    fn fill_market(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<Market, String> {
        use spl_associated_token_account_interface::{address::get_associated_token_address, instruction::create_associated_token_account};
        let usdc = self.create_mint();
        let signers = vec![alice.kp.pubkey(), bob.kp.pubkey()];
        let dao = self.create_dao(mint, BOARD, "Smoke fill", |d, me| ix_init_board(d, me, signers, board_config()))?;
        let me = self.payer.pubkey();
        let source = get_associated_token_address(&dao.treasury, &mint);
        let receive = get_associated_token_address(&dao.treasury, &usdc);
        let keeper_in = get_associated_token_address(&me, &mint);
        let keeper_usdc = get_associated_token_address(&me, &usdc);
        let ixs = [
            create_associated_token_account(&me, &dao.treasury, &mint, &TOKEN),
            create_associated_token_account(&me, &dao.treasury, &usdc, &TOKEN),
            create_associated_token_account(&me, &me, &mint, &TOKEN),
            create_associated_token_account(&me, &me, &usdc, &TOKEN),
            spl_token_interface::instruction::mint_to(&TOKEN, &mint, &source, &me, &[], 1_000).unwrap(),
            spl_token_interface::instruction::mint_to(&TOKEN, &usdc, &keeper_usdc, &me, &[], 10_000).unwrap(),
        ];
        let sig = self.send(&ixs, &[]).map_err(|e| format!("token accounts: {e}"))?;
        println!("  treasury holds 1000 of {mint}; keeper holds 10000 of {usdc} ({sig})");
        Ok(Market { dao, usdc, source, receive, keeper_in, keeper_usdc })
    }

    /// Alice proposes, Bob confirms, the timelock passes, and the hub runs it.
    fn board_pass(&mut self, dao: &Dao, alice_bob: &(Member, Member), instructions: Vec<StoredInstruction>) -> Result<(), String> {
        let p = self.board_propose(dao, &alice_bob.0.kp, instructions)?;
        self.board_confirm(dao, &alice_bob.1.kp, p)?;
        self.wait_until(self.now() + board_config().timelock as i64 + 1);
        self.execute(dao, BOARD, p)
    }

    /// Sends `ixs` and requires them to fail with `error` (in simulation, so nothing lands).
    fn expect_error(&self, ixs: &[Instruction], error: FillError, what: &str) -> Result<(), String> {
        let code = format!("custom program error: {:#x}", u32::from(error));
        match self.send(ixs, &[]) {
            Ok(sig) => Err(format!("{what} went through ({sig})")),
            Err(e) if e.contains(&code) => {
                println!("  refused as expected: {what} ({error:?})");
                Ok(())
            }
            Err(e) => Err(format!("{what} failed, but not with {error:?}: {e}")),
        }
    }
}

fn board_config() -> vortex_board::BoardConfig {
    vortex_board::BoardConfig { required_approvals: 2, timelock: 5, execution_period: 3_600 }
}

fn expect(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.to_string())
    }
}

fn open_ix(m: &Market, o: &Order, id: u64, deadline: i64) -> StoredInstruction {
    stored(Instruction {
        program_id: fill::ID,
        accounts: fill::accounts::OpenOrder {
            owner: m.dao.treasury,
            mint_in: m.dao.mint,
            mint_out: m.usdc,
            source: m.source,
            receive: m.receive,
            order: o.key,
            input_vault: o.input_vault,
            output_vault: o.output_vault,
            token_program_in: TOKEN,
            token_program_out: TOKEN,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: fill::instruction::OpenOrder { id, amount: SELL, min_out: MIN_OUT, deadline }.data(),
    })
}

fn take_ix(m: &Market, o: &Order, keeper: &Pubkey) -> Instruction {
    Instruction {
        program_id: fill::ID,
        accounts: fill::accounts::Take {
            keeper: *keeper,
            order: o.key,
            input_vault: o.input_vault,
            output_vault: o.output_vault,
            mint_in: m.dao.mint,
            keeper_account: m.keeper_in,
            token_program_in: TOKEN,
            instructions: solana_sdk_ids::sysvar::instructions::ID,
        }
        .to_account_metas(None),
        data: fill::instruction::Take {}.data(),
    }
}

/// The stand-in for a swap: the keeper pays `amount` of the second token into the order's output vault.
fn swap_ix(m: &Market, o: &Order, keeper: &Pubkey, amount: u64) -> Instruction {
    spl_token_interface::instruction::transfer(&TOKEN, &m.keeper_usdc, &o.output_vault, keeper, &[], amount).unwrap()
}

fn settle_ix(m: &Market, o: &Order) -> Instruction {
    Instruction {
        program_id: fill::ID,
        accounts: fill::accounts::Settle {
            order: o.key,
            owner: m.dao.treasury,
            input_vault: o.input_vault,
            output_vault: o.output_vault,
            mint_in: m.dao.mint,
            mint_out: m.usdc,
            refund: m.source,
            receive: m.receive,
            token_program_in: TOKEN,
            token_program_out: TOKEN,
        }
        .to_account_metas(None),
        data: fill::instruction::Settle {}.data(),
    }
}

fn cancel_ix(m: &Market, o: &Order, caller: &Pubkey) -> Instruction {
    Instruction {
        program_id: fill::ID,
        accounts: fill::accounts::Cancel {
            caller: *caller,
            order: o.key,
            owner: m.dao.treasury,
            input_vault: o.input_vault,
            output_vault: o.output_vault,
            mint_in: m.dao.mint,
            mint_out: m.usdc,
            refund: m.source,
            receive: m.receive,
            token_program_in: TOKEN,
            token_program_out: TOKEN,
        }
        .to_account_metas(None),
        data: fill::instruction::Cancel {}.data(),
    }
}
