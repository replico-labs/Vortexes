//! Decision markets: split/merge, trading in the pass and fail pools,
//! TWAP settlement, execution through the hub, redemption and reclaiming
//! liquidity - with every token accounted for.

use vortex_decision_markets as dm;
use vortex_decision_markets::{MarketsConfig, Market, Side};
use vortex_tests::*;

fn dm_config() -> MarketsConfig {
    MarketsConfig { trading_period: 100, threshold_bps: 500, timelock: 50, execution_period: 200, min_base_seed: 100, min_quote_seed: 100 }
}

struct Trader {
    kp: Keypair,
    base: Pubkey,
    quote: Pubkey,
}

struct Dm {
    env: Env,
    quote_mint: Pubkey,
}

fn quote_vault(env: &Env) -> Pubkey {
    Pubkey::find_program_address(&[dm::QUOTE_VAULT_SEED, env.governance(DM).as_ref()], &DM).0
}

/// One base vault per token the DAO has used.
fn base_vault(env: &Env, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[dm::BASE_VAULT_SEED, env.governance(DM).as_ref(), mint.as_ref()], &DM).0
}

fn setup() -> Dm {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.insecure_clone();
    let quote_mint = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
    let c = creator.pubkey();
    env.create_dao("Futarchy", DM, |env| Instruction {
        program_id: DM,
        accounts: dm::accounts::InitGovernance {
            authority: c,
            payer: c,
            hub_dao: env.dao,
            governance: env.governance(DM),
            base_mint: env.mint,
            dao_token: hub::dao_token_address(&env.dao),
            quote_mint,
            base_vault: base_vault(env, &env.mint),
            quote_vault: quote_vault(env),
            base_token_program: env.token_program,
            quote_token_program: env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dm::instruction::InitGovernance { epoch: env.governance_epoch(DM), config: dm_config() }.data(),
    });
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    Dm { env, quote_mint }
}

impl Dm {
    fn trader(&mut self, base: u64, quote: u64) -> Trader {
        let m = self.env.member(base);
        let creator = self.env.creator.insecure_clone();
        let quote_ata = CreateAssociatedTokenAccount::new(&mut self.env.svm, &creator, &self.quote_mint).owner(&m.kp.pubkey()).send().unwrap();
        if quote > 0 {
            MintTo::new(&mut self.env.svm, &creator, &self.quote_mint, &quote_ata, quote).send().unwrap();
        }
        Trader { kp: m.kp, base: m.ata, quote: quote_ata }
    }

    fn tokens(&self) -> dm::accounts::Tokens {
        dm::accounts::Tokens {
            governance: self.env.governance(DM),
            base_vault: base_vault(&self.env, &self.env.mint),
            quote_vault: quote_vault(&self.env),
            base_mint: self.env.mint,
            quote_mint: self.quote_mint,
            base_token_program: self.env.token_program,
            quote_token_program: self.env.token_program,
        }
    }

    fn send(&mut self, who: &Keypair, mut accounts: Vec<AccountMeta>, extra: Vec<AccountMeta>, data: Vec<u8>) -> Result<(), FailedTransactionMetadata> {
        accounts.extend(extra);
        self.env.send(&[Instruction { program_id: DM, accounts, data }], &[who])
    }

    fn proposal(&self, p: Pubkey) -> dm::Proposal {
        dm::Proposal::try_deserialize(&mut self.env.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    fn position(&self, p: Pubkey, owner: &Pubkey) -> dm::Position {
        dm::Position::try_deserialize(&mut self.env.svm.get_account(&self.position_pda(p, owner)).unwrap().data.as_slice()).unwrap()
    }

    fn position_pda(&self, p: Pubkey, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[dm::POSITION_SEED, p.as_ref(), owner.as_ref()], &DM).0
    }

    fn propose(&mut self, t: &Trader, base_seed: u64, quote_seed: u64) -> Result<Pubkey, FailedTransactionMetadata> {
        let g = dm::Governance::try_deserialize(&mut self.env.svm.get_account(&self.env.governance(DM)).unwrap().data.as_slice()).unwrap();
        let id = g.proposal_count + 1;
        let p = self.env.proposal_pda(DM, id);
        let a = dm::accounts::Propose {
            proposer: t.kp.pubkey(),
            governance: self.env.governance(DM),
            hub_dao: self.env.dao,
            dao_token: hub::dao_token_address(&self.env.dao),
            proposal: p,
            proposer_base: t.base,
            proposer_quote: t.quote,
            tokens: self.tokens(),
            system_program: SYSTEM,
        };
        let data = dm::instruction::Propose { id, metadata_uri: "List on a new exchange".into(), instructions: vec![self.env.pay_sol(&t.kp.pubkey(), 1_000)], base_seed, quote_seed }.data();
        self.send(&t.kp, a.to_account_metas(None), vec![], data).map(|_| p)
    }

    fn split(&mut self, t: &Trader, p: Pubkey, base: u64, quote: u64) -> Result<(), FailedTransactionMetadata> {
        let a = dm::accounts::Split {
            owner: t.kp.pubkey(),
            proposal: p,
            position: self.position_pda(p, &t.kp.pubkey()),
            owner_base: t.base,
            owner_quote: t.quote,
            tokens: self.tokens(),
            system_program: SYSTEM,
        };
        self.send(&t.kp, a.to_account_metas(None), vec![], dm::instruction::Split { base, quote }.data())
    }

    fn redeem_accounts(&self, t: &Trader, p: Pubkey) -> Vec<AccountMeta> {
        dm::accounts::Redeem {
            owner: t.kp.pubkey(),
            governance: self.env.governance(DM),
            proposal: p,
            position: self.position_pda(p, &t.kp.pubkey()),
            owner_base: t.base,
            owner_quote: t.quote,
            tokens: self.tokens(),
        }
        .to_account_metas(None)
    }

    fn merge(&mut self, t: &Trader, p: Pubkey, base: u64, quote: u64) -> Result<(), FailedTransactionMetadata> {
        let a = self.redeem_accounts(t, p);
        self.send(&t.kp, a, vec![], dm::instruction::Merge { base, quote }.data())
    }

    fn redeem(&mut self, t: &Trader, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let a = self.redeem_accounts(t, p);
        self.send(&t.kp, a, vec![], dm::instruction::Redeem {}.data())
    }

    fn trade(&mut self, t: &Trader, p: Pubkey, market: Market, side_in: Side, amount_in: u64, min_out: u64) -> Result<(), FailedTransactionMetadata> {
        let a = dm::accounts::Trade { owner: t.kp.pubkey(), proposal: p, position: self.position_pda(p, &t.kp.pubkey()) };
        self.send(&t.kp, a.to_account_metas(None), vec![], dm::instruction::Trade { market, side_in, amount_in, min_out }.data())
    }

    fn finalize(&mut self, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let admin = self.env.admin.insecure_clone();
        let a = dm::accounts::Finalize { governance: self.env.governance(DM), proposal: p };
        self.send(&admin, a.to_account_metas(None), vec![], dm::instruction::Finalize {}.data())
    }

    fn reclaim(&mut self, t: &Trader, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let a = dm::accounts::ReclaimLiquidity {
            proposer: t.kp.pubkey(),
            governance: self.env.governance(DM),
            proposal: p,
            proposer_base: t.base,
            proposer_quote: t.quote,
            tokens: self.tokens(),
        };
        self.send(&t.kp, a.to_account_metas(None), vec![], dm::instruction::ReclaimLiquidity {}.data())
    }

    fn vaults(&self) -> (u64, u64) {
        (self.env.token_balance(&base_vault(&self.env, &self.env.mint)), self.env.token_balance(&quote_vault(&self.env)))
    }
}

#[test]
fn a_proposal_the_pass_market_prices_higher_runs_and_everything_settles() {
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let bob = d.trader(0, 1_000);
    let p = d.propose(&alice, 1_000, 1_000).unwrap();
    assert_eq!(d.vaults(), (1_000, 1_000));
    // Bob thinks it passes: buys pass base with pass quote.
    d.split(&bob, p, 0, 600).unwrap();
    assert_eq!(code(d.trade(&bob, p, Market::Pass, Side::Quote, 601, 0)), err(GovError::InsufficientConditionalBalance));
    assert_eq!(code(d.trade(&bob, p, Market::Pass, Side::Quote, 500, 400)), err(GovError::SlippageExceeded));
    d.trade(&bob, p, Market::Pass, Side::Quote, 500, 300).unwrap();
    let pos = d.position(p, &bob.kp.pubkey());
    assert_eq!((pos.pass_quote, pos.fail_quote), (100, 600));
    assert!(pos.pass_base > 300 && pos.pass_base < 334, "≈ 332 base for 500 quote at 1:1 with a 0.3% fee");
    let pass_base = pos.pass_base;

    assert_eq!(code(d.finalize(p)), err(GovError::TradingOpen));
    d.env.warp(START + 100);
    assert_eq!(code(d.trade(&bob, p, Market::Pass, Side::Quote, 10, 0)), err(GovError::TradingClosed));
    assert_eq!(code(d.split(&bob, p, 0, 10)), err(GovError::TradingClosed));
    d.finalize(p).unwrap();
    let pr = d.proposal(p);
    assert!(pr.passed && pr.pass_twap > pr.fail_twap, "pass {} vs fail {}", pr.pass_twap, pr.fail_twap);
    assert_eq!(code(d.finalize(p)), err(GovError::AlreadyFinalized));

    assert_eq!(code(d.env.execute(DM, p)), err(GovError::ProposalNotExecutable), "timelock");
    d.env.warp(START + 150);
    let before = d.env.lamports(&alice.kp.pubkey());
    d.env.execute(DM, p).unwrap();
    assert_eq!(d.env.lamports(&alice.kp.pubkey()) - before, 1_000);

    // Pass side redeems 1:1; fail side is worthless.
    d.redeem(&bob, p).unwrap();
    assert_eq!((d.env.token_balance(&bob.base), d.env.token_balance(&bob.quote)), (pass_base, 400 + 100));
    assert_eq!(code(d.redeem(&bob, p)), err(GovError::NothingToClaim));
    assert_eq!(code(d.reclaim(&bob, p)), err(GovError::Unauthorized));
    d.reclaim(&alice, p).unwrap();
    assert_eq!(code(d.reclaim(&alice, p)), err(GovError::AlreadyClaimed));
    assert_eq!((d.env.token_balance(&alice.base), d.env.token_balance(&alice.quote)), (1_000 - pass_base, 1_000 + 500));
    let pr = d.proposal(p);
    assert_eq!((pr.base_collateral, pr.quote_collateral), (0, 0));
    assert_eq!(d.vaults(), (0, 0), "every token paid out, nothing stranded");
}

#[test]
fn a_proposal_the_fail_market_prefers_doesnt_run_and_the_fail_side_redeems() {
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let carol = d.trader(600, 0);
    let p = d.propose(&alice, 1_000, 1_000).unwrap();
    // Carol thinks it fails: sells pass base.
    d.split(&carol, p, 600, 0).unwrap();
    d.trade(&carol, p, Market::Pass, Side::Base, 500, 0).unwrap();
    d.env.warp(START + 100);
    d.finalize(p).unwrap();
    assert!(!d.proposal(p).passed);
    d.env.warp(START + 150);
    assert_eq!(code(d.env.execute(DM, p)), err(GovError::ProposalDidNotPass));
    d.redeem(&carol, p).unwrap();
    assert_eq!(d.env.token_balance(&carol.base), 600, "fail base back 1:1; her pass-side trade is worthless");
    d.reclaim(&alice, p).unwrap();
    assert_eq!(d.vaults(), (0, 0));
}

#[test]
fn equal_markets_dont_pass_and_a_last_second_spike_barely_moves_the_twap() {
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let p = d.propose(&alice, 1_000, 1_000).unwrap();
    d.env.warp(START + 100);
    d.finalize(p).unwrap();
    let pr = d.proposal(p);
    assert_eq!(pr.pass_twap, pr.fail_twap);
    assert!(!pr.passed, "needs to beat fail by the threshold");

    // A whale doubles the pass price in the last second: the TWAP moves about 1%.
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let whale = d.trader(0, 10_000);
    let p = d.propose(&alice, 1_000, 1_000).unwrap();
    d.split(&whale, p, 0, 5_000).unwrap();
    d.env.warp(START + 99);
    d.trade(&whale, p, Market::Pass, Side::Quote, 420, 0).unwrap();
    d.env.warp(START + 100);
    d.finalize(p).unwrap();
    let pr = d.proposal(p);
    assert!(!pr.passed, "pass {} vs fail {}: under the 5% threshold", pr.pass_twap, pr.fail_twap);
    assert!(pr.pass_twap > pr.fail_twap);
}

#[test]
fn merging_returns_the_underlying_and_cancelling_settles_as_failed() {
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let bob = d.trader(500, 500);
    assert_eq!(code(d.propose(&alice, 99, 1_000)), err(GovError::InvalidAmount), "below the minimum seed");
    let p = d.propose(&alice, 1_000, 1_000).unwrap();
    d.split(&bob, p, 300, 200).unwrap();
    assert_eq!(code(d.merge(&bob, p, 301, 0)), err(GovError::InsufficientConditionalBalance));
    d.merge(&bob, p, 100, 200).unwrap();
    assert_eq!((d.env.token_balance(&bob.base), d.env.token_balance(&bob.quote)), (300, 500));

    let cancel = |d: &Dm, who: &Keypair| Instruction {
        program_id: DM,
        accounts: dm::accounts::Cancel { authority: who.pubkey(), governance: d.env.governance(DM), proposal: p }.to_account_metas(None),
        data: dm::instruction::Cancel {}.data(),
    };
    assert_eq!(code(d.env.send(&[cancel(&d, &bob.kp)], &[&bob.kp])), err(GovError::Unauthorized));
    d.env.send(&[cancel(&d, &alice.kp)], &[&alice.kp]).unwrap();
    let pr = d.proposal(p);
    assert!(pr.finalized && !pr.passed && pr.cancelled);
    assert_eq!(code(d.split(&bob, p, 1, 0)), err(GovError::TradingClosed));
    d.redeem(&bob, p).unwrap();
    d.reclaim(&alice, p).unwrap();
    assert_eq!((d.env.token_balance(&bob.base), d.env.token_balance(&bob.quote)), (500, 500), "bob whole again");
    assert_eq!((d.env.token_balance(&alice.base), d.env.token_balance(&alice.quote)), (1_000, 1_000), "alice's seed back");
    assert_eq!(d.vaults(), (0, 0));
}

#[test]
fn after_a_token_change_new_proposals_trade_the_new_token_and_open_ones_keep_theirs() {
    let mut d = setup();
    let alice = d.trader(1_000, 1_000);
    let old_mint = d.env.mint;
    let open = d.propose(&alice, 200, 200).unwrap();

    // The DAO changes its token (the hub's record, as a passed proposal would).
    let creator = d.env.creator.insecure_clone();
    let new_mint = CreateMint::new(&mut d.env.svm, &creator).decimals(6).send().unwrap();
    let record = hub::dao_token_address(&d.env.dao);
    let mut account = d.env.svm.get_account(&record).unwrap();
    let mut t = hub::DaoToken::try_deserialize(&mut account.data.as_slice()).unwrap();
    t.mint = new_mint;
    t.pool = d.env.pool_for(new_mint);
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&t, &mut data).unwrap();
    account.data[..data.len()].copy_from_slice(&data);
    d.env.svm.set_account(record, account).unwrap();

    // Until someone syncs, proposing on the old token is refused.
    assert_eq!(code(d.propose(&alice, 200, 200)), err(GovError::WrongMint));
    let sync = Instruction {
        program_id: DM,
        accounts: dm::accounts::SyncBaseToken {
            payer: creator.pubkey(),
            governance: d.env.governance(DM),
            hub_dao: d.env.dao,
            dao_token: record,
            base_mint: new_mint,
            base_vault: base_vault(&d.env, &new_mint),
            base_token_program: d.env.token_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: dm::instruction::SyncBaseToken {}.data(),
    };
    d.env.send(&[sync.clone()], &[&creator]).unwrap();
    assert!(d.env.send(&[sync], &[&creator]).is_err(), "already synced");
    let g = dm::Governance::try_deserialize(&mut d.env.svm.get_account(&d.env.governance(DM)).unwrap().data.as_slice()).unwrap();
    assert_eq!((g.base_mint, g.base_vault), (new_mint, base_vault(&d.env, &new_mint)));

    // A trader with the new token proposes; the old proposal keeps the old vault.
    d.env.mint = new_mint;
    let bob = d.trader(1_000, 1_000);
    let p = d.propose(&bob, 200, 200).unwrap();
    assert_eq!((d.proposal(p).base_mint, d.proposal(p).base_vault), (new_mint, base_vault(&d.env, &new_mint)));
    assert_eq!((d.proposal(open).base_mint, d.proposal(open).base_vault), (old_mint, base_vault(&d.env, &old_mint)));
    assert_eq!(d.env.token_balance(&base_vault(&d.env, &old_mint)), 200);
    assert_eq!(d.env.token_balance(&base_vault(&d.env, &new_mint)), 200);
}
