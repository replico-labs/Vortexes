//! Every model can be switched to, from a Board-born DAO, with the new
//! setup made by the treasury in the switch proposal itself; and back.

use vortex_decision_markets as dm;
use vortex_sortition as sort;
use vortex_sowellian as sow;
use vortex_tests::*;

const MODELS: [Pubkey; 10] = [TW, QV, OPT, BOARD, CONV, DEL, LIQ, SORT, SOW, DM];

/// The treasury's setup of `gp` for the next epoch.
fn init_for(env: &mut Env, gp: Pubkey, signer: Pubkey) -> Instruction {
    let t = env.treasury;
    if gp == OPT {
        env.ix_init_optimistic(&t, &t, opt_config())
    } else if gp == BOARD {
        env.ix_init_board(&t, &t, vec![signer], board_config(1))
    } else if gp == CONV {
        env.ix_init_conviction(&t, &t, conv_config())
    } else if gp == DEL {
        env.ix_init_delegate(&t, &t, vec![signer], DelegateConfig { council_size: 1, council_quorum: 1, ..del_config() })
    } else if gp == SORT {
        let governance = env.governance(SORT);
        let config = sort::SortitionConfig {
            council_size: 1,
            term_length: 1_000,
            eligibility_threshold: 100,
            council_quorum: 1,
            council_approval_bps: 6_000,
            voting_delay: 10,
            voting_period: 100,
            timelock: 50,
            execution_period: 200,
            randomness_timeout: 60,
        };
        Instruction {
            program_id: SORT,
            accounts: sort::accounts::InitGovernance {
                authority: t,
                payer: t,
                hub_dao: env.dao,
                governance,
                pool: Pubkey::find_program_address(&[sort::POOL_SEED, governance.as_ref()], &SORT).0,
                dao_token: hub::dao_token_address(&env.dao),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: sort::instruction::InitGovernance { epoch: env.governance_epoch(SORT), council: vec![signer], config }.data(),
        }
    } else if gp == SOW {
        let config = sow::SowellianConfig {
            proposal_bond: 50,
            approval_voting_delay: 10,
            approval_voting_period: 100,
            approval_quorum_bps: 1_000,
            approval_threshold_bps: 6_000,
            positions_window: 100,
            execution_timelock: 50,
            execution_period: 200,
            resolution_bond: 20,
            challenge_period: 100,
            challenge_bond: 30,
            adjudication_voting_period: 100,
            adjudication_quorum_bps: 1_000,
            adjudication_threshold_bps: 6_000,
            max_oracle_staleness: 60,
        };
        Instruction {
            program_id: SOW,
            accounts: sow::accounts::InitGovernance { authority: t, payer: t, hub_dao: env.dao, governance: env.governance(SOW), dao_token: hub::dao_token_address(&env.dao), system_program: SYSTEM }
                .to_account_metas(None),
            data: sow::instruction::InitGovernance { epoch: env.governance_epoch(SOW), config }.data(),
        }
    } else if gp == DM {
        let creator = env.creator.insecure_clone();
        let quote_mint = CreateMint::new(&mut env.svm, &creator).decimals(6).send().unwrap();
        let governance = env.governance(DM);
        let config = dm::MarketsConfig { trading_period: 100, threshold_bps: 500, timelock: 50, execution_period: 200, min_base_seed: 100, min_quote_seed: 100 };
        Instruction {
            program_id: DM,
            accounts: dm::accounts::InitGovernance {
                authority: t,
                payer: t,
                hub_dao: env.dao,
                governance,
                base_mint: env.mint,
                dao_token: hub::dao_token_address(&env.dao),
                quote_mint,
                base_vault: Pubkey::find_program_address(&[dm::BASE_VAULT_SEED, governance.as_ref(), env.mint.as_ref()], &DM).0,
                quote_vault: Pubkey::find_program_address(&[dm::QUOTE_VAULT_SEED, governance.as_ref()], &DM).0,
                base_token_program: env.token_program,
                quote_token_program: env.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: dm::instruction::InitGovernance { epoch: env.governance_epoch(DM), config }.data(),
        }
    } else {
        env.ix_init_governance(gp, &t, &t, config())
    }
}

/// A Board-born DAO (the creator its only signer) with a token on record.
fn board_dao() -> Env {
    let mut env = Env::bare(litesvm_token::TOKEN_ID);
    let creator = env.creator.pubkey();
    env.create_dao("Board-born", BOARD, |env| env.ix_init_board(&creator, &creator, vec![creator], board_config(1)));
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    env
}

/// Board proposes and runs `instructions` (1 of 1).
fn board_runs(env: &mut Env, instructions: Vec<StoredInstruction>) {
    let creator = env.creator.insecure_clone();
    let p = env.board_propose(&creator, instructions).unwrap();
    let t = env.now();
    env.warp(t + 50);
    env.execute(BOARD, p).unwrap();
}

fn switch_from_board(env: &mut Env, to: Pubkey) {
    let epoch = env.hub_dao().epoch;
    env.pin(to, epoch + 1);
    let signer = env.creator.pubkey();
    let init = stored(init_for(env, to, signer));
    env.pin(BOARD, epoch);
    board_runs(env, vec![stored(env.ix_propose_switch(to)), init]);
    env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
    env.pin(to, epoch + 1);
    env.apply_switch(to).unwrap();
    env.pin.set(None);
    let d = env.hub_dao();
    assert_eq!((d.governance_program, d.epoch, d.governance), (to, epoch + 1, env.governance_at(to, epoch + 1)));
}

#[test]
fn a_board_born_dao_can_switch_to_every_model() {
    for to in MODELS {
        let mut env = board_dao();
        switch_from_board(&mut env, to);
        assert_eq!(env.dao_token().mint, env.mint, "the token on record stays");
    }
}

#[test]
fn token_models_switch_back_to_a_board_and_on_again() {
    for model in [TW, QV] {
        let mut env = board_dao();
        switch_from_board(&mut env, model);
        let alice = env.member(1_000);
        env.deposit(model, &alice, 600).unwrap();

        // model -> board (epoch 3), set up by the treasury.
        let s1 = Keypair::new();
        env.svm.airdrop(&s1.pubkey(), 1_000_000_000).unwrap();
        env.pin(BOARD, 3);
        let init = stored(env.ix_init_board(&env.treasury, &env.treasury, vec![s1.pubkey(), env.creator.pubkey()], board_config(1)));
        env.pin.set(None);
        let s = env.pass(model, &alice, vec![stored(env.ix_propose_switch(BOARD)), init]);
        env.execute(model, s).unwrap();
        env.warp(env.hub_dao().pending_switch.unwrap().ready_at);
        env.apply_switch(BOARD).unwrap();
        assert_eq!((env.hub_dao().governance_program, env.hub_dao().epoch), (BOARD, 3));
        assert_eq!(env.board_governance().signers.len(), 2, "the new board, not the epoch-1 one");

        // board -> the same model again (epoch 4): alice's stake is still there.
        switch_from_board(&mut env, model);
        let host = Keypair::new().pubkey();
        let p = env.pass(model, &alice, vec![env.pay_sol(&host, 1_000_000)]);
        env.execute(model, p).unwrap();
        assert_eq!(env.lamports(&host), 1_000_000);
    }
}
