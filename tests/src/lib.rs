//! Test harness shared by the end-to-end tests: the compiled hub and
//! governance programs in LiteSVM, with the real SPL Token, Token-2022 and Associated
//! Token programs. Build first with `anchor build`.
//!
//! token-weighted and quadratic share their source (token_voting.rs), so
//! their instructions and accounts have identical layouts: the helpers
//! build them with token-weighted's types and point them at either
//! program by ID.

use solana_clock::Clock;
use solana_instruction_error::InstructionError;
use solana_message::Message;
use solana_transaction::{Transaction, TransactionError};
pub use anchor_lang::{
    prelude::{AccountMeta, Pubkey},
    AccountDeserialize, InstructionData, ToAccountMetas,
};
pub use litesvm::{types::FailedTransactionMetadata, LiteSVM};
pub use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
pub use solana_instruction::Instruction;
pub use solana_keypair::Keypair;
pub use solana_signer::Signer;
pub use vortex_core::{GovError, ProposalState, StoredAccountMeta, StoredInstruction, VotingConfig, GOVERNANCE_SEED};
pub use vortex_hub as hub;
pub use vortex_token_weighted::{self as tw, Governance, Proposal, VoteChoice, Voter};

pub const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/");
pub const START: i64 = 1_800_000_000;
pub const TW: Pubkey = tw::ID;
pub const QV: Pubkey = vortex_quadratic::ID;
pub const OPT: Pubkey = vortex_optimistic::ID;
pub const BOARD: Pubkey = vortex_board::ID;
pub const CONV: Pubkey = vortex_conviction::ID;
pub const DEL: Pubkey = vortex_delegate::ID;
pub const SYSTEM: Pubkey = anchor_lang::system_program::ID;

pub fn config() -> VotingConfig {
    VotingConfig {
        quorum_bps: 1_000,   // 10%
        approval_bps: 6_000, // 60%
        voting_delay: 10,
        voting_period: 100,
        timelock: 50,
        execution_period: 200,
        proposal_threshold: 100,
    }
}

pub struct Member {
    pub kp: Keypair,
    pub ata: Pubkey,
}

pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub creator: Keypair,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub dao: Pubkey,
    pub treasury: Pubkey,
}

impl Env {
    pub fn new() -> Self {
        Self::with_token_program(litesvm_token::TOKEN_ID)
    }

    pub fn with_token_program(token_program: Pubkey) -> Self {
        let mut env = Self::bare(token_program);
        // A DAO run by token-weighted: hub record + its setup there, in one transaction.
        let creator = env.creator.pubkey();
        let init = env_init(TW);
        env.create_dao("Ark", TW, |env| init(env, &creator));
        env
    }

    /// The hub with all six models approved, a token mint, and no DAO yet.
    pub fn bare(token_program: Pubkey) -> Self {
        let mut svm = LiteSVM::new();
        for (id, name) in [
            (hub::ID, "vortex_hub"),
            (TW, "vortex_token_weighted"),
            (QV, "vortex_quadratic"),
            (OPT, "vortex_optimistic"),
            (BOARD, "vortex_board"),
            (CONV, "vortex_conviction"),
            (DEL, "vortex_delegate"),
        ] {
            if let Err(e) = svm.add_program_from_file(id, format!("{DIR}{name}.so")) {
                panic!(
                    "couldn't load {name}.so ({e:?}). Build with `anchor build`, not a bare `cargo-build-sbf` at the root: \
                     building everything at once strips the hub's entry point."
                );
            }
        }
        let admin = Keypair::new();
        let creator = Keypair::new();
        svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
        svm.airdrop(&creator.pubkey(), 100_000_000_000).unwrap();
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = START;
        svm.set_sysvar(&clock);
        let mint = if token_program == litesvm_token::TOKEN_ID {
            CreateMint::new(&mut svm, &creator).decimals(6).send().unwrap()
        } else {
            create_mint_2022(&mut svm, &creator)
        };
        let mut env = Env { svm, admin, creator, mint, token_program, dao: Pubkey::default(), treasury: Pubkey::default() };

        // The hub and its approved models.
        let admin_kp = env.admin.insecure_clone();
        env.send(&[env.ix_init_hub()], &[&admin_kp]).unwrap();
        env.send(
            &[
                env.ix_set_model(TW, "token-weighted", true),
                env.ix_set_model(QV, "quadratic", true),
                env.ix_set_model(OPT, "optimistic", true),
                env.ix_set_model(BOARD, "board", true),
                env.ix_set_model(CONV, "conviction", true),
                env.ix_set_model(DEL, "delegate", true),
            ],
            &[&admin_kp],
        )
        .unwrap();
        env
    }

