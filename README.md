# DAO Vortexes

The SVM implementation of [DAO Spaces](https://github.com/replico-labs/Spaces): on-chain DAO governance on Solana where a DAO's treasury and its governance rules are separate, so a DAO can change how it decides by vote, without its treasury moving.

Built with Anchor 1.2.1 on Agave 4.3. All eleven programs are deployed on devnet, where every model has passed the smoke test; not on mainnet. The companion bot lives in [`protean-bot`](https://github.com/replico-labs/protean-bot).

## Why it's built differently from Spaces

On the EVM, every DAO gets its own `Treasury` contract, and switching models is one call: `transferGovernance(newGovernance)`. On Solana, a treasury held by a program address (PDA) can only ever be signed for by the program it belongs to, so it can't be handed from one governance program to another. Vortexes keeps that shared state in one place instead:

- **`vortex-hub`**, one program serving every DAO. It holds each DAO's **treasury** and its **record**: which governance program the DAO currently listens to.
- **Governance programs** only **decide**: proposals, votes, signers, and the token deposits (staking) voting power comes from. They hold no DAO funds.

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

**The DAO's token.** The hub records each DAO's token (`set_dao_token`, a `[b"token", dao]` account). The creator records it in the creation transaction, before the first governance is set up; after that, only a passed proposal can change it. Every token-based program sets a DAO up only with the recorded mint (`WrongMint` otherwise), so a switch always keeps the token members hold. Moving to another token takes its own `set_dao_token` instruction in a proposal, never a side effect of a switch. DAOs created before the record existed record it with their first switch to a token model (the bot adds the instruction to that switch proposal).

**Deposits after a switch.** Each program keeps the DAO's deposits in its own vault (`[b"vault", governance]`), and withdrawing from an old program never checks which program is active, so members can always take their tokens out of a model the DAO has left and deposit them in the new one. The bot's `/migrate` does both in one command, finding the old vaults from the DAO's address alone.

## Governance models

| Model | How it decides |
|---|---|
| **Token-weighted** | Deposit the DAO's token; vote for, against or abstain; quorum and approval threshold |
| **Quadratic** | Same, but a deposit of n tokens gives √n votes |
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
| `programs/hub` | DAO records, treasuries, the approved-models list, execution, switching |
| `programs/token-weighted` | Token-weighted voting. Its voting logic (`token_voting.rs`) is shared with quadratic |
| `programs/quadratic` | Quadratic voting: token-weighted with a √ vote weight |
| `programs/optimistic` | Challenge window, bonds, challenge votes |
| `programs/board` | Signers, confirmations |
| `programs/conviction` | Conviction, the asset list and spending budgets |
| `programs/delegate` | Elections, council votes, recalls |
| `programs/liquid` | Token voting with delegation: running totals, overrides, one hop |
| `programs/sortition` | The draw pool, ORAO-randomness draws, council votes |
| `programs/sowellian` | Approval votes, the YES/NO market, oracle and human resolution, adjudication, bonds |
| `programs/decision-markets` | Conditional balances, the pass and fail pools, TWAP settlement, redemption |
| `programs/fill` | Fill orders: a treasury swaps through any route (Jupiter's included) without trusting the keeper who runs it. Not a governance model |
| `crates/vortex-core` | Shared library: seeds, the proposal prefix every model uses, vote math, errors (not deployed) |

Each program's header comment explains its rules in full.

## Key design decisions

- **The hub holds the money; governance programs only decide.** A DAO's treasury address, funds and token accounts never move when it switches models.
- **Decisions can't be forged.** The hub only accepts a confirmation from the program the DAO points to, returned through Solana's CPI return data, for a proposal in the current epoch.
- **One shared proposal format.** Every model's proposal account starts with the same `ProposalCore` (DAO, epoch, instructions), so the hub can run proposals from any model.
- **Adding a model doesn't touch the hub.** A new program implements `confirm_execution` and `ProposalCore`, and the hub admin approves it. The admin has no power over any DAO's treasury or rules.
- **Deposits instead of snapshots.** Solana tokens have no balance history, so voting power is what's deposited, and deposits stay locked until the votes they were used in close.
- **Times are in seconds,** not blocks.
- **Fixes over the EVM contracts:**
  - Board: removing a signer also removes their pending confirmations.
  - Delegate: a recalled member's votes stop counting, and a new council ends the old one's proposals.
  - Optimistic: a challenger gets their bond back if the proposal is cancelled.

## Liquid delegation

The EVM version adds each delegator's weight after their delegate votes, one transaction per delegator from a keeper, and follows delegation chains. On Solana a chain can't be walked inside one transaction, so `vortex-liquid` works differently:

- **Running totals.** Delegating adds the member's deposit to their delegate's `delegated_power`; deposits, withdrawals and undelegating keep it in step. A delegate's vote counts their own deposit plus that total at once, with no keeper.
- **Overrides.** A delegator votes themselves with `override_vote`.
  - If the delegate already voted with their tokens, the weight moves from the delegate's choice to theirs.
  - If not, it's recorded against that proposal, and the delegate's later vote leaves it out. That record's address is fixed by the proposal and the delegate, so a delegate can't leave it out of the vote.
- **One hop.** A delegate can't delegate, and nobody can delegate to someone who has.
- **No double counting without snapshots.**
  - A global counter (`seq`) orders delegations and votes, so a vote includes a delegator's tokens exactly when it came after the delegation.
  - While a vote that counted a delegator's tokens is open, that delegator can't withdraw, add tokens or undelegate. They can always override.
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
- **Votes use deposits.** Approval and adjudication votes count deposited tokens, locked until those votes close. Positions and bonds come from wallets, so betting never reduces anyone's vote; the EVM version needs a pre-market snapshot to get the same effect.
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
anchor build      # programs into target/deploy/, IDLs into target/idl/
cargo test        # unit tests + end-to-end tests (needs the build above)
```

The end-to-end tests run the compiled programs in [LiteSVM](https://github.com/LiteSVM/litesvm) with the real SPL Token, Token-2022 and Associated Token programs. Current result: **78 end-to-end tests and 10 unit tests passed, 0 failed**. They cover every model's lifecycle, the hub's checks, switching between models with one treasury and one token (a switch onto another token fails, a deliberate `set_dao_token` change works), and budgets. `tests/tests/fill.rs` (7 tests: an order opened by a proposal and filled by a keeper, short fills, takes without their settle, output sent before the take, expiry and cancels) is new and hasn't been run yet.

`idl/` holds the IDLs for clients such as the bot; copy them from `target/idl/` after changing a program. Error codes come from `GovError` in `vortex-core` and are the same in every governance program; `vortex-fill` has its own (`FillError`).

## Deployment

The program IDs in the code are the devnet deployment's below. To deploy your own copy, generate your own program keypairs and point the code at them:

```bash
anchor keys sync      # your own program keypairs' addresses into the code
anchor build
anchor program deploy --provider.cluster devnet --provider.wallet ~/.config/solana/id.json
```

`--provider.wallet` is the wallet that pays the rent and becomes each program's upgrade authority; `-p <name>` deploys a single program.

Keep the program keypairs out of git (`.gitignore` already does): whoever holds them controls those addresses. Deploying all eleven programs costs about 29 SOL in rent.

Then run the smoke test. It sets up the hub (your wallet becomes the hub admin) and approves every deployed model, then creates a DAO in each of the ten models and pushes a proposal through that pays a little SOL from its treasury:

- **Liquid:** Bob delegates to Alice, Alice votes with both deposits, and Bob overrides with his own; the tally must come out 600 for, 400 against.
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

Each program's IDL is also published on-chain in its metadata account (`anchor program deploy` writes it).

External programs the models read (not ours; the same addresses on devnet and mainnet):

| Program | Address | Used by |
|---|---|---|
| ORAO VRF | `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y` | `vortex-sortition`: draw randomness |
| Pyth Solana Receiver | `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` | `vortex-sowellian`: owns the `PriceUpdateV2` accounts it resolves with |
| Pyth Push Oracle | `pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT` | Derives the sponsored price account addresses (`[shard u16 LE, feed id]`; shard 0) |

The hub is set up and all ten models are approved. The smoke test passed on devnet for every model, including ORAO VRF randomness for a sortition draw (answered in about 2 seconds, fee 0.0003 SOL) and a Sowellian proposal resolved from Pyth's sponsored SOL/USD price account. Its token-weighted → board switch finished after the 2-day delay: the DAO now runs on board with the same treasury, and the board has paid from it.

**DAO token record.** All eleven programs on devnet run the version with the hub's `set_dao_token` and the token check in every token-based program. New DAOs record their token when they're created. DAOs created before the upgrade keep working; their next switch to a token model records the token first.

### Upgrading the devnet programs

`scripts/upgrade-devnet.sh` uploads each program from `target/deploy/` to its address above. It needs only the upgrade authority's wallet, not the program keypairs: given an address, `solana program deploy` can only upgrade the program already there, never create a new one.

```bash
anchor build && cargo test
solana program show 5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe --url devnet   # Authority must be your wallet
WALLET=~/.config/solana/id.json bash scripts/upgrade-devnet.sh
```

Each upgrade writes the new code into a temporary buffer first (about 3.5 SOL of rent for the largest program, returned when it finishes). If a program has grown past its account, `solana program extend <address> 40000 --url devnet --keypair <wallet>` makes room. An interrupted upgrade leaves its buffer behind; `solana program close --buffers --url devnet --keypair <wallet>` returns the rent.


These addresses come from program keypairs that aren't in git. Deploying from your own keypairs gives new addresses: run `anchor keys sync`, rebuild, deploy, copy the IDLs, and point the bot at them with `SOLANA_DEVNET_<MODEL>_PROGRAM_ID`.

**Fill orders** (`vortex-fill`) aren't deployed: the routes they're for (Jupiter) are on mainnet only, so the program goes live there with the hub. Its address in the code, `9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77`, comes from a keypair kept off git like the others.

### Mainnet — not yet deployed

## Known gaps

- **No audit.** The hub holds every DAO's treasury. Get it audited before mainnet, and move the hub admin and every program's upgrade authority to a multisig.
- **Devnet only.** Every model has run a real proposal on devnet, and a DAO has switched models there (token-weighted → board) keeping its treasury.
- **The bot** ([protean-bot](https://github.com/replico-labs/protean-bot)) runs all ten models from its IDLs; its end-to-end tests run them on a local validator (the four newer models with ORAO's program and a Pyth price account loaded), not yet on devnet.
- **Budgets only see listed assets,** and only in the treasury's associated token accounts.

## License

MIT. See [LICENSE](LICENSE).
