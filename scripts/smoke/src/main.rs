//! Smoke test for a live cluster (devnet, or a local validator).
//!
//! `run`: sets up the hub (first time only), then for each model creates a
//! DAO, funds its treasury and pushes one proposal through that pays a
//! little SOL, checking the payment landed. It also starts a switch of the
//! token-weighted DAO to board, which can only finish after the hub's 2-day
//! delay.
//!
//! `finish-switch`: once the delay has passed, applies that switch and has
//! the board pay from the same treasury.
//!
//! ```text
//! cargo run -p vortex-smoke -- run [--url devnet] [--keypair ~/.config/solana/id.json]
//! cargo run -p vortex-smoke -- finish-switch [--url devnet]
//! ```
//!
//! The wallet pays for everything (about 0.6 SOL in all, mostly rent) and
//! becomes the hub admin if the hub isn't set up yet. Program IDs come from
//! the programs' source, so build after `anchor keys sync`.

use anchor_lang::{prelude::Pubkey, AccountDeserialize, InstructionData, ToAccountMetas};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::{read_keypair_file, write_keypair_file, Keypair};
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::{path::PathBuf, thread::sleep, time::Duration};
use vortex_core::{StoredAccountMeta, StoredInstruction, VotingConfig, GOVERNANCE_SEED};
use vortex_hub as hub;

const SOL: u64 = 1_000_000_000;
/// What each proposal pays: above the rent-exempt minimum for a new account.
const PAYMENT: u64 = 2_000_000;
const TREASURY_FUNDING: u64 = 50_000_000;
const MEMBER_FUNDING: u64 = 60_000_000;
const SYSTEM: Pubkey = anchor_lang::system_program::ID;

const TW: Pubkey = vortex_token_weighted::ID;
const QV: Pubkey = vortex_quadratic::ID;
const OPT: Pubkey = vortex_optimistic::ID;
const BOARD: Pubkey = vortex_board::ID;
const CONV: Pubkey = vortex_conviction::ID;
const DEL: Pubkey = vortex_delegate::ID;
const MODELS: [(Pubkey, &str); 6] = [
    (TW, "token-weighted"),
    (QV, "quadratic"),
    (OPT, "optimistic"),
    (BOARD, "board"),
    (CONV, "conviction"),
    (DEL, "delegate"),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let url = match flag("--url").as_deref() {
        None | Some("devnet") => "https://api.devnet.solana.com".to_string(),
        Some("localhost") | Some("local") => "http://127.0.0.1:8899".to_string(),
        Some(u) => u.to_string(),
    };
    let keypair_path = flag("--keypair").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").expect("HOME")).join(".config/solana/id.json")
    });
    let payer = read_keypair_file(&keypair_path).unwrap_or_else(|e| panic!("couldn't read {}: {e}", keypair_path.display()));
    let mut s = Smoke { rpc: RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed()), payer, url, results: vec![] };
    println!("cluster: {}\nwallet:  {} ({:.3} SOL)\n", s.url, s.payer.pubkey(), s.balance(&s.payer.pubkey()) as f64 / SOL as f64);

    match args.first().map(String::as_str) {
        Some("run") => s.run(),
        Some("finish-switch") => s.finish_switch(),
        _ => {
            eprintln!("usage: vortex-smoke <run|finish-switch> [--url devnet|localhost|URL] [--keypair PATH]");
            std::process::exit(2);
        }
    }
    s.report();
}

struct Member {
    kp: Keypair,
    ata: Pubkey,
}

/// One DAO being smoke-tested.
struct Dao {
    dao: Pubkey,
    treasury: Pubkey,
    mint: Pubkey,
}

impl Dao {
    fn governance(&self, gp: Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[GOVERNANCE_SEED, self.dao.as_ref()], &gp).0
    }
    fn vault(&self, gp: Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"vault", self.governance(gp).as_ref()], &gp).0
    }
    fn voter(&self, gp: Pubkey, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"voter", self.governance(gp).as_ref(), owner.as_ref()], &gp).0
    }
    fn proposal(&self, gp: Pubkey, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[b"proposal", self.governance(gp).as_ref(), &id.to_le_bytes()], &gp).0
    }
    fn pay(&self, to: &Pubkey) -> StoredInstruction {
        stored(solana_system_interface::instruction::transfer(&self.treasury, to, PAYMENT))
    }
}

