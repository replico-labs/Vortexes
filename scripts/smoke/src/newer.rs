//! The four newer models' flows: liquid, sortition (with ORAO randomness),
//! Sowellian (human and Pyth resolution) and decision markets.

use super::*;

/// ORAO VRF: the same address on devnet and mainnet; a local validator
/// runs `scripts/orao-stub` there instead.
const ORAO: Pubkey = vortex_sortition::ORAO_VRF;
const ORAO_STATE_SEED: &[u8] = b"orao-vrf-network-configuration";
const ORAO_NETWORK_STATE: [u8; 8] = [212, 237, 148, 56, 97, 245, 51, 169];
const ORAO_INIT_NETWORK: [u8; 8] = [104, 231, 73, 121, 240, 0, 5, 148];
const ORAO_REQUEST_V2: [u8; 8] = [38, 151, 209, 6, 195, 102, 28, 217];
const ORAO_FULFILL_V2: [u8; 8] = [164, 227, 237, 84, 112, 111, 147, 196];
/// How long to wait for ORAO to answer a draw.
const ORAO_WAIT: u64 = 180;

/// Pyth's SOL/USD feed and the push oracle its sponsored price accounts live under.
const SOL_USD: [u8; 32] = hex32("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d");
const PYTH_PUSH_ORACLE: Pubkey = anchor_lang::pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");
/// How long to wait for Pyth to publish a price after the measurement deadline.
const PYTH_WAIT: i64 = 600;

impl Smoke {
    // ---------------------------------------------------------------
    // liquid
    // ---------------------------------------------------------------