    /// Creates the DAO in the hub and, in the same transaction, the
    /// creator's setup in `gp` (built by `init` once the DAO's address is known).
    pub fn create_dao(&mut self, name: &str, gp: Pubkey, init: impl FnOnce(&Env) -> Instruction) {
        let create_key = Keypair::new();
        self.dao = Pubkey::find_program_address(&[hub::DAO_SEED, create_key.pubkey().as_ref()], &hub::ID).0;
        self.treasury = hub::treasury_address(&self.dao);
        let creator_kp = self.creator.insecure_clone();
        let create = self.ix_create_dao(name, gp, &create_key.pubkey());
        let init = init(self);
        self.send(&[create, init], &[&creator_kp, &create_key]).unwrap();
    }

    pub fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), FailedTransactionMetadata> {
        let fee_payer = signers[0].pubkey();
        let tx = Transaction::new(signers, Message::new(ixs, Some(&fee_payer)), self.svm.latest_blockhash());
        let res = self.svm.send_transaction(tx).map(|_| ());
        self.svm.expire_blockhash();
        res
    }

    pub fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }

    pub fn warp(&mut self, to: i64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = to;
        self.svm.set_sysvar(&clock);
    }

    // ---- addresses ----

    pub fn hub_pda() -> Pubkey {
        Pubkey::find_program_address(&[hub::HUB_SEED], &hub::ID).0
    }

    pub fn model_pda(program: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[hub::MODEL_SEED, program.as_ref()], &hub::ID).0
    }

    pub fn governance(&self, gp: Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[GOVERNANCE_SEED, self.dao.as_ref()], &gp).0
    }

    pub fn vault(&self, gp: Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[tw::VAULT_SEED, self.governance(gp).as_ref()], &gp).0
    }

    pub fn voter_pda(&self, gp: Pubkey, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[tw::VOTER_SEED, self.governance(gp).as_ref(), owner.as_ref()], &gp).0
    }

    pub fn proposal_pda(&self, gp: Pubkey, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[tw::PROPOSAL_SEED, self.governance(gp).as_ref(), &id.to_le_bytes()], &gp).0
    }

    pub fn vote_pda(&self, gp: Pubkey, proposal: &Pubkey, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[tw::VOTE_SEED, proposal.as_ref(), owner.as_ref()], &gp).0
    }

    // ---- hub instructions ----

    pub fn ix_init_hub(&self) -> Instruction {
        Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::InitHub { admin: self.admin.pubkey(), hub: Self::hub_pda(), system_program: SYSTEM }.to_account_metas(None),
            data: hub::instruction::InitHub {}.data(),
        }
    }

    pub fn ix_set_model(&self, program: Pubkey, name: &str, enabled: bool) -> Instruction {
        Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::SetModel { admin: self.admin.pubkey(), hub: Self::hub_pda(), model: Self::model_pda(&program), system_program: SYSTEM }
                .to_account_metas(None),
            data: hub::instruction::SetModel { program_id: program, name: name.into(), enabled }.data(),
        }
    }

    pub fn ix_create_dao(&self, name: &str, gp: Pubkey, create_key: &Pubkey) -> Instruction {
        Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::CreateDao {
                creator: self.creator.pubkey(),
                create_key: *create_key,
                model: Self::model_pda(&gp),
                dao: Pubkey::find_program_address(&[hub::DAO_SEED, create_key.as_ref()], &hub::ID).0,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: hub::instruction::CreateDao { name: name.into(), governance_program: gp }.data(),
        }
    }

    pub fn ix_propose_switch(&self, to: Pubkey) -> Instruction {
        Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::ProposeSwitch { treasury: self.treasury, dao: self.dao, model: Self::model_pda(&to) }.to_account_metas(None),
            data: hub::instruction::ProposeSwitch { new_program: to }.data(),
        }
    }

    pub fn ix_cancel_switch(&self) -> Instruction {
        Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::TreasuryOnly { treasury: self.treasury, dao: self.dao }.to_account_metas(None),
            data: hub::instruction::CancelSwitch {}.data(),
        }
    }

    pub fn apply_switch(&mut self, to: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::ApplySwitch { dao: self.dao, model: Self::model_pda(&to), new_governance: self.governance(to) }.to_account_metas(None),
            data: hub::instruction::ApplySwitch {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    /// Runs a proposal through the hub, passing every account its instructions use.
    pub fn execute(&mut self, gp: Pubkey, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let p = self.core(proposal);
        let mut accounts = hub::accounts::Execute {
            dao: self.dao,
            executor: hub::executor_address(&self.dao),
            governance_program: gp,
            governance: self.governance(gp),
            proposal,
        }
        .to_account_metas(None);
        for ix in &p.instructions {
            for m in &ix.accounts {
                // The treasury signs inside the hub, not in the transaction.
                accounts.push(AccountMeta { pubkey: m.pubkey, is_signer: false, is_writable: m.is_writable });
            }
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
        }
        let ix = Instruction { program_id: hub::ID, accounts, data: hub::instruction::Execute {}.data() };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    // ---- voting-program instructions (same layout in both programs) ----

    pub fn ix_init_governance(&self, gp: Pubkey, authority: &Pubkey, payer: &Pubkey, config: VotingConfig) -> Instruction {
        Instruction {
            program_id: gp,
            accounts: tw::accounts::InitGovernance {
                authority: *authority,
                payer: *payer,
                hub_dao: self.dao,
                governance: self.governance(gp),
                mint: self.mint,
                vault: self.vault(gp),
                token_program: self.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: tw::instruction::InitGovernance { config }.data(),
        }
    }

    pub fn member(&mut self, tokens: u64) -> Member {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        let ata = if self.token_program == litesvm_token::TOKEN_ID {
            let creator = self.creator.insecure_clone();
            let ata = CreateAssociatedTokenAccount::new(&mut self.svm, &creator, &self.mint).owner(&kp.pubkey()).send().unwrap();
            if tokens > 0 {
                MintTo::new(&mut self.svm, &creator, &self.mint, &ata, tokens).send().unwrap();
            }
            ata
        } else {
            self.ata_2022(&kp.pubkey(), tokens)
        };
        Member { kp, ata }
    }

    pub fn deposit(&mut self, gp: Pubkey, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::Deposit {
                owner: m.kp.pubkey(),
                governance: self.governance(gp),
                voter: self.voter_pda(gp, &m.kp.pubkey()),
                owner_token_account: m.ata,
                vault: self.vault(gp),
                mint: self.mint,
                token_program: self.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: tw::instruction::Deposit { amount }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn withdraw(&mut self, gp: Pubkey, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::Withdraw {
                owner: m.kp.pubkey(),
                governance: self.governance(gp),
                voter: self.voter_pda(gp, &m.kp.pubkey()),
                owner_token_account: m.ata,
                vault: self.vault(gp),
                mint: self.mint,
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: tw::instruction::Withdraw { amount }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn propose(&mut self, gp: Pubkey, m: &Member, instructions: Vec<StoredInstruction>) -> Result<(u64, Pubkey), FailedTransactionMetadata> {
        let id = self.proposal_count(gp) + 1;
        let proposal = self.proposal_pda(gp, id);
        let voter = self.voter_pda(gp, &m.kp.pubkey());
        let has_voter = self.svm.get_account(&voter).is_some_and(|a| a.lamports > 0);
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::Propose {
                proposer: m.kp.pubkey(),
                governance: self.governance(gp),
                hub_dao: self.dao,
                voter: has_voter.then_some(voter),
                proposal,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: tw::instruction::Propose { id, metadata_uri: "Pay the community call host".into(), instructions }.data(),
        };
        // Anchor marks a missing optional account with the program's own ID;
        // these are token-weighted's types, so point that at `gp`.
        let mut ix = ix;
        if !has_voter {
            ix.accounts[3].pubkey = gp;
        }
        self.send(&[ix], &[&m.kp]).map(|_| (id, proposal))
    }

    pub fn vote(&mut self, gp: Pubkey, m: &Member, proposal: Pubkey, choice: VoteChoice) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::CastVote {
                owner: m.kp.pubkey(),
                governance: self.governance(gp),
                proposal,
                voter: self.voter_pda(gp, &m.kp.pubkey()),
                vote_record: self.vote_pda(gp, &proposal, &m.kp.pubkey()),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: tw::instruction::CastVote { choice }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn queue(&mut self, gp: Pubkey, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::Queue { governance: self.governance(gp), proposal }.to_account_metas(None),
            data: tw::instruction::Queue {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn cancel(&mut self, gp: Pubkey, who: &Keypair, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: gp,
            accounts: tw::accounts::Cancel { authority: who.pubkey(), governance: self.governance(gp), proposal }.to_account_metas(None),
            data: tw::instruction::Cancel {}.data(),
        };
        self.send(&[ix], &[who])
    }

    /// Propose, vote it through with `voter`, queue, and wait out the
    /// timelock. Returns the proposal and when it became executable.
    pub fn pass(&mut self, gp: Pubkey, voter: &Member, instructions: Vec<StoredInstruction>) -> Pubkey {
        let (_, p) = self.propose(gp, voter, instructions).unwrap();
        let prop = self.proposal_account(p);
        self.warp(prop.voting_starts_at);
        self.vote(gp, voter, p, VoteChoice::For).unwrap();
        self.warp(prop.voting_ends_at);
        self.queue(gp, p).unwrap();
        self.warp(prop.voting_ends_at + config().timelock as i64);
        p
    }

    // ---- reads ----

    /// Any model's proposal's standard prefix.
    pub fn core(&self, proposal: Pubkey) -> vortex_core::ProposalCore {
        vortex_core::ProposalCore::read(&self.svm.get_account(&proposal).unwrap().data).unwrap()
    }

    /// The DAO's proposal count in `gp`, whichever model it is.
    pub fn proposal_count(&self, gp: Pubkey) -> u64 {
        let data = self.svm.get_account(&self.governance(gp)).unwrap().data;
        if gp == OPT {
            vortex_optimistic::Governance::try_deserialize(&mut data.as_slice()).unwrap().proposal_count
        } else if gp == BOARD {
            vortex_board::Governance::try_deserialize(&mut data.as_slice()).unwrap().proposal_count
        } else if gp == CONV {
            vortex_conviction::Governance::try_deserialize(&mut data.as_slice()).unwrap().proposal_count
        } else if gp == DEL {
            vortex_delegate::Governance::try_deserialize(&mut data.as_slice()).unwrap().proposal_count
        } else {
            Governance::try_deserialize(&mut data.as_slice()).unwrap().proposal_count
        }
    }

    pub fn hub_dao(&self) -> hub::Dao {
        hub::Dao::try_deserialize(&mut self.svm.get_account(&self.dao).unwrap().data.as_slice()).unwrap()
    }

    pub fn governance_account(&self, gp: Pubkey) -> Governance {
        Governance::try_deserialize(&mut self.svm.get_account(&self.governance(gp)).unwrap().data.as_slice()).unwrap()
    }

    pub fn proposal_account(&self, p: Pubkey) -> Proposal {
        Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    pub fn voter_account(&self, gp: Pubkey, owner: &Pubkey) -> Voter {
        Voter::try_deserialize(&mut self.svm.get_account(&self.voter_pda(gp, owner)).unwrap().data.as_slice()).unwrap()
    }

    pub fn token_balance(&self, account: &Pubkey) -> u64 {
        litesvm_token::get_spl_account::<litesvm_token::spl_token::state::Account>(&self.svm, account).unwrap().amount
    }

    pub fn lamports(&self, account: &Pubkey) -> u64 {
        self.svm.get_account(account).map_or(0, |a| a.lamports)
    }

    pub fn state(&self, gp: Pubkey, proposal: Pubkey) -> ProposalState {
        self.proposal_account(proposal).state(&self.governance_account(gp).config, self.now())
    }

    // ---- stored instructions ----

    pub fn pay_sol(&self, to: &Pubkey, lamports: u64) -> StoredInstruction {
        stored(solana_system_interface::instruction::transfer(&self.treasury, to, lamports))
    }

    pub fn update_config_ix(&self, gp: Pubkey, config: VotingConfig) -> StoredInstruction {
        stored(Instruction {
            program_id: gp,
            accounts: tw::accounts::UpdateConfig { treasury: self.treasury, governance: self.governance(gp) }.to_account_metas(None),
            data: tw::instruction::UpdateConfig { config }.data(),
        })
    }

    /// `owner`'s Token-2022 associated account, holding `tokens`.
    pub fn ata_2022(&mut self, owner: &Pubkey, tokens: u64) -> Pubkey {
        use spl_token_2022_interface::{instruction::mint_to_checked, ID};
        let ata = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, &self.mint, &ID);
        let mut ixs = vec![spl_associated_token_account_interface::instruction::create_associated_token_account(&self.creator.pubkey(), owner, &self.mint, &ID)];
        if tokens > 0 {
            ixs.push(mint_to_checked(&ID, &self.mint, &ata, &self.creator.pubkey(), &[], tokens, 6).unwrap());
        }
        let creator = self.creator.insecure_clone();
        self.send(&ixs, &[&creator]).unwrap();
        ata
    }
}

/// A Token-2022 mint (no extensions) with `payer` as mint authority.
pub fn create_mint_2022(svm: &mut LiteSVM, payer: &Keypair) -> Pubkey {
    use anchor_lang::solana_program::program_pack::Pack;
    use spl_token_2022_interface::{instruction::initialize_mint2, state::Mint, ID};
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(Mint::LEN);
    let ixs = [
        solana_system_interface::instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, Mint::LEN as u64, &ID),
        initialize_mint2(&ID, &mint.pubkey(), &payer.pubkey(), None, 6).unwrap(),
    ];
    let tx = Transaction::new(&[payer, &mint], Message::new(&ixs, Some(&payer.pubkey())), svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
    mint.pubkey()
}

pub fn stored(ix: Instruction) -> StoredInstruction {
    StoredInstruction {
        program_id: ix.program_id,
        accounts: ix.accounts.iter().map(|m| StoredAccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data,
    }
}

/// The custom error a failed transaction ended with.
pub fn code<T: std::fmt::Debug>(res: Result<T, FailedTransactionMetadata>) -> u32 {
    let failed = res.expect_err("expected the transaction to fail");
    match failed.err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => c,
        other => panic!("not a custom error: {other:?}\n{:#?}", failed.meta.logs),
    }
}

pub fn err(e: GovError) -> u32 {
    u32::from(e)
}

/// Alice 600, Bob 400 deposited in token-weighted; treasury holds 5 SOL.
pub fn dao_with_members() -> (Env, Member, Member) {
    let mut env = Env::new();
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(TW, &alice, 600).unwrap();
    env.deposit(TW, &bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, alice, bob)
}


/// The creator's first-setup instruction for `gp` with default rules
/// (board: the creator as its only signer; delegate: the creator as the
/// whole council).
pub fn env_init(gp: Pubkey) -> impl FnOnce(&Env, &Pubkey) -> Instruction {
    move |env: &Env, creator: &Pubkey| {
        if gp == OPT {
            env.ix_init_optimistic(creator, creator, opt_config())
        } else if gp == BOARD {
            env.ix_init_board(creator, creator, vec![*creator], board_config(1))
        } else if gp == CONV {
            env.ix_init_conviction(creator, creator, conv_config())
        } else if gp == DEL {
            env.ix_init_delegate(creator, creator, vec![*creator], DelegateConfig { council_size: 1, council_quorum: 1, ..del_config() })
        } else {
            env.ix_init_governance(gp, creator, creator, config())
        }
    }
}

// ---------------------------------------------------------------
// Optimistic
// ---------------------------------------------------------------

pub use vortex_optimistic::{self as opt, OptimisticConfig, OptimisticState};

/// 30 s challenge window, 50-token bond; challenge votes like `config()`.
pub fn opt_config() -> OptimisticConfig {
    OptimisticConfig {
        challenge_period: 30,
        challenge_bond: 50,
        quorum_bps: 1_000,
        approval_bps: 6_000,
        voting_period: 100,
        timelock: 50,
        execution_period: 200,
        proposal_threshold: 100,
    }
}

impl Env {
    pub fn opt_bond_vault(&self) -> Pubkey {
        Pubkey::find_program_address(&[opt::BOND_VAULT_SEED, self.governance(OPT).as_ref()], &OPT).0
    }

    pub fn ix_init_optimistic(&self, authority: &Pubkey, payer: &Pubkey, config: OptimisticConfig) -> Instruction {
        Instruction {
            program_id: OPT,
            accounts: opt::accounts::InitGovernance {
                authority: *authority,
                payer: *payer,
                hub_dao: self.dao,
                governance: self.governance(OPT),
                mint: self.mint,
                vault: self.vault(OPT),
                bond_vault: self.opt_bond_vault(),
                token_program: self.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: opt::instruction::InitGovernance { config }.data(),
        }
    }

    pub fn opt_governance(&self) -> opt::Governance {
        opt::Governance::try_deserialize(&mut self.svm.get_account(&self.governance(OPT)).unwrap().data.as_slice()).unwrap()
    }

    pub fn opt_proposal(&self, p: Pubkey) -> opt::Proposal {
        opt::Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    pub fn opt_state(&self, p: Pubkey) -> OptimisticState {
        self.opt_proposal(p).state(&self.opt_governance().config, self.now())
    }

    pub fn challenge(&mut self, m: &Member, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: OPT,
            accounts: opt::accounts::Challenge {
                challenger: m.kp.pubkey(),
                governance: self.governance(OPT),
                proposal: p,
                challenger_token_account: m.ata,
                bond_vault: self.opt_bond_vault(),
                mint: self.mint,
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: opt::instruction::Challenge {}.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn finalize_unchallenged(&mut self, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: OPT,
            accounts: opt::accounts::FinalizeUnchallenged { governance: self.governance(OPT), proposal: p }.to_account_metas(None),
            data: opt::instruction::FinalizeUnchallenged {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn finalize_challenge(&mut self, p: Pubkey, treasury_ata: Option<Pubkey>, challenger_ata: Option<Pubkey>) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: OPT,
            accounts: opt::accounts::FinalizeChallenge {
                governance: self.governance(OPT),
                proposal: p,
                bond_vault: self.opt_bond_vault(),
                treasury_token_account: treasury_ata,
                challenger_token_account: challenger_ata,
                mint: self.mint,
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: opt::instruction::FinalizeChallenge {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn reclaim_bond(&mut self, p: Pubkey, challenger_ata: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: OPT,
            accounts: opt::accounts::ReclaimBond {
                governance: self.governance(OPT),
                proposal: p,
                bond_vault: self.opt_bond_vault(),
                challenger_token_account: challenger_ata,
                mint: self.mint,
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: opt::instruction::ReclaimBond {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    /// The treasury's token account for the DAO's mint (created if missing).
    pub fn treasury_ata(&mut self) -> Pubkey {
        let ata = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&self.treasury, &self.mint, &self.token_program);
        if self.svm.get_account(&ata).is_none() {
            let ix = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                &self.creator.pubkey(),
                &self.treasury,
                &self.mint,
                &self.token_program,
            );
            let creator = self.creator.insecure_clone();
            self.send(&[ix], &[&creator]).unwrap();
        }
        ata
    }
}

// ---------------------------------------------------------------
// Board
// ---------------------------------------------------------------

pub use vortex_board::{self as board, BoardConfig, BoardState};

pub fn board_config(required_approvals: u16) -> BoardConfig {
    BoardConfig { required_approvals, timelock: 50, execution_period: 200 }
}

impl Env {
    pub fn ix_init_board(&self, authority: &Pubkey, payer: &Pubkey, signers: Vec<Pubkey>, config: BoardConfig) -> Instruction {
        Instruction {
            program_id: BOARD,
            accounts: board::accounts::InitGovernance {
                authority: *authority,
                payer: *payer,
                hub_dao: self.dao,
                governance: self.governance(BOARD),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: board::instruction::InitGovernance { signers, config }.data(),
        }
    }

    pub fn board_governance(&self) -> board::Governance {
        board::Governance::try_deserialize(&mut self.svm.get_account(&self.governance(BOARD)).unwrap().data.as_slice()).unwrap()
    }

    pub fn board_proposal(&self, p: Pubkey) -> board::Proposal {
        board::Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    pub fn board_state(&self, p: Pubkey) -> BoardState {
        self.board_proposal(p).state(&self.board_governance().config, self.now())
    }

    pub fn board_propose(&mut self, signer: &Keypair, instructions: Vec<StoredInstruction>) -> Result<Pubkey, FailedTransactionMetadata> {
        let id = self.proposal_count(BOARD) + 1;
        let proposal = self.proposal_pda(BOARD, id);
        let ix = Instruction {
            program_id: BOARD,
            accounts: board::accounts::Propose {
                proposer: signer.pubkey(),
                governance: self.governance(BOARD),
                hub_dao: self.dao,
                proposal,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: board::instruction::Propose { id, metadata_uri: "Pay the auditors".into(), instructions }.data(),
        };
        self.send(&[ix], &[signer]).map(|_| proposal)
    }

    pub fn board_confirm(&mut self, signer: &Keypair, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: BOARD,
            accounts: board::accounts::Confirm { signer: signer.pubkey(), governance: self.governance(BOARD), proposal: p }.to_account_metas(None),
            data: board::instruction::Confirm {}.data(),
        };
        self.send(&[ix], &[signer])
    }

    pub fn board_revoke(&mut self, signer: &Keypair, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: BOARD,
            accounts: board::accounts::Confirm { signer: signer.pubkey(), governance: self.governance(BOARD), proposal: p }.to_account_metas(None),
            data: board::instruction::RevokeConfirmation {}.data(),
        };
        self.send(&[ix], &[signer])
    }

    /// A stored instruction for the board's treasury-only admin calls.
    pub fn board_admin_ix(&self, data: Vec<u8>) -> StoredInstruction {
        stored(Instruction {
            program_id: BOARD,
            accounts: board::accounts::TreasuryOnly { treasury: self.treasury, governance: self.governance(BOARD) }.to_account_metas(None),
            data,
        })
    }
}

// ---------------------------------------------------------------
// Conviction
// ---------------------------------------------------------------

pub use vortex_conviction::{self as conv, AssetAmount, ConvictionConfig, ConvictionState, SOL};

/// Conviction moves 10 per second; the bar is 20% of deposits, at least 100.
pub fn conv_config() -> ConvictionConfig {
    ConvictionConfig { growth_rate: 10, min_conviction: 100, support_bps: 2_000, proposal_threshold: 100, timelock: 50, execution_period: 200 }
}

impl Env {
    /// Conviction setup with SOL and the DAO's token listed at weight 0
    /// (budgets enforced, no extra bar).
    pub fn ix_init_conviction(&self, authority: &Pubkey, payer: &Pubkey, config: ConvictionConfig) -> Instruction {
        self.ix_init_conviction_weighted(authority, payer, config, 0, 0)
    }

    pub fn ix_init_conviction_weighted(&self, authority: &Pubkey, payer: &Pubkey, config: ConvictionConfig, sol_weight: u64, token_weight: u64) -> Instruction {
        Instruction {
            program_id: CONV,
            accounts: conv::accounts::InitGovernance {
                authority: *authority,
                payer: *payer,
                hub_dao: self.dao,
                governance: self.governance(CONV),
                mint: self.mint,
                vault: self.vault(CONV),
                token_program: self.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: conv::instruction::InitGovernance { config, sol_weight, token_weight }.data(),
        }
    }

    pub fn conv_governance(&self) -> conv::Governance {
        conv::Governance::try_deserialize(&mut self.svm.get_account(&self.governance(CONV)).unwrap().data.as_slice()).unwrap()
    }

    pub fn conv_proposal(&self, p: Pubkey) -> conv::Proposal {
        conv::Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    pub fn conv_voter(&self, owner: &Pubkey) -> conv::Voter {
        conv::Voter::try_deserialize(&mut self.svm.get_account(&self.voter_pda(CONV, owner)).unwrap().data.as_slice()).unwrap()
    }

    pub fn conv_state(&self, p: Pubkey) -> ConvictionState {
        self.conv_proposal(p).state(&self.conv_governance().config, self.now())
    }

    /// `p`'s conviction right now.
    pub fn conviction(&self, p: Pubkey) -> u64 {
        self.conv_proposal(p).conviction_at(self.conv_governance().config.growth_rate, self.now())
    }

    /// Proposes with a budget, passing the listed assets' accounts.
    pub fn conv_propose(&mut self, m: &Member, instructions: Vec<StoredInstruction>, budget: Vec<AssetAmount>) -> Result<(u64, Pubkey), FailedTransactionMetadata> {
        let id = self.proposal_count(CONV) + 1;
        let proposal = self.proposal_pda(CONV, id);
        let voter = self.voter_pda(CONV, &m.kp.pubkey());
        let has_voter = self.svm.get_account(&voter).is_some_and(|a| a.lamports > 0);
        let mut accounts = conv::accounts::Propose {
            proposer: m.kp.pubkey(),
            governance: self.governance(CONV),
            hub_dao: self.dao,
            voter: has_voter.then_some(voter),
            proposal,
            system_program: SYSTEM,
        }
        .to_account_metas(None);
        for a in &self.conv_governance().assets {
            accounts.push(AccountMeta::new_readonly(a.account, false));
        }
        let ix = Instruction {
            program_id: CONV,
            accounts,
            data: conv::instruction::Propose { id, metadata_uri: "Fund the community garden".into(), instructions, budget }.data(),
        };
        self.send(&[ix], &[&m.kp]).map(|_| (id, proposal))
    }

    /// Lamports of SOL, as a budget line.
    pub fn sol_budget(lamports: u64) -> AssetAmount {
        AssetAmount { mint: SOL, amount: lamports }
    }

    /// A stored instruction for conviction's treasury-only calls.
    pub fn conv_admin_ix(&self, data: Vec<u8>) -> StoredInstruction {
        stored(Instruction {
            program_id: CONV,
            accounts: conv::accounts::TreasuryOnly { treasury: self.treasury, governance: self.governance(CONV) }.to_account_metas(None),
            data,
        })
    }

    pub fn conv_add_asset_ix(&self, mint: Pubkey, weight: u64) -> StoredInstruction {
        stored(Instruction {
            program_id: CONV,
            accounts: conv::accounts::AddAsset { treasury: self.treasury, governance: self.governance(CONV), mint }.to_account_metas(None),
            data: conv::instruction::AddAsset { weight }.data(),
        })
    }

    pub fn apply_asset_change(&mut self, mint: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: CONV,
            accounts: conv::accounts::ApplyAssetChange { governance: self.governance(CONV) }.to_account_metas(None),
            data: conv::instruction::ApplyAssetChange { mint }.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn support(&mut self, m: &Member, p: Pubkey, previous: Option<Pubkey>) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: CONV,
            accounts: conv::accounts::Support {
                owner: m.kp.pubkey(),
                governance: self.governance(CONV),
                voter: self.voter_pda(CONV, &m.kp.pubkey()),
                proposal: p,
                previous,
            }
            .to_account_metas(None),
            data: conv::instruction::Support {}.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn withdraw_support(&mut self, m: &Member, p: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: CONV,
            accounts: conv::accounts::WithdrawSupport {
                owner: m.kp.pubkey(),
                governance: self.governance(CONV),
                voter: self.voter_pda(CONV, &m.kp.pubkey()),
                proposal: p,
            }
            .to_account_metas(None),
            data: conv::instruction::WithdrawSupport {}.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn conv_update_config_ix(&self, config: ConvictionConfig) -> StoredInstruction {
        self.conv_admin_ix(conv::instruction::UpdateConfig { config }.data())
    }
}

// ---------------------------------------------------------------
// Delegate
// ---------------------------------------------------------------

pub use vortex_delegate::{self as del, DelegateConfig};

/// A 3-seat council, 2 votes and 60% to pass; 1000 s terms; elections:
/// 50 s to declare (100 tokens), 100 s to vote; recalls: 10% quorum, 60%.
pub fn del_config() -> DelegateConfig {
    DelegateConfig {
        council_size: 3,
        term_length: 1_000,
        candidacy_threshold: 100,
        candidacy_period: 50,
        election_voting_period: 100,
        council_quorum: 2,
        council_approval_bps: 6_000,
        voting_delay: 10,
        voting_period: 100,
        timelock: 50,
        execution_period: 200,
        recall_quorum_bps: 1_000,
        recall_approval_bps: 6_000,
        recall_voting_period: 100,
    }
}

impl Env {
    pub fn ix_init_delegate(&self, authority: &Pubkey, payer: &Pubkey, council: Vec<Pubkey>, config: DelegateConfig) -> Instruction {
        Instruction {
            program_id: DEL,
            accounts: del::accounts::InitGovernance {
                authority: *authority,
                payer: *payer,
                hub_dao: self.dao,
                governance: self.governance(DEL),
                mint: self.mint,
                vault: self.vault(DEL),
                token_program: self.token_program,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: del::instruction::InitGovernance { council, config }.data(),
        }
    }

    pub fn del_governance(&self) -> del::Governance {
        del::Governance::try_deserialize(&mut self.svm.get_account(&self.governance(DEL)).unwrap().data.as_slice()).unwrap()
    }

    pub fn del_proposal(&self, p: Pubkey) -> del::Proposal {
        del::Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    pub fn del_state(&self, p: Pubkey) -> ProposalState {
        self.del_proposal(p).state(&self.del_governance(), self.now())
    }

    pub fn election_pda(&self, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[del::ELECTION_SEED, self.governance(DEL).as_ref(), &id.to_le_bytes()], &DEL).0
    }

    pub fn recall_pda(&self, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[del::RECALL_SEED, self.governance(DEL).as_ref(), &id.to_le_bytes()], &DEL).0
    }

    pub fn election(&self, e: Pubkey) -> del::Election {
        del::Election::try_deserialize(&mut self.svm.get_account(&e).unwrap().data.as_slice()).unwrap()
    }

    pub fn recall(&self, r: Pubkey) -> del::Recall {
        del::Recall::try_deserialize(&mut self.svm.get_account(&r).unwrap().data.as_slice()).unwrap()
    }

    pub fn del_propose(&mut self, member: &Keypair, instructions: Vec<StoredInstruction>) -> Result<Pubkey, FailedTransactionMetadata> {
        let id = self.proposal_count(DEL) + 1;
        let proposal = self.proposal_pda(DEL, id);
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::Propose { proposer: member.pubkey(), governance: self.governance(DEL), hub_dao: self.dao, proposal, system_program: SYSTEM }
                .to_account_metas(None),
            data: del::instruction::Propose { id, metadata_uri: "Fund the grants round".into(), instructions }.data(),
        };
        self.send(&[ix], &[member]).map(|_| proposal)
    }

    pub fn council_vote(&mut self, member: &Keypair, p: Pubkey, choice: del::VoteChoice) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::CouncilAction { member: member.pubkey(), governance: self.governance(DEL), proposal: p }.to_account_metas(None),
            data: del::instruction::CastVote { choice }.data(),
        };
        self.send(&[ix], &[member])
    }

    pub fn start_election(&mut self) -> Result<Pubkey, FailedTransactionMetadata> {
        let id = self.del_governance().election_count + 1;
        let election = self.election_pda(id);
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::StartElection { payer: self.admin.pubkey(), governance: self.governance(DEL), election, system_program: SYSTEM }
                .to_account_metas(None),
            data: del::instruction::StartElection { id }.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin]).map(|_| election)
    }

    pub fn declare(&mut self, m: &Member, e: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::DeclareCandidacy {
                candidate: m.kp.pubkey(),
                governance: self.governance(DEL),
                election: e,
                voter: self.voter_pda(DEL, &m.kp.pubkey()),
            }
            .to_account_metas(None),
            data: del::instruction::DeclareCandidacy {}.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn elect(&mut self, m: &Member, e: Pubkey, candidates: Vec<Pubkey>) -> Result<(), FailedTransactionMetadata> {
        let ballot = Pubkey::find_program_address(&[del::BALLOT_SEED, e.as_ref(), m.kp.pubkey().as_ref()], &DEL).0;
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::VoteInElection {
                owner: m.kp.pubkey(),
                governance: self.governance(DEL),
                election: e,
                voter: self.voter_pda(DEL, &m.kp.pubkey()),
                ballot,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: del::instruction::VoteInElection { candidates }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn finalize_election(&mut self, e: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::FinalizeElection { governance: self.governance(DEL), election: e }.to_account_metas(None),
            data: del::instruction::FinalizeElection {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn initiate_recall(&mut self, m: &Member, member: Pubkey) -> Result<Pubkey, FailedTransactionMetadata> {
        let id = self.del_governance().recall_count + 1;
        let recall = self.recall_pda(id);
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::InitiateRecall {
                initiator: m.kp.pubkey(),
                governance: self.governance(DEL),
                voter: self.voter_pda(DEL, &m.kp.pubkey()),
                recall,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: del::instruction::InitiateRecall { id, member }.data(),
        };
        self.send(&[ix], &[&m.kp]).map(|_| recall)
    }

    pub fn vote_recall(&mut self, m: &Member, r: Pubkey, choice: del::VoteChoice) -> Result<(), FailedTransactionMetadata> {
        let record = Pubkey::find_program_address(&[del::RECALL_VOTE_SEED, r.as_ref(), m.kp.pubkey().as_ref()], &DEL).0;
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::VoteRecall {
                owner: m.kp.pubkey(),
                governance: self.governance(DEL),
                recall: r,
                voter: self.voter_pda(DEL, &m.kp.pubkey()),
                recall_vote: record,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: del::instruction::VoteRecall { choice }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    pub fn finalize_recall(&mut self, r: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: DEL,
            accounts: del::accounts::FinalizeRecall { governance: self.governance(DEL), recall: r }.to_account_metas(None),
            data: del::instruction::FinalizeRecall {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    pub fn del_update_config_ix(&self, config: DelegateConfig) -> StoredInstruction {
        stored(Instruction {
            program_id: DEL,
            accounts: del::accounts::UpdateConfig { treasury: self.treasury, governance: self.governance(DEL) }.to_account_metas(None),
            data: del::instruction::UpdateConfig { config }.data(),
        })
    }
}