struct Smoke {
    rpc: RpcClient,
    payer: Keypair,
    url: String,
    results: Vec<(String, Result<(), String>)>,
}

impl Smoke {
    // ---------------------------------------------------------------
    // run
    // ---------------------------------------------------------------

    fn run(&mut self) {
        let deployed: Vec<(Pubkey, &str)> = MODELS.iter().copied().filter(|(id, name)| self.check_deployed(id, name)).collect();
        if !self.check_deployed(&hub::ID, "hub") {
            panic!("the hub isn't deployed on this cluster");
        }
        self.setup_hub(&deployed);

        // A token and two members, Alice and Bob, holding 1000 each.
        let mint = self.create_mint();
        let alice = self.member(mint);
        let bob = self.member(mint);
        println!("token {mint}; members Alice {} and Bob {}\n", alice.kp.pubkey(), bob.kp.pubkey());

        for (gp, name) in deployed {
            println!("== {name}");
            // Each DAO keeps its deposits, so top the members up first.
            self.mint_to(mint, &[&alice, &bob]);
            let result = match gp {
                g if g == TW || g == QV => self.voting_flow(gp, mint, &alice, &bob),
                g if g == OPT => self.optimistic_flow(mint, &alice),
                g if g == BOARD => self.board_flow(mint, &alice, &bob),
                g if g == CONV => self.conviction_flow(mint, &alice),
                g if g == DEL => self.delegate_flow(mint, &alice, &bob),
                _ => unreachable!(),
            };
            self.record(name, result);
        }
        if self.results.iter().any(|(n, r)| n == "token-weighted" && r.is_ok()) && self.is_deployed(&BOARD) {
            println!("== switch: token-weighted -> board");
            self.mint_to(mint, &[&alice, &bob]);
            let result = self.start_switch(mint, &alice, &bob);
            self.record("switch (started)", result);
        }
    }

    fn check_deployed(&self, id: &Pubkey, name: &str) -> bool {
        let ok = self.is_deployed(id);
        if !ok {
            println!("{name}: not deployed at {id}, skipping");
        }
        ok
    }

    fn is_deployed(&self, id: &Pubkey) -> bool {
        self.rpc.get_account(id).map(|a| a.executable).unwrap_or(false)
    }

    /// `init_hub` if needed, then approve every deployed model.
    fn setup_hub(&mut self, models: &[(Pubkey, &str)]) {
        let hub_pda = Pubkey::find_program_address(&[hub::HUB_SEED], &hub::ID).0;
        if self.rpc.get_account(&hub_pda).is_err() {
            let ix = Instruction {
                program_id: hub::ID,
                accounts: hub::accounts::InitHub { admin: self.payer.pubkey(), hub: hub_pda, system_program: SYSTEM }.to_account_metas(None),
                data: hub::instruction::InitHub {}.data(),
            };
            let sig = self.send(&[ix], &[]).expect("init_hub");
            println!("hub set up, admin = this wallet ({sig})");
        }
        let admin = hub::Hub::try_deserialize(&mut self.rpc.get_account_data(&hub_pda).unwrap().as_slice()).unwrap().admin;
        for (gp, name) in models {
            let model = model_pda(gp);
            let approved = self
                .rpc
                .get_account_data(&model)
                .ok()
                .and_then(|d| hub::Model::try_deserialize(&mut d.as_slice()).ok())
                .is_some_and(|m| m.enabled);
            if approved {
                continue;
            }
            if admin != self.payer.pubkey() {
                println!("{name}: not approved, and this wallet isn't the hub admin ({admin})");
                continue;
            }
            let ix = Instruction {
                program_id: hub::ID,
                accounts: hub::accounts::SetModel { admin: self.payer.pubkey(), hub: hub_pda, model, system_program: SYSTEM }.to_account_metas(None),
                data: hub::instruction::SetModel { program_id: *gp, name: name.to_string(), enabled: true }.data(),
            };
            let sig = self.send(&[ix], &[]).expect("set_model");
            println!("approved {name} ({sig})");
        }
        println!();
    }

    // ---------------------------------------------------------------
    // per-model flows
    // ---------------------------------------------------------------

