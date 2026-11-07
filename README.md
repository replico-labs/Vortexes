# DAO Vortexes

The SVM implementation of [DAO Spaces](https://github.com/replico-labs/Spaces): on-chain DAO governance on Solana where a DAO's treasury and its governance rules are separate, so a DAO can change how it decides by vote, without its treasury moving.

Built with Anchor 1.2.1 on Agave 4.3. Thirteen programs: the hub, the stake pool, ten models and the fill program. The devnet deployment runs the version before the stake pool and the security council (see [Devnet](#devnet)); nothing is on mainnet. The companion bot lives in [`protean-bot`](https://github.com/replico-labs/protean-bot).

## Why it's built differently from Spaces

On the EVM, every DAO gets its own `Treasury` contract, and switching models is one call: `transferGovernance(newGovernance)`. On Solana, a treasury held by a program address (PDA) can only ever be signed for by the program it belongs to, so it can't be handed from one governance program to another. Vortexes keeps that shared state in one place instead:

- **`vortex-hub`**, one program serving every DAO. It holds each DAO's **treasury** and its **record**: which governance program the DAO currently listens to.
- **`vortex-stake`**, the DAO's **stake pool**: members stake the DAO's token once, and every token-based model reads voting power from it, so stake survives every switch.
- **Governance programs** only **decide**: proposals, votes and signers. They hold no DAO funds and no member tokens.

## Architecture

```
                   execute(proposal)
   anyone ───────────────────────────────▶ ┌──────────────────────┐
                                           │      vortex-hub      │
   ┌────────────────────────┐  confirm?    │  DAO record:         │
   │ the DAO's current      │ ◀─────────── │  · governance program│
   │ governance program     │              │  · epoch             │
   │ (any approved model)   │ ───────────▶ │                      │
   └────────────────────────┘  receipt     │  treasury (PDA) ─────┼──▶ runs the
                                           └──────────────────────┘    proposal's
                                                                       instructions
```

**Running a proposal.** Anyone calls the hub's `execute`. The hub asks the DAO's current governance program to confirm the proposal (`confirm_execution`, signed by a hub PDA so nobody else can ask). The program checks that the proposal passed and can run now, marks it executed, and answers with the proposal's address. Only then does the hub run the proposal's instructions, signed by the treasury. If any instruction fails, nothing changes.

**Switching models.** A passed proposal calls the hub's `propose_switch(new_program)` and sets the DAO up in the new program. After a 2-day delay (cancellable by another proposal), anyone calls `apply_switch`: the hub now listens only to the new program, and the DAO's **epoch** goes up. Proposals only run in the epoch they were made in, so nothing from an earlier setup can ever run again, even if the DAO switches back.

Every switch sets up a **fresh governance account**, `[b"governance", dao, epoch]` in the new program. A DAO can switch to any approved model in either direction, Board included, back to one it ran before, or to a fresh copy of the one running now (new rules or signers), and it always starts clean.

**The DAO's token and its stake pool.** The hub records each DAO's token and its current pool (`set_dao_token`, a `[b"token", dao]` account holding the mint and `pool = [b"pool", dao, mint]` in `vortex-stake`). The creator records it in the creation transaction, before the first governance is set up; after that, only a passed proposal can change it.

- **Staking.** Members stake in the current pool (`vortex-stake::stake`) and withdraw from any of the DAO's pools whatever isn't locked. Voting reads the stake; nothing moves at a switch.
- **Changing the token without changing governance.** A proposal calls `set_dao_token(new_mint)`: the record points at the new token's pool, the governance stays, and the old pool only takes withdrawals. Every proposal stores the pool it was made with, so proposals already open keep counting the old pool; new ones count the new pool.
- **The converter.** The same proposal can open a converter (`open_converter(rate_num, rate_den, fund)`), funded from the treasury in the same instruction (a plain transfer into its vault tops it up later). Members then `convert` their old stake: the old tokens go to the treasury and the new ones are staked for them at the rate. A later proposal can `close_converter`, returning what's left.
- **Locks need the member.** A governance can only lock stake in an instruction the member signs (voting, supporting, delegating, joining a pool), and it must be the DAO's current governance. Votes hold the stake until they end (`lock_until`); standing commitments lock an amount (`lock`) that only the governance that locked it releases, even after a switch.
- **Decision markets** trade the DAO's token as their base. After a token change, anyone calls `sync_base_token` to move new proposals onto it; open proposals keep their own base vault.

**The security council.** A passed proposal can put a council in front of the treasury (`set_council(signers, required, tenure)`). It's kept on the hub's DAO record, so it survives every switch. While it's on, `execute` refuses: `submit_to_council` confirms a passed proposal with the governance program and records it as waiting (pinned by a hash of its instructions); signers `council_confirm` or `council_reject`; once enough confirm, anyone calls `run_confirmed` (in the same epoch). Turning it off (`clear_council`) is a proposal like any other, so the council confirms it. If it won't, the DAO replaces the signers once their tenure is over: a proposal that does nothing but `set_council` runs without the council's confirmation after that. Approvals given to an earlier council don't count.

## Governance models

| Model | How it decides |
|---|---|
| **Token-weighted** | Stake the DAO's token; vote for, against or abstain; quorum and approval threshold |
| **Quadratic** | Same, but a stake of n tokens gives √n votes |
| **Optimistic** | Passes after a challenge window unless someone posts a bond to challenge it; challenged proposals go to a token vote |
| **Board (Multisig)** | M-of-N signers confirm; no token at all |
| **Conviction** | Members back one proposal at a time; support builds conviction over time, with per-asset spending budgets |
| **Delegate** | Members elect a council for a term; the council proposes and votes; members can recall council members |
| **Liquid** | Token-weighted, but members can delegate their votes (one hop) and still override their delegate on any proposal |
| **Sortition** | The council is drawn at random (ORAO VRF) from members who opted in; the council proposes and votes |
| **Sowellian** | Proposals state success criteria; members approve them; a YES/NO market backs predictions; the outcome is resolved by a Pyth price or a challengeable human resolver |
| **Decision Markets** | Futarchy: each proposal opens a pass market and a fail market; the higher time-weighted price decides, no votes |

All ten Spaces models are now ported.

## Programs

| Program | Responsibility |
|---|---|
| `programs/hub` | DAO records, treasuries, the approved-models list, execution, switching, the token record, the security council, monthly allowances |
| `programs/stake` | Stake pools per DAO token, consent locks, converters between a DAO's old and new token. Not a governance model |
| `programs/token-weighted` | Token-weighted voting. Its voting logic (`token_voting.rs`) is shared with quadratic |
| `programs/quadratic` | Quadratic voting: token-weighted with a √ vote weight |
| `programs/optimistic` | Challenge window, bonds, challenge votes |
| `programs/board` | Signers, confirmations |
| `programs/conviction` | Conviction, the asset list and spending budgets |
| `programs/delegate` | Elections for council seats (ties drawn by ORAO), council votes, recalls |
| `programs/liquid` | Token voting with delegation: running totals, overrides, one hop |
| `programs/sortition` | The draw pool, ORAO-randomness draws, council votes |
| `programs/sowellian` | Approval votes, the YES/NO market, oracle and human resolution, adjudication, bonds |
| `programs/decision-markets` | Conditional balances, the pass and fail pools, TWAP settlement, redemption |
| `programs/fill` | Fill orders: a treasury swaps through any route (Jupiter's included) without trusting the keeper who runs it. Not a governance model |
| `programs/distribution` | A DAO's Distributor (promised tokens, vesting, claims tied to chat accounts, council-approved welcome grants) and its raises (pro-rata, paying only into the DAO's treasury, or a prop firm's vault). Not a governance model |
| `programs/vault` | A prop-firm DAO's vault: the DAO's token as its shares, deposits and redemptions priced at windows, fees split between traders and the treasury, a drawdown pause, its value read from Phoenix's accounts. Not a governance model |
| `crates/vortex-core` | Shared library: seeds, the proposal prefix every model uses, vote math, errors (not deployed) |

Each program's header comment explains its rules in full.

## Key design decisions

- **The hub holds the money; governance programs only decide.** A DAO's treasury address, funds and token accounts never move when it switches models.
- **Decisions can't be forged.** The hub only accepts a confirmation from the program the DAO points to, returned through Solana's CPI return data, for a proposal in the current epoch.
- **One shared proposal format.** Every model's proposal account starts with the same `ProposalCore` (DAO, epoch, instructions), so the hub can run proposals from any model.
- **Adding a model doesn't touch the hub.** A new program implements `confirm_execution` and `ProposalCore`, and the hub admin approves it. The admin has no power over any DAO's treasury or rules.
- **Stake instead of snapshots.** Solana tokens have no balance history, so voting power is what's staked in the DAO's pool, and stake stays locked until the votes it was used in close. One pool serves every model, so a switch never asks members to move tokens.
- **Times are in seconds,** not blocks.
- **Fixes over the EVM contracts:**
  - Board: removing a signer also removes their pending confirmations.
  - Delegate: a recalled member's votes stop counting, and a new council ends the old one's proposals.
  - Optimistic: a challenger gets their bond back if the proposal is cancelled.

## Liquid delegation

The EVM version adds each delegator's weight after their delegate votes, one transaction per delegator from a keeper, and follows delegation chains. On Solana a chain can't be walked inside one transaction, so `vortex-liquid` works differently:

- **Running totals.** Delegating locks the member's unlocked stake and adds it to their delegate's `delegated_power`; undelegating releases it. Delegations only count on proposals in the same pool as the delegation. A delegate's vote counts their own stake plus that total at once, with no keeper.
- **Overrides.** A delegator votes themselves with `override_vote`.
  - If the delegate already voted with their tokens, the weight moves from the delegate's choice to theirs.
  - If not, it's recorded against that proposal, and the delegate's later vote leaves it out. That record's address is fixed by the proposal and the delegate, so a delegate can't leave it out of the vote.
- **One hop.** A delegate can't delegate, and nobody can delegate to someone who has.
- **No double counting without snapshots.**
  - A global counter (`seq`) orders delegations and votes, so a vote includes a delegator's tokens exactly when it came after the delegation.
  - While a vote that counted a delegator's tokens is open, that delegator can't undelegate. They can always override.
  - A member can't delegate while a vote of their own is open.

## Sortition randomness

Pyth Entropy, which the EVM version uses, isn't on Solana. `vortex-sortition` uses [ORAO VRF](https://github.com/orao-network/solana-vrf) (`VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`, the same address on devnet and mainnet) without calling it:
- **Starting a draw.** The draw's transaction first asks ORAO for randomness, with the seed `sha256("vortex-sortition", governance, draw number, nonce)`, then calls `start_draw`. That checks the request is ORAO's account at that seed's address and still unanswered, then freezes the pool. Nobody can join or leave the pool knowing the result.
- **Finalizing.** Once ORAO answers, `finalize_draw` reads the 64 random bytes and seats the council with a partial Fisher-Yates shuffle.
- **Stuck draws.** If ORAO doesn't answer within `randomness_timeout`, the draw can be started again with a new request. That's allowed only while the old request is still unanswered, so a result can't be seen and then thrown away.
- **No dependency on ORAO's code.** Because the program never calls ORAO, it doesn't depend on ORAO's Anchor version. The tests write ORAO's request accounts directly in the layout from `orao-solana-vrf` 0.7.0.
- **Small pools.** If the pool is smaller than the council, everyone in it is seated, and the quorum is capped at the council's size.

## Sowellian resolution

The flow is the EVM version's: propose with a bond and criteria, approval vote, YES/NO positions, execution through the hub, a measurement period, then resolution. On Solana:
- **Oracle track: Pyth.** The proposal names a Pyth feed id and an 18-decimal target. Resolving takes a `PriceUpdateV2` account, and the program refuses it unless:
  - it's owned by the Pyth Receiver (`rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`);
  - it's fully verified (all Wormhole guardian signatures checked);
  - it's for that feed;
  - it was published after the measurement period ended, a check the EVM version doesn't make;
  - it's no older than `max_oracle_staleness`.
- **Votes use stake.** Approval and adjudication votes count staked tokens, locked until those votes close. Positions and bonds come from wallets, so betting never reduces anyone's vote; the EVM version needs a pre-market snapshot to get the same effect.
- **Bonds are claimed, not pushed.** Each owner claims their own bond (`claim_bond`), and forfeits are swept to the treasury (`sweep_to_treasury`). Nobody's missing token account can block a resolution.
- **Expiry.** An approved proposal that nobody runs within `execution_period` can be voided: stakes are refunded and the bond returned.

## Decision markets

Same rules as the EVM version: a proposal's pass and fail markets trade for `trading_period`, and it passes if the pass TWAP beats the fail TWAP by more than `threshold_bps`. Built for Solana:
- **Conditional tokens are a ledger.** The EVM version deploys four ERC20s and two pair contracts per proposal. Here each proposal keeps everyone's pass and fail balances (`split` and `merge` move the DAO's token or the quote token in and out), and keeps both pools' reserves.
- **The quote token is set per DAO**, e.g. wrapped SOL or USDC, in place of WMON.
- **One step to open.** Proposing seeds both pools from the proposer and starts trading at once; there's no 2M-gas limit to split around.
- **Settlement.** The winning side redeems 1:1 and the losing side is worthless. The proposer reclaims the winning pool, which is their seed plus fees after trades.
- **Cancelling** settles as failed, because the proposal won't run.
- **Collateral is tracked per proposal.** Every payout comes out of that, so a market can never draw on another proposal's funds. The tests check that the vaults end at exactly zero.
- **TWAP.** It's price × seconds over the whole window, the EVM pair's method. A one-second spike at the end moves it by about 1%, which the tests also check.

## Conviction spending budgets

As in Spaces' version 2: the DAO keeps a list of up to 10 assets it protects (SOL and its own token from the start), each with a weight. Every proposal declares a budget per asset, and its bar rises by `weight × amount ÷ the treasury's holding`. Proposals that loosen the rules, switch model or hand over an authority need the bar of spending everything.

The program wraps every proposal's instructions between `record_balances` and `check_budget`, so the check runs inside the hub's execution. Overspending, or closing a watched token account or giving it a new owner or delegate, reverts the whole execution. Weight cuts and removals wait 7 days. Only listed assets are watched.

## Monthly allowances

A passed proposal calls the hub's `create_allowance(recipient, mint, amount, ends_at)` (mint None: SOL; the end at most a year out). The recipient calls `claim_allowance` once per 30-day period and the hub pays from the treasury; a period not claimed lapses. `update_allowance` and `revoke_allowance` by proposal. With a council on, granting goes through the council; claiming doesn't.

## Council seats (Delegate)

The council is numbered seats, labelled by proposal (`set_seat_label`). `declare_candidacy(seat)` runs for one seat; a ballot picks at most one candidate per seat; each seat goes to its most-voted candidate. A tie is drawn by ORAO: the caller requests randomness at `tie_seed(governance, election id)` in `finalize_election`'s transaction and `resolve_ties` seats the council once ORAO answers; after a day without an answer the tie goes to whoever declared first. A seat nobody ran for keeps its holder; a recall leaves the seat vacant.

## Distributor and raises

- **Distributor** `[distributor, dao, mint, operator]`: holds promised tokens. Allocations (owner wallet and/or chat account id, cliff then linear vesting) come from a passed proposal (`allocate`), a raise when it settles, or `grant_welcome`, approved by a member of the DAO's current Delegate council once a proposal sets the amount and cap (`set_welcome`). The owner `claim`s; the bot's operator `claim_for`s an account into the member's wallet. `return_unpromised` and, after the claim deadline, `close_distributor` send tokens back to the treasury.
- **Raise** `[raise, dao, create_key]`, in one token: contributions stay open for the whole period, past the hard cap. After the end the decider (a new DAO's founder, or the treasury by proposal) `accept`s any amount from the soft cap up, within the decision window, and it goes to the DAO's treasury account and nowhere else. `settle` writes each contributor's tokens into the Distributor; `withdraw_refund` pays back the part not accepted, or everything if the raise failed (below the soft cap, or no decision in time).
- A launch on Solana creates the DAO first, so contributors see its rules and treasury before they pay, then opens the raise against its treasury.
- A prop firm's raise names its vault (`prop_firm_vault`, made with the raise as its seeder) and has no sale supply: `accept` pays into the vault, which mints its first shares to the Distributor, and those are what contributors share.

## Prop-firm vaults

Only for DAOs created as prop firms: the vault's shares **are** the DAO's token (`create_vault` takes the mint authority; the token has no supply yet and no freeze authority), so such a DAO never has two tokens.

- **Windows.** `request_deposit` / `request_redeem` (shares into the escrow) at any time; `cancel_*` until the window. `process_window` (anyone, once due) values the vault, takes fees, prices redemptions and then deposits at one price; members `claim_shares` / `claim_redemption` (as far as money is idle; the rest once the desk brings it back).
- **Moves.** `execute` runs one instruction as the vault (a Phoenix deposit or withdrawal, delegating a subaccount to a trader's key), signed by the treasury through a proposal. Never on the share mint or the escrow, never in the `freeze_before` run-up to a window, and the window waits `settle_delay` after the last move.
- **Value.** Idle USDC (less pending deposits and owed redemptions) + each registered Phoenix trader account (`set_venues`): collateral + position PnL at Phoenix's mark price + unsettled funding, read from Phoenix's PerpAssetMap and trader accounts at fixed offsets + the vault's other registered token accounts at 1:1. A mark price older than `max_price_age_slots`, money in Phoenix's withdraw queue, or spot collateral stops the window. A window with two Phoenix accounts and the full mainnet market list takes about 28,000 compute units.
- **Fees and drawdown.** A yearly management fee and a performance fee above the high water mark, minted as shares and owed to each split recipient (`claim_fees`). Below `max_drawdown_bps` under the high water mark, `paused` is set (by a window or anyone's `check_drawdown`): the bot stops signing traders' orders until a proposal `resume`s.

## Fill orders (swaps)

A proposal can't usefully call a swap router: a route found when the proposal is written is stale by the time it executes, and a router's accounts and lookup tables don't fit a stored instruction. `vortex-fill` splits the swap instead. Jupiter recommends this pattern for programs (Flash Fill).

1. **A passed proposal opens an order** (`open_order`). The treasury moves `amount` of one token into the order's input vault, with the least it accepts of another (`min_out`), a deadline, and the treasury account the output goes to. The treasury pays the order's rent and gets it back when the order closes.
2. **A keeper fills it** in one transaction of its own (any route, lookup tables, any size): `take` hands it the input, it swaps, sending the output to the order's output vault, and `settle` checks that the vault gained at least `min_out` since the take. `settle` then sends everything in it to the treasury and closes the order. Less, and the whole transaction reverts, the take included.
3. **Past the deadline anyone can `cancel`** an unfilled order, returning the input to the treasury. The treasury can cancel any time, through a proposal.

How a keeper is kept honest:

- `take` only runs as a top-level instruction, and only when the next instruction of this program in the transaction is a `settle` of the same order (read from the instructions sysvar). Two orders can't share one swap, and a take can't happen on its own.
- Each order has its own output vault, owned by the order. Nothing else that pays the treasury in the same transaction counts toward a fill, such as another order's refund or a proposal executed alongside it. Only output that arrives after the take counts; anything sent to the vault earlier still goes to the treasury.

The keeper gets nothing but the chance to fill. The bot runs one, filling at the best route it finds and sending all the output to the treasury. The program has its own errors (`FillError`), not `GovError`.

## Building and testing

You need Rust (pinned to 1.97.1), Agave 4.3 and the Anchor CLI 1.2.1.

```bash
anchor build --ignore-keys   # programs into target/deploy/, IDLs into target/idl/
cargo test                   # unit tests + end-to-end tests (needs the build above)
```

`--ignore-keys`: without the deployer's program keypairs, `anchor build` makes random ones in `target/deploy/` and then refuses to build because they don't match the program IDs in the code. The tests load each program at the ID in the code, so the keypairs don't matter for them. Don't run `anchor keys sync` to silence it: that rewrites every program ID to your random keypairs' addresses.

Build each program on its own if `anchor build` isn't available: `cargo build-sbf --manifest-path programs/<name>/Cargo.toml` (and `scripts/orao-stub`, `scripts/hub-stub`).

`scripts/hub-stub` is a test tool only, never deployed: loaded at the hub's address in LiteSVM, it signs as a DAO's treasury PDA the way an executed proposal does. The bot's `scripts/test-phoenix-flow.mjs` uses it to run a prop firm's trading desk through the real vault program against Phoenix's mainnet binaries: the vault's PDA as the Phoenix authority, each move a `vault.execute`, and `check_drawdown` valuing the vault from Phoenix's own trader accounts (flat and with a position open, to a millionth).

The end-to-end tests run the compiled programs in [LiteSVM](https://github.com/LiteSVM/litesvm) with the real SPL Token, Token-2022 and Associated Token programs. Current result: **118 tests passed, 0 failed** (end-to-end and unit). They cover every model's lifecycle, the hub's checks, and budgets, plus:

- `tests/tests/stake.rs`: a token change without a switch, with a proposal left open across it; the converter; stake carried across token-weighted → board → quadratic; a lock refused without the DAO's governance.
- `tests/tests/switching.rs`: a Board-born DAO switching to every model; token-weighted and quadratic → board → the same model again.
- `tests/tests/council.rs`: proposals waiting for the council, rejection, the council surviving a switch, waiting proposals dying with their epoch, replacing a council that won't step down after its tenure, then turning it off.
- `decision_markets.rs`: `sync_base_token` after a token change.
- `tests/tests/vault.rs`: windows, fees and claims; moves only by proposal and never near a window; the drawdown pause; a closed vault; a prop firm's raise seeding the vault; and the Phoenix valuation against a mainnet copy of Phoenix's PerpAssetMap (`tests/fixtures/`), with every offset checked against Phoenix's own `phoenix-rise-accounts` crate.

The smoke test (below) also passes against a local `solana-test-validator` with every program loaded at its address (`--bpf-program <id> target/deploy/<name>.so`) and `scripts/orao-stub` at ORAO's: all ten models, the switch start, and fill orders. `tests/tests/fill.rs` (7 tests: an order opened by a proposal and filled by a keeper, short fills, takes without their settle, output sent before the take, expiry and cancels) passed, 7 of 7.

`idl/` holds the IDLs for clients such as the bot (`vortex_stake.json` included); copy them from `target/idl/` after changing a program. Error codes come from `GovError` in `vortex-core` and are the same in every governance program; `vortex-fill` has its own (`FillError`).

## Deployment

The program IDs in the code are the devnet deployment's below. To deploy your own copy, generate your own program keypairs and point the code at them:

```bash
anchor keys sync      # your own program keypairs' addresses into the code
anchor build
anchor program deploy --provider.cluster devnet --provider.wallet ~/.config/solana/id.json
```

`--provider.wallet` is the wallet that pays the rent and becomes each program's upgrade authority; `-p <name>` deploys a single program.

Keep the program keypairs out of git (`.gitignore` already does): whoever holds them controls those addresses. Deploying all thirteen programs costs about 32 SOL in rent.

Then run the smoke test. It sets up the hub (your wallet becomes the hub admin) and approves every deployed model, then creates a DAO in each of the ten models (with its stake pool), stakes for the members, and pushes a proposal through that pays a little SOL from its treasury:

- **Liquid:** Bob delegates to Alice, Alice votes with both stakes, and Bob overrides with his own; the tally must come out 600 for, 400 against.
- **Sortition:** three members join the pool, and a draw with real ORAO randomness seats two of them, who pass the proposal.
- **Sowellian:** two proposals. One is on the human track, with YES and NO positions, resolved unchallenged and paid out; the other is on the oracle track, resolved from Pyth's sponsored SOL/USD price account.
- **Decision markets:** trades push the pass market above the fail market, so it passes and runs; the trader redeems and the proposer takes back the liquidity.

It also starts a token-weighted → board switch, which can only finish after the hub's 2-day delay. A full run costs about 1 SOL and takes about 15 minutes, mostly waiting out short voting windows (and, on devnet, for ORAO to answer and Pyth to publish a fresh price).

```bash
cargo run -p vortex-smoke -- run --url devnet             # uses ~/.config/solana/id.json; --keypair to change
cargo run -p vortex-smoke -- run --url devnet --only liquid,sortition,sowellian,decision-markets
cargo run -p vortex-smoke -- approve --url devnet         # only approve the deployed models (hub admin)
cargo run -p vortex-smoke -- finish-switch --url devnet   # 2 days later
```

Approving needs the hub admin's wallet; with another wallet, unapproved models are reported and their runs fail at "create DAO". On a local validator (`--url local`), load `scripts/orao-stub` at ORAO's address and a SOL/USD `PriceUpdateV2` account (the bot's `scripts/test-solana-new-models.mjs --price-account <file>` writes one); the smoke test then answers the randomness requests itself. Without the price account, the oracle track is left out.

### Devnet

| Program | Address |
|---|---|
| `vortex-hub` | `5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe` |
| `vortex-token-weighted` | `Hf9MbqsbSDXGugmKYAVSLe3Urt3tTUPq5wNEsJgrikV8` |
| `vortex-quadratic` | `ES1UEWvh4uHf2nwE3pYSWms8QNEXDn2JtSiNjtAWi3tR` |
| `vortex-optimistic` | `Bjcfj2p8da2mjzDzMbcpxSrqayuiDToDVZdaBPmGPpdX` |
| `vortex-board` | `9yERBxsEw5oDDQVX2BEZP7ehpGAr3wArZoT3sbxNoXZE` |
| `vortex-conviction` | `9sxWrySQv33tdf7WZQKW7zkDrjeq1EiphKqBgq2kxrWQ` |
| `vortex-delegate` | `6TVZxo8LeXTsgH8iC39SD1AUeNUYJx3kGf71bEmsUvk2` |
| `vortex-liquid` | `CMf1cxiDNmEasmM9VQwJSz5p4RVw1A5GZUjTEy95du1W` |
| `vortex-sortition` | `FjuZX6dakUUY2GxqnHiqkpDUADBJyA3GkmoT5gqvp6U5` |
| `vortex-sowellian` | `G99YpKvNXVQmTgvXX6XGt1z3LvbEyhhDCENwat1nsrx2` |
| `vortex-decision-markets` | `FfUPS88Xoao8N4QTW9SGtua1XyN2q4HyBpNha7AB5t4S` |
| `vortex-fill` | `9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77` |
| `vortex-stake` | `7T27h8r13Xmp1m4vQbHFfs1xTqdQaHnxW7ybHvwjqKAd` (not deployed yet) |
| `vortex-distribution` | `4FeTS89hg2BtrkV2vN73be93bVvokBRHR7gbC6uUJgAq` (not deployed yet) |

Each program's IDL is also published on-chain in its metadata account (`anchor program deploy` writes it).

External programs the models read (not ours; the same addresses on devnet and mainnet):

| Program | Address | Used by |
|---|---|---|
| ORAO VRF | `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y` | `vortex-sortition`: draw randomness |
| Pyth Solana Receiver | `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` | `vortex-sowellian`: owns the `PriceUpdateV2` accounts it resolves with |
| Pyth Push Oracle | `pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT` | Derives the sponsored price account addresses (`[shard u16 LE, feed id]`; shard 0) |

The hub is set up and all ten models are approved. The smoke test passed on devnet for every model, including ORAO VRF randomness for a sortition draw (answered in about 2 seconds, fee 0.0003 SOL) and a Sowellian proposal resolved from Pyth's sponsored SOL/USD price account. Its token-weighted → board switch finished after the 2-day delay: the DAO now runs on board with the same treasury, and the board has paid from it.

**DAO token record.** All eleven programs on devnet run the version with the hub's `set_dao_token` and the token check in every token-based program.

**Not on devnet yet: the stake pool, epoch addresses and the security council.** Account layouts change (the hub's DAO record, every governance and proposal account), so after the upgrade devnet DAOs are made fresh. Deploy `vortex-stake` and upgrade the rest with `scripts/upgrade-devnet.sh` (it deploys a program that isn't there yet from its keypair), then rerun the smoke test.

### Upgrading the devnet programs

`scripts/upgrade-devnet.sh` uploads each program from `target/deploy/` to its address above. For an upgrade it needs only the upgrade authority's wallet, not the program keypairs: given an address, `solana program deploy` can only upgrade the program already there, never create a new one. A program that isn't deployed yet (`vortex-stake`) needs its keypair, `target/deploy/vortex_stake-keypair.json`.

```bash
anchor build && cargo test
solana program show 5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe --url devnet   # Authority must be your wallet
WALLET=~/.config/solana/id.json bash scripts/upgrade-devnet.sh
```

Each upgrade writes the new code into a temporary buffer first (about 3.5 SOL of rent for the largest program, returned when it finishes). If a program has grown past its account, `solana program extend <address> 40000 --url devnet --keypair <wallet>` makes room. An interrupted upgrade leaves its buffer behind; `solana program close --buffers --url devnet --keypair <wallet>` returns the rent.


These addresses come from program keypairs that aren't in git. Deploying from your own keypairs gives new addresses: run `anchor keys sync`, rebuild, deploy, copy the IDLs, and point the bot at them with `SOLANA_DEVNET_<MODEL>_PROGRAM_ID`.

**Fill orders** (`vortex-fill`, address `9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77`, from a keypair kept off git like the others). Jupiter routes on mainnet only, so devnet is for checking the program itself: the smoke test fills an order with a stand-in for the swap. To build, test, deploy it to devnet and run that check in one go:

```bash
WALLET=~/.config/solana/id.json bash scripts/deploy-fill-devnet.sh
```

It needs `target/deploy/vortex_fill-keypair.json` matching the address in the code. **Deployed on devnet** (slot 515849711, upgrade authority `5BmARdL3uzCiX3wDXnxoJL98J14kVxfFTEMPeacs5Qo5`); the smoke test passed there through the hub: the proposal opened the order, the four refusals held (take without settle, short fill, early cancel by a stranger, fill past the deadline), the fill paid the treasury 260 against a minimum of 250 and returned the rent, and the expired order's tokens went back on cancel. The smoke test alone: `cargo run -p vortex-smoke -- fill --url devnet`. It has a board DAO's proposal open an order (100 of its token for at least 250 of a second token), checks that a take without its settle and a short fill are refused, fills it as a keeper, and lets a second order expire and cancels it.

### Mainnet — not yet deployed

## Known gaps

- **No audit.** The hub holds every DAO's treasury. Get it audited before mainnet, and move the hub admin and every program's upgrade authority to a multisig.
- **Devnet only.** Every model has run a real proposal on devnet, and a DAO has switched models there (token-weighted → board) keeping its treasury. The stake pool and the security council have run on a local validator, not yet on devnet.
- **The bot** ([protean-bot](https://github.com/replico-labs/protean-bot)) runs all ten models from its IDLs; its end-to-end tests run them on a local validator (the four newer models with ORAO's program and a Pyth price account loaded), not yet on devnet.
- **The vault's Phoenix valuation** is checked against Phoenix's crate and a real PerpAssetMap, but the trader accounts in the tests are built to Phoenix's layout, not copied from mainnet. Before real money: value a live vault-owned trader account on a mainnet fork, confirm a position's asset index is its market's asset id, and confirm Phoenix's collateral token counts 1:1.
- **Budgets only see listed assets,** and only in the treasury's associated token accounts.

## License

MIT. See [LICENSE](LICENSE).