    /// Bob delegates to Alice, Alice votes For with both deposits, then Bob
    /// overrides Against with his own: 600 For, 400 Against, and it passes.
    pub(crate) fn liquid_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        use vortex_liquid as liq;
        let config = voting_config();
        let dao = self.create_dao(mint, LIQ, "Smoke liquid", |d, me| Instruction {
            program_id: LIQ,
            accounts: liq::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(LIQ),
                mint: d.mint,
                vault: d.vault(LIQ),
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: liq::instruction::InitGovernance { config }.data(),
        })?;
        for (m, amount) in [(alice, 600), (bob, 400)] {
            let ix = Instruction {
                program_id: LIQ,
                accounts: liq::accounts::Deposit {
                    owner: m.kp.pubkey(),
                    governance: dao.governance(LIQ),
                    voter: dao.voter(LIQ, &m.kp.pubkey()),
                    delegate_voter: None,
                    owner_token_account: m.ata,
                    vault: dao.vault(LIQ),
                    mint: dao.mint,
                    token_program: spl_token_interface::ID,
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: liq::instruction::Deposit { amount }.data(),
            };
            self.send_ix(ix, &[&m.kp], "deposit")?;
        }
        let (a, b) = (alice.kp.pubkey(), bob.kp.pubkey());
        self.send_ix(
            Instruction {
                program_id: LIQ,
                accounts: liq::accounts::Delegate { owner: b, governance: dao.governance(LIQ), voter: dao.voter(LIQ, &b), delegate_voter: dao.voter(LIQ, &a), system_program: SYSTEM }
                    .to_account_metas(None),
                data: liq::instruction::Delegate { to: a }.data(),
            },
            &[&bob.kp],
            "Bob delegates to Alice",
        )?;
        let to = Keypair::new().pubkey();
        let p = self.propose_voting(&dao, LIQ, alice, vec![dao.pay(&to)])?;
        let prop = read::<liq::Proposal>(&self.rpc, &p);
        self.wait_until(prop.voting_starts_at);
        let vote = |owner: &Pubkey| Pubkey::find_program_address(&[liq::VOTE_SEED, p.as_ref(), owner.as_ref()], &LIQ).0;
        let overrides = |delegate: &Pubkey| Pubkey::find_program_address(&[liq::OVERRIDE_SEED, p.as_ref(), delegate.as_ref()], &LIQ).0;
        self.send_ix(
            Instruction {
                program_id: LIQ,
                accounts: liq::accounts::CastVote {
                    owner: a,
                    governance: dao.governance(LIQ),
                    proposal: p,
                    voter: dao.voter(LIQ, &a),
                    overrides: overrides(&a),
                    vote_record: vote(&a),
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: liq::instruction::CastVote { choice: liq::VoteChoice::For }.data(),
            },
            &[&alice.kp],
            "Alice votes For with 1000",
        )?;
        self.send_ix(
            Instruction {
                program_id: LIQ,
                accounts: liq::accounts::OverrideVote {
                    owner: b,
                    governance: dao.governance(LIQ),
                    proposal: p,
                    voter: dao.voter(LIQ, &b),
                    delegate_voter: dao.voter(LIQ, &a),
                    delegate_record: vote(&a),
                    overrides: overrides(&a),
                    vote_record: vote(&b),
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: liq::instruction::OverrideVote { choice: liq::VoteChoice::Against }.data(),
            },
            &[&bob.kp],
            "Bob overrides Against with his 400",
        )?;
        let tally = read::<liq::Proposal>(&self.rpc, &p).tally;
        if (tally.for_votes, tally.against_votes) != (600, 400) {
            return Err(format!("tally {} for / {} against, expected 600 / 400", tally.for_votes, tally.against_votes));
        }
        println!("  tally 600 for / 400 against, as expected");
        self.wait_until(prop.voting_ends_at);
        self.queue(&dao, LIQ, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, LIQ, p)?;
        self.expect_paid(&to)
    }

    // ---------------------------------------------------------------
    // sortition
    // ---------------------------------------------------------------

    /// Three members join the pool; once the first term ends, a draw with
    /// ORAO's randomness seats two of them, who pass a proposal.
    pub(crate) fn sortition_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        use vortex_sortition as sort;
        let config = sort::SortitionConfig {
            council_size: 2,
            term_length: 10,
            eligibility_threshold: 100,
            council_quorum: 2,
            council_approval_bps: 5_000,
            voting_delay: 0,
            voting_period: 20,
            timelock: 5,
            execution_period: 3_600,
            randomness_timeout: 300,
        };
        let pool_of = |d: &Dao| Pubkey::find_program_address(&[sort::POOL_SEED, d.governance(SORT).as_ref()], &SORT).0;
        let council = vec![alice.kp.pubkey(), bob.kp.pubkey()];
        let dao = self.create_dao(mint, SORT, "Smoke sortition", |d, me| Instruction {
            program_id: SORT,
            accounts: sort::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(SORT),
                pool: pool_of(d),
                mint: d.mint,
                vault: d.vault(SORT),
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: sort::instruction::InitGovernance { council, config }.data(),
        })?;
        let pool = pool_of(&dao);
        let carol = self.member(mint);
        let members = [alice, bob, &carol];
        for m in members {
            self.deposit(&dao, SORT, m, 100)?;
            self.send_ix(
                Instruction {
                    program_id: SORT,
                    accounts: sort::accounts::PoolAction { owner: m.kp.pubkey(), governance: dao.governance(SORT), pool, voter: dao.voter(SORT, &m.kp.pubkey()) }.to_account_metas(None),
                    data: sort::instruction::JoinPool {}.data(),
                },
                &[&m.kp],
                "join pool",
            )?;
        }

        let g = read::<sort::Governance>(&self.rpc, &dao.governance(SORT));
        self.wait_until(g.term_ends_at);
        let orao = self.orao_network()?;
        let nonce: [u8; 32] = Keypair::new().pubkey().to_bytes();
        let seed = sort::draw_seed(&dao.governance(SORT), g.draw_count + 1, &nonce);
        let request = orao_request(&seed);
        let ask = Instruction {
            program_id: ORAO,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new(orao.state, false),
                AccountMeta::new(orao.treasury, false),
                AccountMeta::new(request, false),
                AccountMeta::new_readonly(SYSTEM, false),
            ],
            data: [ORAO_REQUEST_V2.as_slice(), &seed].concat(),
        };
        let start = Instruction {
            program_id: SORT,
            accounts: sort::accounts::StartDraw { governance: dao.governance(SORT), pool, randomness: request, previous_randomness: request }.to_account_metas(None),
            data: sort::instruction::StartDraw { nonce }.data(),
        };
        let sig = self.send(&[ask, start], &[]).map_err(|e| format!("start draw: {e}"))?;
        println!("  draw started, ORAO asked for randomness ({sig})");
        self.await_randomness(&orao, request)?;
        self.send_ix(
            Instruction {
                program_id: SORT,
                accounts: sort::accounts::FinalizeDraw { governance: dao.governance(SORT), pool, randomness: request }.to_account_metas(None),
                data: sort::instruction::FinalizeDraw {}.data(),
            },
            &[],
            "finalize draw",
        )?;
        let drawn = read::<sort::Governance>(&self.rpc, &dao.governance(SORT)).council;
        let seated: Vec<&Member> = drawn.iter().filter_map(|k| members.iter().copied().find(|m| m.kp.pubkey() == *k)).collect();
        if seated.len() != 2 {
            return Err(format!("drew {} seats from the pool, expected 2", seated.len()));
        }
        let name = |k: &Pubkey| ["Alice", "Bob", "Carol"][members.iter().position(|m| m.kp.pubkey() == *k).unwrap()];
        println!("  council drawn: {} and {}", name(&drawn[0]), name(&drawn[1]));

        let to = Keypair::new().pubkey();
        let p = dao.proposal(SORT, 1);
        self.send_ix(
            Instruction {
                program_id: SORT,
                accounts: sort::accounts::Propose { proposer: seated[0].kp.pubkey(), governance: dao.governance(SORT), hub_dao: dao.dao, proposal: p, system_program: SYSTEM }
                    .to_account_metas(None),
                data: sort::instruction::Propose { id: 1, metadata_uri: "Smoke test payment".into(), instructions: vec![dao.pay(&to)] }.data(),
            },
            &[&seated[0].kp],
            "propose",
        )?;
        for m in &seated {
            self.send_ix(
                Instruction {
                    program_id: SORT,
                    accounts: sort::accounts::CouncilAction { member: m.kp.pubkey(), governance: dao.governance(SORT), proposal: p }.to_account_metas(None),
                    data: sort::instruction::CastVote { choice: sort::VoteChoice::For }.data(),
                },
                &[&m.kp],
                "council vote",
            )?;
        }
        self.wait_until(read::<sort::Proposal>(&self.rpc, &p).voting_ends_at);
        self.queue(&dao, SORT, p)?;
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, SORT, p)?;
        self.expect_paid(&to)
    }

    /// ORAO's network state and fee treasury. On a local validator running
    /// the stand-in, sets the network up first with this wallet answering
    /// requests.
    fn orao_network(&mut self) -> Result<Orao, String> {
        let state = Pubkey::find_program_address(&[ORAO_STATE_SEED], &ORAO).0;
        if self.rpc.get_account(&state).is_err() {
            // Only ORAO's stand-in accepts this; real ORAO is already set up.
            let me = self.payer.pubkey();
            let mut data = ORAO_INIT_NETWORK.to_vec();
            data.extend_from_slice(&0u64.to_le_bytes()); // fee
            data.extend_from_slice(me.as_ref()); // config authority
            data.extend_from_slice(&1u32.to_le_bytes());
            data.extend_from_slice(me.as_ref()); // the one fulfillment authority
            data.push(0); // no token fee
            let ix = Instruction {
                program_id: ORAO,
                accounts: vec![AccountMeta::new(me, true), AccountMeta::new(state, false), AccountMeta::new_readonly(me, false), AccountMeta::new_readonly(SYSTEM, false)],
                data,
            };
            self.send(&[ix], &[]).map_err(|e| format!("ORAO VRF isn't set up on this cluster ({e})"))?;
            println!("  set up the local ORAO stand-in, answered by this wallet");
        }
        let data = self.rpc.get_account_data(&state).map_err(|e| e.to_string())?;
        if data.len() < 84 || data[..8] != ORAO_NETWORK_STATE {
            return Err("ORAO's network state isn't in the expected layout".into());
        }
        // NetworkState: config { authority, treasury, request_fee, fulfillment_authorities, .. }
        let treasury = Pubkey::try_from(&data[40..72]).unwrap();
        let fee = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(data[80..84].try_into().unwrap()) as usize;
        let me = self.payer.pubkey();
        let answers_here = (0..n).any(|i| data.get(84 + 32 * i..116 + 32 * i) == Some(me.as_ref()));
        println!("  ORAO request fee {fee} lamports");
        Ok(Orao { state, treasury, answers_here })
    }

    /// Waits for ORAO to answer `request`. Where this wallet is the
    /// fulfillment authority (the local stand-in), answers it itself.
    fn await_randomness(&mut self, orao: &Orao, request: Pubkey) -> Result<(), String> {
        let started = std::time::Instant::now();
        let mut answered_here = false;
        loop {
            let data = self.rpc.get_account_data(&request).unwrap_or_default();
            if data.get(8) == Some(&1) {
                println!("  ORAO answered after {}s", started.elapsed().as_secs());
                return Ok(());
            }
            if orao.answers_here && !answered_here && started.elapsed().as_secs() >= 4 {
                let ix = Instruction {
                    program_id: ORAO,
                    accounts: vec![
                        AccountMeta::new(self.payer.pubkey(), true),
                        AccountMeta::new_readonly(solana_sdk_ids::sysvar::instructions::ID, false),
                        AccountMeta::new(orao.state, false),
                        AccountMeta::new(request, false),
                        AccountMeta::new(self.payer.pubkey(), false),
                        AccountMeta::new_readonly(SYSTEM, false),
                    ],
                    data: ORAO_FULFILL_V2.to_vec(),
                };
                self.send(&[ix], &[]).map_err(|e| format!("answering the request locally: {e}"))?;
                answered_here = true;
            }
            if started.elapsed().as_secs() > ORAO_WAIT {
                return Err(format!("ORAO hasn't answered request {request} after {ORAO_WAIT}s"));
            }
            sleep(Duration::from_secs(2));
        }
    }

    // ---------------------------------------------------------------
    // sowellian
    // ---------------------------------------------------------------

    /// Two proposals, both approved and run: #1 on the human track, with
    /// YES and NO positions, resolved Success unchallenged and paid out; #2
    /// on the oracle track (SOL/USD at or above 0, so always Success),
    /// resolved from Pyth's sponsored price account.
    pub(crate) fn sowellian_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        use vortex_sowellian as sow;
        let config = sow::SowellianConfig {
            proposal_bond: 10,
            approval_voting_delay: 0,
            approval_voting_period: 20,
            approval_quorum_bps: 1_000,
            approval_threshold_bps: 5_000,
            positions_window: 15,
            execution_timelock: 5,
            execution_period: 3_600,
            resolution_bond: 10,
            challenge_period: 15,
            challenge_bond: 10,
            adjudication_voting_period: 20,
            adjudication_quorum_bps: 1_000,
            adjudication_threshold_bps: 5_000,
            max_oracle_staleness: PYTH_WAIT as u32,
        };
        let escrow_of = |d: &Dao| Pubkey::find_program_address(&[sow::ESCROW_SEED, d.governance(SOW).as_ref()], &SOW).0;
        let dao = self.create_dao(mint, SOW, "Smoke sowellian", |d, me| Instruction {
            program_id: SOW,
            accounts: sow::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(SOW),
                mint: d.mint,
                vault: d.vault(SOW),
                escrow: escrow_of(d),
                token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: sow::instruction::InitGovernance { config }.data(),
        })?;
        let (g, escrow) = (dao.governance(SOW), escrow_of(&dao));
        self.deposit(&dao, SOW, alice, 600)?;
        self.deposit(&dao, SOW, bob, 400)?;

        let price_account = pyth_price_account(&SOL_USD);
        let oracle = self.rpc.get_account(&price_account).ok().filter(|a| a.owner == sow::PYTH_RECEIVER).is_some();
        if !oracle {
            println!("  no Pyth SOL/USD price account at {price_account}: the oracle track is left out");
        }
        let human = sow::Criteria { method: sow::ResolutionMethod::Human, feed_id: [0; 32], target_value: 0, target_is_minimum: true, measurement_period: 5 };
        let by_pyth = sow::Criteria { method: sow::ResolutionMethod::Oracle, feed_id: SOL_USD, target_value: 0, target_is_minimum: true, measurement_period: 5 };
        let mut proposals = vec![];
        for (id, criteria) in [(1u64, human), (2, by_pyth)].into_iter().take(if oracle { 2 } else { 1 }) {
            let to = Keypair::new().pubkey();
            let p = dao.proposal(SOW, id);
            let ix = Instruction {
                program_id: SOW,
                accounts: sow::accounts::Propose {
                    proposer: alice.kp.pubkey(),
                    governance: g,
                    hub_dao: dao.dao,
                    proposal: p,
                    proposer_token_account: alice.ata,
                    escrow,
                    mint,
                    token_program: spl_token_interface::ID,
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: sow::instruction::Propose { id, metadata_uri: "Smoke test payment".into(), instructions: vec![dao.pay(&to)], criteria }.data(),
            };
            self.send_ix(ix, &[&alice.kp], if id == 1 { "propose #1 (human track)" } else { "propose #2 (Pyth track)" })?;
            proposals.push((p, to));
        }

        // Approval votes, then the positions window.
        let first = read::<sow::Proposal>(&self.rpc, &proposals[0].0);
        self.wait_until(first.approval_starts_at);
        for &(p, _) in &proposals {
            for m in [alice, bob] {
                let ix = Instruction {
                    program_id: SOW,
                    accounts: sow::accounts::CastApprovalVote {
                        owner: m.kp.pubkey(),
                        governance: g,
                        proposal: p,
                        voter: dao.voter(SOW, &m.kp.pubkey()),
                        vote_record: Pubkey::find_program_address(&[sow::APPROVAL_VOTE_SEED, p.as_ref(), m.kp.pubkey().as_ref()], &SOW).0,
                        system_program: SYSTEM,
                    }
                    .to_account_metas(None),
                    data: sow::instruction::CastApprovalVote { choice: sow::VoteChoice::For }.data(),
                };
                self.send_ix(ix, &[&m.kp], "approval vote")?;
            }
        }
        let last = read::<sow::Proposal>(&self.rpc, &proposals.last().unwrap().0);
        self.wait_until(last.approval_ends_at);
        for &(p, _) in &proposals {
            self.sow_settle(g, p, sow::instruction::FinalizeApproval {}.data(), "finalize approval")?;
            let phase = read::<sow::Proposal>(&self.rpc, &p).phase;
            if phase != sow::Phase::PositionsOpen {
                return Err(format!("after the approval vote the proposal is {phase:?}, expected PositionsOpen"));
            }
        }
        let p1 = proposals[0].0;
        let position_of = |p: &Pubkey, owner: &Pubkey| Pubkey::find_program_address(&[sow::POSITION_SEED, p.as_ref(), owner.as_ref()], &SOW).0;
        for (m, side, amount, what) in [(alice, sow::Side::Yes, 100, "Alice backs YES with 100"), (bob, sow::Side::No, 50, "Bob backs NO with 50")] {
            let ix = Instruction {
                program_id: SOW,
                accounts: sow::accounts::TakePosition {
                    owner: m.kp.pubkey(),
                    governance: g,
                    proposal: p1,
                    position: position_of(&p1, &m.kp.pubkey()),
                    owner_token_account: m.ata,
                    escrow,
                    mint,
                    token_program: spl_token_interface::ID,
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: sow::instruction::TakePosition { side, amount }.data(),
            };
            self.send_ix(ix, &[&m.kp], what)?;
        }

        // Run both, then wait out the measurement period.
        let positions_deadline = proposals.iter().map(|(p, _)| read::<sow::Proposal>(&self.rpc, p).positions_deadline).max().unwrap();
        self.wait_until(positions_deadline + config.execution_timelock as i64);
        for &(p, to) in &proposals {
            self.execute(&dao, SOW, p)?;
            self.expect_paid(&to)?;
        }
        let deadline = proposals.iter().map(|(p, _)| read::<sow::Proposal>(&self.rpc, p).measurement_deadline).max().unwrap();
        self.wait_until(deadline);

        // #1: Alice proposes Success; nobody challenges.
        let bond = |m: &Member, p: Pubkey| sow::accounts::BondAction { owner: m.kp.pubkey(), governance: g, proposal: p, owner_token_account: m.ata, escrow, mint, token_program: spl_token_interface::ID };
        self.send_ix(
            Instruction { program_id: SOW, accounts: bond(alice, p1).to_account_metas(None), data: sow::instruction::ProposeResolution { outcome: sow::Outcome::Success }.data() },
            &[&alice.kp],
            "propose resolution: Success",
        )?;
        self.wait_until(read::<sow::Proposal>(&self.rpc, &p1).challenge_deadline);
        self.sow_settle(g, p1, sow::instruction::FinalizeUnchallenged {}.data(), "finalize unchallenged")?;
        let before = self.token_balance(&alice.ata);
        self.send_ix(
            Instruction {
                program_id: SOW,
                accounts: sow::accounts::ClaimPosition {
                    owner: alice.kp.pubkey(),
                    governance: g,
                    proposal: p1,
                    position: position_of(&p1, &alice.kp.pubkey()),
                    owner_token_account: alice.ata,
                    escrow,
                    mint,
                    token_program: spl_token_interface::ID,
                }
                .to_account_metas(None),
                data: sow::instruction::ClaimPosition {}.data(),
            },
            &[&alice.kp],
            "Alice claims her YES position",
        )?;
        let claim_bond = |p: Pubkey| Instruction {
            program_id: SOW,
            accounts: sow::accounts::ClaimBond { owner: alice.kp.pubkey(), governance: g, proposal: p, owner_token_account: alice.ata, escrow, mint, token_program: spl_token_interface::ID }
                .to_account_metas(None),
            data: sow::instruction::ClaimBond {}.data(),
        };
        self.send_ix(claim_bond(p1), &[&alice.kp], "Alice takes back her proposal and resolution bonds")?;
        let got = self.token_balance(&alice.ata).saturating_sub(before);
        // The whole pool (100 + 50), plus both bonds (10 + 10).
        if got != 170 {
            return Err(format!("Alice got {got} back from #1, expected 170"));
        }
        println!("  #1 resolved Success; Alice got the pool (150) and her bonds (20) back");

        // #2: Pyth decides.
        if oracle {
            let p2 = proposals[1].0;
            let measured = read::<sow::Proposal>(&self.rpc, &p2).measurement_deadline;
            self.await_pyth(&price_account, measured)?;
            self.send_ix(
                Instruction {
                    program_id: SOW,
                    accounts: sow::accounts::ResolveViaOracle { governance: g, proposal: p2, price_update: price_account }.to_account_metas(None),
                    data: sow::instruction::ResolveViaOracle {}.data(),
                },
                &[],
                "resolve #2 from Pyth",
            )?;
            let outcome = read::<sow::Proposal>(&self.rpc, &p2).final_outcome;
            if outcome != sow::Outcome::Success {
                return Err(format!("#2 resolved {outcome:?}, expected Success (SOL/USD is above 0)"));
            }
            let before = self.token_balance(&alice.ata);
            self.send_ix(claim_bond(p2), &[&alice.kp], "Alice takes back her proposal bond")?;
            if self.token_balance(&alice.ata).saturating_sub(before) != 10 {
                return Err("Alice didn't get her 10-token bond back from #2".into());
            }
            println!("  #2 resolved Success from Pyth's SOL/USD price");
        }
        Ok(())
    }

    fn sow_settle(&mut self, g: Pubkey, p: Pubkey, data: Vec<u8>, what: &str) -> Result<(), String> {
        let ix = Instruction { program_id: SOW, accounts: vortex_sowellian::accounts::Settle { governance: g, proposal: p }.to_account_metas(None), data };
        self.send_ix(ix, &[], what).map(|_| ())
    }

    /// Waits until Pyth's price account holds a price published at or after `t`.
    fn await_pyth(&self, account: &Pubkey, t: i64) -> Result<(), String> {
        let started = self.now();
        let mut shown = false;
        loop {
            let data = self.rpc.get_account_data(account).map_err(|e| e.to_string())?;
            // PriceUpdateV2: disc 8, write authority 32, verification 1, then feed id 32, price 8, conf 8, exponent 4, publish_time 8.
            let published = data.get(93..101).map(|b| i64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
            if published >= t {
                return Ok(());
            }
            if !shown {
                println!("  waiting for a Pyth price published after the measurement deadline (latest is {}s older)", t - published);
                shown = true;
            }
            if self.now() - started > PYTH_WAIT {
                return Err(format!("Pyth hasn't published a newer SOL/USD price into {account} in {PYTH_WAIT}s"));
            }
            sleep(Duration::from_secs(5));
        }
    }

    // ---------------------------------------------------------------
    // decision markets
    // ---------------------------------------------------------------

    /// Alice proposes and seeds both markets. Bob splits, buys in the pass
    /// market and sells in the fail market, so pass trades higher. It
    /// passes and runs; Bob redeems his pass tokens and Alice takes her
    /// liquidity back.
    pub(crate) fn decision_markets_flow(&mut self, mint: Pubkey, alice: &Member, bob: &Member) -> Result<(), String> {
        use vortex_decision_markets as dm;
        let config = dm::MarketsConfig { trading_period: 30, threshold_bps: 100, timelock: 5, execution_period: 3_600, min_base_seed: 1, min_quote_seed: 1 };
        // A second token for the markets to price the DAO's token in.
        let quote = self.create_mint();
        let quote_ata = |m: &Member| spl_associated_token_account_interface::address::get_associated_token_address(&m.kp.pubkey(), &quote);
        for m in [alice, bob] {
            let ixs = [
                spl_associated_token_account_interface::instruction::create_associated_token_account(&self.payer.pubkey(), &m.kp.pubkey(), &quote, &spl_token_interface::ID),
                spl_token_interface::instruction::mint_to(&spl_token_interface::ID, &quote, &quote_ata(m), &self.payer.pubkey(), &[], 1_000).unwrap(),
            ];
            self.send(&ixs, &[]).map_err(|e| format!("quote tokens: {e}"))?;
        }
        let vault = |d: &Dao, seed: &[u8]| Pubkey::find_program_address(&[seed, d.governance(DM).as_ref()], &DM).0;
        let dao = self.create_dao(mint, DM, "Smoke markets", |d, me| Instruction {
            program_id: DM,
            accounts: dm::accounts::InitGovernance {
                authority: *me,
                payer: *me,
                hub_dao: d.dao,
                governance: d.governance(DM),
                base_mint: d.mint,
                quote_mint: quote,
                base_vault: vault(d, dm::BASE_VAULT_SEED),
                quote_vault: vault(d, dm::QUOTE_VAULT_SEED),
                base_token_program: spl_token_interface::ID,
                quote_token_program: spl_token_interface::ID,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: dm::instruction::InitGovernance { config }.data(),
        })?;
        let g = dao.governance(DM);
        let tokens = || dm::accounts::Tokens {
            governance: g,
            base_vault: vault(&dao, dm::BASE_VAULT_SEED),
            quote_vault: vault(&dao, dm::QUOTE_VAULT_SEED),
            base_mint: mint,
            quote_mint: quote,
            base_token_program: spl_token_interface::ID,
            quote_token_program: spl_token_interface::ID,
        };
        let to = Keypair::new().pubkey();
        let p = dao.proposal(DM, 1);
        self.send_ix(
            Instruction {
                program_id: DM,
                accounts: dm::accounts::Propose {
                    proposer: alice.kp.pubkey(),
                    governance: g,
                    hub_dao: dao.dao,
                    proposal: p,
                    proposer_base: alice.ata,
                    proposer_quote: quote_ata(alice),
                    tokens: tokens(),
                    system_program: SYSTEM,
                }
                .to_account_metas(None),
                data: dm::instruction::Propose { id: 1, metadata_uri: "Smoke test payment".into(), instructions: vec![dao.pay(&to)], base_seed: 100, quote_seed: 100 }.data(),
            },
            &[&alice.kp],
            "propose, seeding both pools with 100 / 100",
        )?;
        let position = Pubkey::find_program_address(&[dm::POSITION_SEED, p.as_ref(), bob.kp.pubkey().as_ref()], &DM).0;
        self.send_ix(
            Instruction {
                program_id: DM,
                accounts: dm::accounts::Split { owner: bob.kp.pubkey(), proposal: p, position, owner_base: bob.ata, owner_quote: quote_ata(bob), tokens: tokens(), system_program: SYSTEM }
                    .to_account_metas(None),
                data: dm::instruction::Split { base: 200, quote: 200 }.data(),
            },
            &[&bob.kp],
            "Bob splits 200 / 200",
        )?;
        for (market, side_in, what) in [(dm::Market::Pass, dm::Side::Quote, "Bob buys in the pass market"), (dm::Market::Fail, dm::Side::Base, "Bob sells in the fail market")] {
            self.send_ix(
                Instruction {
                    program_id: DM,
                    accounts: dm::accounts::Trade { owner: bob.kp.pubkey(), proposal: p, position }.to_account_metas(None),
                    data: dm::instruction::Trade { market, side_in, amount_in: 100, min_out: 1 }.data(),
                },
                &[&bob.kp],
                what,
            )?;
        }
        self.wait_until(read::<dm::Proposal>(&self.rpc, &p).trading_deadline);
        self.send_ix(
            Instruction { program_id: DM, accounts: dm::accounts::Finalize { governance: g, proposal: p }.to_account_metas(None), data: dm::instruction::Finalize {}.data() },
            &[],
            "finalize the markets",
        )?;
        let settled = read::<dm::Proposal>(&self.rpc, &p);
        if !settled.passed {
            return Err(format!("didn't pass: pass TWAP {} vs fail TWAP {}", settled.pass_twap, settled.fail_twap));
        }
        println!("  passed: pass TWAP {} vs fail TWAP {} (quote per base x 10^12)", settled.pass_twap, settled.fail_twap);
        self.wait_until(self.now() + config.timelock as i64);
        self.execute(&dao, DM, p)?;
        self.expect_paid(&to)?;

        let pos = read::<dm::Position>(&self.rpc, &position);
        let before = (self.token_balance(&bob.ata), self.token_balance(&quote_ata(bob)));
        self.send_ix(
            Instruction {
                program_id: DM,
                accounts: dm::accounts::Redeem { owner: bob.kp.pubkey(), governance: g, proposal: p, position, owner_base: bob.ata, owner_quote: quote_ata(bob), tokens: tokens() }
                    .to_account_metas(None),
                data: dm::instruction::Redeem {}.data(),
            },
            &[&bob.kp],
            "Bob redeems his pass tokens",
        )?;
        let got = (self.token_balance(&bob.ata).saturating_sub(before.0), self.token_balance(&quote_ata(bob)).saturating_sub(before.1));
        if got != (pos.pass_base, pos.pass_quote) {
            return Err(format!("Bob redeemed {got:?}, expected his pass balances ({}, {})", pos.pass_base, pos.pass_quote));
        }
        println!("  Bob redeemed {} base and {} quote", got.0, got.1);
        self.send_ix(
            Instruction {
                program_id: DM,
                accounts: dm::accounts::ReclaimLiquidity { proposer: alice.kp.pubkey(), governance: g, proposal: p, proposer_base: alice.ata, proposer_quote: quote_ata(alice), tokens: tokens() }
                    .to_account_metas(None),
                data: dm::instruction::ReclaimLiquidity {}.data(),
            },
            &[&alice.kp],
            "Alice takes back the pass pool's liquidity",
        )?;
        let left = read::<dm::Proposal>(&self.rpc, &p);
        if left.base_collateral != 0 || left.quote_collateral != 0 {
            return Err(format!("{} base and {} quote still held for the proposal, expected none", left.base_collateral, left.quote_collateral));
        }
        Ok(())
    }

    fn token_balance(&self, account: &Pubkey) -> u64 {
        self.rpc.get_token_account_balance(account).ok().and_then(|b| b.amount.parse().ok()).unwrap_or(0)
    }
}

struct Orao {
    state: Pubkey,
    treasury: Pubkey,
    /// This wallet is a fulfillment authority (only on the local stand-in).
    answers_here: bool,
}

fn orao_request(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[vortex_sortition::ORAO_REQUEST_SEED, seed], &ORAO).0
}

/// Pyth's sponsored price account for `feed` (shard 0).
fn pyth_price_account(feed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&0u16.to_le_bytes(), feed], &PYTH_PUSH_ORACLE).0
}

const fn hex32(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("not lowercase hex"),
    }
}