    fn voting_flow(&mut self, gp: Pubkey, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        let config = voting_config();
        let dao = self.create_dao(mint, gp, "Smoke vote", |d, me| ix_init_voting(d, gp, me, config))?;
        self.deposit(&dao, gp, alice, 600)?;
        self.deposit(&dao, gp, bob, 400)?;
        let to = Keypair::new().pubkey();
        let p = self.propose_voting(&dao, gp, alice, vec![dao.pay(&to)])?;
        let prop = read::<vortex_token_weighted::Proposal>(&self.rpc, &p);
        self.wait_until(prop.voting_starts_at);
        self.send_ix(
            Instruction {
                program_id: gp,
                accounts: vortex_token_weighted::accounts::CastVote {
                    owner: alice.kp.pubkey(),
                    governance: dao.governance(gp),
                    proposal: p,
                    voter: dao.voter(gp, &alice.kp.pubkey()),
                    vote_record: Pubkey::find_program_address(&[b"vote", p.as_ref(), alice.kp.pubkey().as_ref()], &gp).0,
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: vortex_token_weighted::instruction::CastVote { choice: vortex_token_weighted::VoteChoice::For }.data(),
            },
            &[&alice.kp],
            "vote",
        )?;
        self.wait_until(prop.voting_ends_at);
        self.queue(&dao, gp, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, gp, p)?;
        self.expect_paid(&to)
    }

    fn optimistic_flow(&mut self, mint: Pubkey, alice: &Member) -> Result<(), String> {
        let config = vortex_optimistic::OptimisticConfig {
            challenge_period: 15,
            challenge_bond: 1,
            quorum_bps: 1_000,
            approval_bps: 5_000,
            voting_period: 20,
            timelock: 5,
            execution_period: 3_600,
            proposal_threshold: 1,
        };
        let dao = self.create_dao(mint, OPT, "Smoke optimistic", |d, me| Instruction {
            program_id: OPT,
            accounts: vortex_optimistic::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(OPT),
                mint: d.mint,
                vault: d.vault(OPT),
                bond_vault: Pubkey::find_program_address(&[vortex_optimistic::BOND_VAULT_SEED, d.governance(OPT).as_ref()], &OPT).0,
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: vortex_optimistic::instruction::InitGovernance { config }.data(),
        })?;
        self.deposit(&dao, OPT, alice, 600)?;
        let to = Keypair::new().pubkey();
        let p = self.propose_voting(&dao, OPT, alice, vec![dao.pay(&to)])?;
        let deadline = read::<vortex_optimistic::Proposal>(&self.rpc, &p).challenge_deadline;
        self.wait_until(deadline);
        self.send_ix(
            Instruction {
                program_id: OPT,
                accounts: vortex_optimistic::accounts::FinalizeUnchallenged { governance: dao.governance(OPT), proposal: p }.to_account_metas(None),
                data: vortex_optimistic::instruction::FinalizeUnchallenged {}.data(),
            },
            &[],
            "finalize_unchallenged",
        )?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, OPT, p)?;
        self.expect_paid(&to)
    }

    fn board_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        let config = vortex_board::BoardConfig { required_approvals: 2, timelock: 5, execution_period: 3_600 };
        let signers = vec![alice.kp.pubkey(), bob.kp.pubkey()];
        let dao = self.create_dao(mint, BOARD, "Smoke board", |d, me| ix_init_board(d, me, signers, config))?;
        let to = Keypair::new().pubkey();
        let p = self.board_propose(&dao, &alice.kp, vec![dao.pay(&to)])?;
        self.board_confirm(&dao, &bob.kp, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, BOARD, p)?;
        self.expect_paid(&to)
    }

    fn conviction_flow(&mut self, mint: Pubkey, alice: &Member) -> Result<(), String> {
        use vortex_conviction as conv;
        let config = conv::ConvictionConfig { growth_rate: 50, min_conviction: 100, support_bps: 2_000, proposal_threshold: 1, timelock: 5, execution_period: 3_600 };
        let dao = self.create_dao(mint, CONV, "Smoke conviction", |d, me| Instruction {
            program_id: CONV,
            accounts: conv::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(CONV),
                mint: d.mint,
                vault: d.vault(CONV),
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: conv::instruction::InitGovernance { config, sol_weight: 100, token_weight: 100 }.data(),
        })?;
        self.deposit(&dao, CONV, alice, 600)?;
        let to = Keypair::new().pubkey();
        let p = dao.proposal(CONV, 1);
        let mut accounts = conv::accounts::Propose {
            proposer: alice.kp.pubkey(),
            governance: dao.governance(CONV),
            hub_dao: dao.dao,
            voter: Some(dao.voter(CONV, &alice.kp.pubkey())),
            proposal: p,
            system_program: SYSTEM,
        }
        .to_account_metas(None);
        let g = read::<conv::Governance>(&self.rpc, &dao.governance(CONV));
        accounts.extend(g.assets.iter().map(|a| AccountMeta::new_readonly(a.account, false)));
        let budget = vec![conv::AssetAmount { mint: conv::SOL, amount: PAYMENT }];
        let data = conv::instruction::Propose { id: 1, metadata_uri: "Smoke test payment".into(), instructions: vec![dao.pay(&to)], budget }.data();
        self.send_ix(Instruction { program_id: CONV, accounts, data }, &[&alice.kp], "propose")?;
        self.send_ix(
            Instruction {
                program_id: CONV,
                accounts: conv::accounts::Support {
                    owner: alice.kp.pubkey(),
                    governance: dao.governance(CONV),
                    voter: dao.voter(CONV, &alice.kp.pubkey()),
                    proposal: p,
                    previous: None,
                }
                .to_account_metas(None),
                data: conv::instruction::Support {}.data(),
            },
            &[&alice.kp],
            "support",
        )?;
        let required = read::<conv::Proposal>(&self.rpc, &p).required_conviction;
        println!("  bar {required}, building at {} per second", config.growth_rate);
        self.wait_until(self.now() + required.div_ceil(config.growth_rate) as i64 + 1);
        self.queue(&dao, CONV, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, CONV, p)?;
        self.expect_paid(&to)
    }

    fn delegate_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        use vortex_delegate as del;
        let config = del::DelegateConfig {
            council_size: 2,
            term_length: 3_600,
            candidacy_threshold: 1,
            candidacy_period: 60,
            election_voting_period: 60,
            council_quorum: 2,
            council_approval_bps: 5_000,
            voting_delay: 0,
            voting_period: 20,
            timelock: 5,
            execution_period: 3_600,
            recall_quorum_bps: 1_000,
            recall_approval_bps: 5_000,
            recall_voting_period: 60,
        };
        let council = vec![alice.kp.pubkey(), bob.kp.pubkey()];
        let dao = self.create_dao(mint, DEL, "Smoke delegate", |d, me| Instruction {
            program_id: DEL,
            accounts: del::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(DEL),
                mint: d.mint,
                vault: d.vault(DEL),
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: del::instruction::InitGovernance { council, config }.data(),
        })?;
        let to = Keypair::new().pubkey();
        let p = dao.proposal(DEL, 1);
        self.send_ix(
            Instruction {
                program_id: DEL,
                accounts: del::accounts::Propose { proposer: alice.kp.pubkey(), governance: dao.governance(DEL), hub_dao: dao.dao, proposal: p, system_program: SYSTEM }
                    .to_account_metas(None),
                data: del::instruction::Propose { id: 1, metadata_uri: "Smoke test payment".into(), instructions: vec![dao.pay(&to)] }.data(),
            },
            &[&alice.kp],
            "propose",
        )?;
        for m in [alice, bob] {
            self.send_ix(
                Instruction {
                    program_id: DEL,
                    accounts: del::accounts::CouncilAction { member: m.kp.pubkey(), governance: dao.governance(DEL), proposal: p }.to_account_metas(None),
                    data: del::instruction::CastVote { choice: del::VoteChoice::For }.data(),
                },
                &[&m.kp],
                "council vote",
            )?;
        }
        let ends = read::<del::Proposal>(&self.rpc, &p).voting_ends_at;
        self.wait_until(ends);
        self.queue(&dao, DEL, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, DEL, p)?;
        self.expect_paid(&to)
    }

    // ---------------------------------------------------------------
    // switching
    // ---------------------------------------------------------------

    /// A token-weighted DAO votes to switch to a 2-of-2 board of Alice and
    /// Bob, setting the board up in the same proposal. Saves what
    /// `finish-switch` needs.
    fn start_switch(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        let config = voting_config();
        let dao = self.create_dao(mint, TW, "Smoke switch", |d, me| ix_init_voting(d, TW, me, config))?;
        self.deposit(&dao, TW, alice, 600)?;
        let board = vortex_board::BoardConfig { required_approvals: 2, timelock: 5, execution_period: 3_600 };
        let init_board = stored(ix_init_board(&dao, &dao.treasury, vec![alice.kp.pubkey(), bob.kp.pubkey()], board));
        let propose_switch = stored(Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::ProposeSwitch { treasury: dao.treasury, dao: dao.dao, model: model_pda(&BOARD) }.to_account_metas(None),
            data: hub::instruction::ProposeSwitch { new_program: BOARD }.data(),
        });
        let p = self.propose_voting(&dao, TW, alice, vec![propose_switch, init_board])?;
        let prop = read::<vortex_token_weighted::Proposal>(&self.rpc, &p);
        self.wait_until(prop.voting_starts_at);
        self.send_ix(
            Instruction {
                program_id: TW,
                accounts: vortex_token_weighted::accounts::CastVote {
                    owner: alice.kp.pubkey(),
                    governance: dao.governance(TW),
                    proposal: p,
                    voter: dao.voter(TW, &alice.kp.pubkey()),
                    vote_record: Pubkey::find_program_address(&[b"vote", p.as_ref(), alice.kp.pubkey().as_ref()], &TW).0,
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: vortex_token_weighted::instruction::CastVote { choice: vortex_token_weighted::VoteChoice::For }.data(),
            },
            &[&alice.kp],
            "vote",
        )?;
        self.wait_until(prop.voting_ends_at);
        self.queue(&dao, TW, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, TW, p)?;
        let record = read::<hub::Dao>(&self.rpc, &dao.dao);
        let ready_at = record.pending_switch.ok_or("the switch isn't pending")?.ready_at;

        let dir = state_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        write_keypair_file(&alice.kp, dir.join("alice.json")).map_err(|e| e.to_string())?;
        write_keypair_file(&bob.kp, dir.join("bob.json")).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("switch.txt"), format!("{}\n{}\n{}\n", dao.dao, mint, ready_at)).map_err(|e| e.to_string())?;
        println!("  switch pending; run `finish-switch` after {} (unix time {ready_at})", describe_wait(ready_at - self.now()));
        Ok(())
    }

    fn finish_switch(&mut self) {
        let dir = state_dir();
        let saved = std::fs::read_to_string(dir.join("switch.txt")).expect("no saved switch: run `run` first");
        let mut lines = saved.lines();
        let dao_key: Pubkey = lines.next().unwrap().parse().unwrap();
        let mint: Pubkey = lines.next().unwrap().parse().unwrap();
        let alice = read_keypair_file(dir.join("alice.json")).unwrap();
        let bob = read_keypair_file(dir.join("bob.json")).unwrap();
        let dao = Dao { dao: dao_key, treasury: hub::treasury_address(&dao_key), mint };
        let result = (|| {
            let record = read::<hub::Dao>(&self.rpc, &dao.dao);
            if record.governance_program != BOARD {
                let ready_at = record.pending_switch.ok_or("no switch pending")?.ready_at;
                if self.now() < ready_at {
                    return Err(format!("not ready yet: {} to go", describe_wait(ready_at - self.now())));
                }
                self.send_ix(
                    Instruction {
                        program_id: hub::ID,
                        accounts: hub::accounts::ApplySwitch { dao: dao.dao, model: model_pda(&BOARD), new_governance: dao.governance(BOARD) }.to_account_metas(None),
                        data: hub::instruction::ApplySwitch {}.data(),
                    },
                    &[],
                    "apply_switch",
                )?;
            }
            let record = read::<hub::Dao>(&self.rpc, &dao.dao);
            println!("  the DAO now runs on board, epoch {}; same treasury {}", record.epoch, dao.treasury);
            let to = Keypair::new().pubkey();
            let p = self.board_propose(&dao, &alice, vec![dao.pay(&to)])?;
            self.board_confirm(&dao, &bob, p)?;
            self.wait_until(self.now() + 5);
            self.execute(&dao, BOARD, p)?;
            self.expect_paid(&to)
        })();
        self.record("switch (finished)", result);
    }

    // ---------------------------------------------------------------
    // shared steps
    // ---------------------------------------------------------------

    /// Creates a hub DAO and its first setup in `gp`, in one transaction,
    /// then funds its treasury.
    fn create_dao(&mut self, mint: Pubkey, gp: Pubkey, name: &str, init: impl FnOnce(&Dao, &Pubkey) -> Instruction) -> Result<Dao, String> {
        let create_key = Keypair::new();
        let dao_key = Pubkey::find_program_address(&[hub::DAO_SEED, create_key.pubkey().as_ref()], &hub::ID).0;
        let dao = Dao { dao: dao_key, treasury: hub::treasury_address(&dao_key), mint };
        let create = Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::CreateDao { creator: self.payer.pubkey(), create_key: create_key.pubkey(), model: model_pda(&gp), dao: dao_key, system_program: SYSTEM }
                .to_account_metas(None),
            data: hub::instruction::CreateDao { name: name.into(), governance_program: gp }.data(),
        };
        let init = init(&dao, &self.payer.pubkey());
        let fund = solana_system_interface::instruction::transfer(&self.payer.pubkey(), &dao.treasury, TREASURY_FUNDING);
        let sig = self.send(&[create, init, fund], &[&create_key]).map_err(|e| format!("create DAO: {e}"))?;
        println!("  DAO {dao_key} created, treasury {} funded ({sig})", dao.treasury);
        Ok(dao)
    }

    fn deposit(&mut self, dao: &Dao, gp: Pubkey, m: &Member, amount: u64) -> Result<(), String> {
        let ix = Instruction {
            program_id: gp,
            accounts: vortex_token_weighted::accounts::Deposit {
                owner: m.kp.pubkey(),
                governance: dao.governance(gp),
                voter: dao.voter(gp, &m.kp.pubkey()),
                owner_token_account: m.ata,
                vault: dao.vault(gp),
                mint: dao.mint,
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: vortex_token_weighted::instruction::Deposit { amount }.data(),
        };
        self.send_ix(ix, &[&m.kp], "deposit").map(|_| ())
    }

    /// Proposal 1 in a token-weighted, quadratic or optimistic DAO (same
    /// layout in all three).
    fn propose_voting(&mut self, dao: &Dao, gp: Pubkey, m: &Member, instructions: Vec<StoredInstruction>) -> Result<Pubkey, String> {
        let p = dao.proposal(gp, 1);
        let ix = Instruction {
            program_id: gp,
            accounts: vortex_token_weighted::accounts::Propose {
                proposer: m.kp.pubkey(),
                governance: dao.governance(gp),
                hub_dao: dao.dao,
                voter: Some(dao.voter(gp, &m.kp.pubkey())),
                proposal: p,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: vortex_token_weighted::instruction::Propose { id: 1, metadata_uri: "Smoke test payment".into(), instructions }.data(),
        };
        self.send_ix(ix, &[&m.kp], "propose")?;
        Ok(p)
    }

    fn queue(&mut self, dao: &Dao, gp: Pubkey, p: Pubkey) -> Result<(), String> {
        let ix = Instruction {
            program_id: gp,
            accounts: vortex_token_weighted::accounts::Queue { governance: dao.governance(gp), proposal: p }.to_account_metas(None),
            data: vortex_token_weighted::instruction::Queue {}.data(),
        };
        self.send_ix(ix, &[], "queue").map(|_| ())
    }

    fn board_propose(&mut self, dao: &Dao, signer: &Keypair, instructions: Vec<StoredInstruction>) -> Result<Pubkey, String> {
        let id = read::<vortex_board::Governance>(&self.rpc, &dao.governance(BOARD)).proposal_count + 1;
        let p = dao.proposal(BOARD, id);
        let ix = Instruction {
            program_id: BOARD,
            accounts: vortex_board::accounts::Propose { proposer: signer.pubkey(), governance: dao.governance(BOARD), hub_dao: dao.dao, proposal: p, system_program: SYSTEM }
                .to_account_metas(None),
            data: vortex_board::instruction::Propose { id, metadata_uri: "Smoke test payment".into(), instructions }.data(),
        };
        self.send_ix(ix, &[signer], "propose")?;
        Ok(p)
    }

    fn board_confirm(&mut self, dao: &Dao, signer: &Keypair, p: Pubkey) -> Result<(), String> {
        let ix = Instruction {
            program_id: BOARD,
            accounts: vortex_board::accounts::Confirm { signer: signer.pubkey(), governance: dao.governance(BOARD), proposal: p }.to_account_metas(None),
            data: vortex_board::instruction::Confirm {}.data(),
        };
        self.send_ix(ix, &[signer], "confirm").map(|_| ())
    }

    /// Runs a proposal through the hub, passing every account its
    /// instructions use.
    fn execute(&mut self, dao: &Dao, gp: Pubkey, proposal: Pubkey) -> Result<(), String> {
        let core = vortex_core::ProposalCore::read(&self.rpc.get_account_data(&proposal).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let mut accounts = hub::accounts::Execute {
            dao: dao.dao,
            executor: hub::executor_address(&dao.dao),
            governance_program: gp,
            governance: dao.governance(gp),
            proposal,
        }
        .to_account_metas(None);
        for ix in &core.instructions {
            for m in &ix.accounts {
                // The treasury signs inside the hub, not in the transaction.
                accounts.push(AccountMeta { pubkey: m.pubkey, is_signer: false, is_writable: m.is_writable });
            }
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
        }
        let ix = Instruction { program_id: hub::ID, accounts, data: hub::instruction::Execute {}.data() };
        let budget = solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(400_000);
        let sig = self.send(&[budget, ix], &[]).map_err(|e| format!("execute: {e}"))?;
        println!("  executed through the hub ({sig})");
        Ok(())
    }

    fn expect_paid(&self, to: &Pubkey) -> Result<(), String> {
        let got = self.balance(to);
        if got == PAYMENT {
            println!("  recipient {to} received {PAYMENT} lamports");
            Ok(())
        } else {
            Err(format!("recipient {to} has {got} lamports, expected {PAYMENT}"))
        }
    }

    // ---------------------------------------------------------------
    // token and members
    // ---------------------------------------------------------------

    fn create_mint(&mut self) -> Pubkey {
        let mint = Keypair::new();
        let rent = self.rpc.get_minimum_balance_for_rent_exemption(82).unwrap();
        let ixs = [
            solana_system_interface::instruction::create_account(&self.payer.pubkey(), &mint.pubkey(), rent, 82, &spl_token_interface::ID),
            spl_token_interface::instruction::initialize_mint2(&spl_token_interface::ID, &mint.pubkey(), &self.payer.pubkey(), None, 6).unwrap(),
        ];
        self.send(&ixs, &[&mint]).expect("create mint");
        mint.pubkey()
    }

    /// A new wallet with some SOL and 1000 tokens.
    fn member(&mut self, mint: Pubkey) -> Member {
        let kp = Keypair::new();
        let ata = spl_associated_token_account_interface::address::get_associated_token_address(&kp.pubkey(), &mint);
        let ixs = [
            solana_system_interface::instruction::transfer(&self.payer.pubkey(), &kp.pubkey(), MEMBER_FUNDING),
            spl_associated_token_account_interface::instruction::create_associated_token_account(&self.payer.pubkey(), &kp.pubkey(), &mint, &spl_token_interface::ID),
            spl_token_interface::instruction::mint_to(&spl_token_interface::ID, &mint, &ata, &self.payer.pubkey(), &[], 1_000).unwrap(),
        ];
        self.send(&ixs, &[]).expect("fund member");
        Member { kp, ata }
    }

    /// 1000 more tokens for each member.
    fn mint_to(&mut self, mint: Pubkey, members: &[&Member]) {
        let ixs: Vec<Instruction> = members
            .iter()
            .map(|m| spl_token_interface::instruction::mint_to(&spl_token_interface::ID, &mint, &m.ata, &self.payer.pubkey(), &[], 1_000).unwrap())
            .collect();
        self.send(&ixs, &[]).expect("mint to members");
    }

    // ---------------------------------------------------------------
    // cluster plumbing
    // ---------------------------------------------------------------

    /// Sends with this wallet paying fees, plus any other signers.
    fn send(&self, ixs: &[Instruction], extra: &[&Keypair]) -> Result<String, String> {
        let blockhash = self.rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut signers: Vec<&Keypair> = vec![&self.payer];
        signers.extend_from_slice(extra);
        let tx = Transaction::new_signed_with_payer(ixs, Some(&self.payer.pubkey()), &signers, blockhash);
        self.rpc.send_and_confirm_transaction(&tx).map(|s| s.to_string()).map_err(|e| e.to_string())
    }

    fn send_ix(&self, ix: Instruction, extra: &[&Keypair], what: &str) -> Result<String, String> {
        let sig = self.send(&[ix], extra).map_err(|e| format!("{what}: {e}"))?;
        println!("  {what} ({sig})");
        Ok(sig)
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.rpc.get_balance(key).unwrap_or(0)
    }

    /// The cluster's clock (unix_timestamp in the Clock sysvar).
    fn now(&self) -> i64 {
        let data = self.rpc.get_account_data(&solana_sdk_ids::sysvar::clock::ID).expect("clock sysvar");
        // Clock: slot, epoch_start_timestamp, epoch, leader_schedule_epoch (8 bytes each), then unix_timestamp.
        i64::from_le_bytes(data[32..40].try_into().unwrap())
    }

    /// Waits until the cluster's clock reaches `t`.
    fn wait_until(&self, t: i64) {
        let mut shown = false;
        loop {
            let now = self.now();
            if now >= t {
                return;
            }
            if !shown {
                println!("  waiting {}s for the cluster clock", t - now);
                shown = true;
            }
            sleep(Duration::from_secs(2));
        }
    }

    fn record(&mut self, name: &str, result: Result<(), String>) {
        match &result {
            Ok(()) => println!("  PASS\n"),
            Err(e) => println!("  FAIL: {e}\n"),
        }
        self.results.push((name.to_string(), result));
    }

    fn report(&self) {
        println!("== results");
        for (name, r) in &self.results {
            match r {
                Ok(()) => println!("  PASS  {name}"),
                Err(e) => println!("  FAIL  {name}: {e}"),
            }
        }
        if self.results.iter().any(|(_, r)| r.is_err()) {
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------
// helpers
// ---------------------------------------------------------------

fn voting_config() -> VotingConfig {
    VotingConfig { quorum_bps: 1_000, approval_bps: 5_000, voting_delay: 0, voting_period: 20, timelock: 5, execution_period: 3_600, proposal_threshold: 1 }
}

fn ix_init_voting(d: &Dao, gp: Pubkey, authority: &Pubkey, config: VotingConfig) -> Instruction {
    Instruction {
        program_id: gp,
        accounts: vortex_token_weighted::accounts::InitGovernance {
            authority: *authority,
            payer: *authority,
            hub_dao: d.dao,
            governance: d.governance(gp),
            mint: d.mint,
            vault: d.vault(gp),
            token_program: spl_token_interface::ID,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: vortex_token_weighted::instruction::InitGovernance { config }.data(),
    }
}

fn ix_init_board(d: &Dao, authority: &Pubkey, signers: Vec<Pubkey>, config: vortex_board::BoardConfig) -> Instruction {
    Instruction {
        program_id: BOARD,
        accounts: vortex_board::accounts::InitGovernance { authority: *authority, payer: *authority, hub_dao: d.dao, governance: d.governance(BOARD), system_program: SYSTEM }
            .to_account_metas(None),
        data: vortex_board::instruction::InitGovernance { signers, config }.data(),
    }
}

fn model_pda(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[hub::MODEL_SEED, program.as_ref()], &hub::ID).0
}

fn stored(ix: Instruction) -> StoredInstruction {
    StoredInstruction {
        program_id: ix.program_id,
        accounts: ix.accounts.iter().map(|m| StoredAccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data,
    }
}

fn read<T: AccountDeserialize>(rpc: &RpcClient, key: &Pubkey) -> T {
    T::try_deserialize(&mut rpc.get_account_data(key).unwrap_or_else(|e| panic!("reading {key}: {e}")).as_slice()).unwrap()
}

/// Where `run` leaves what `finish-switch` needs (gitignored).
fn state_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".state")
}

fn describe_wait(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
}
