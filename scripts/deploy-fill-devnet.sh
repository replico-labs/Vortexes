#!/usr/bin/env bash
# Builds, tests and deploys vortex-fill (programs/fill) to devnet, then runs
# its smoke test there. Run from the repository root.
#
#   WALLET=~/.config/solana/id.json bash scripts/deploy-fill-devnet.sh
#
# Needs target/deploy/vortex_fill-keypair.json: the keypair for the address
# in programs/fill/src/lib.rs (declare_id!). The first run creates the
# program; later runs upgrade it (WALLET must be its upgrade authority).
# The wallet pays the program's rent (a few SOL on devnet, partly returned
# after the upload) and the smoke test (about 0.1 SOL).
set -eo pipefail
WALLET=${WALLET:-~/.config/solana/id.json}
KEYPAIR=target/deploy/vortex_fill-keypair.json

[ -f "$KEYPAIR" ] || { echo "Missing $KEYPAIR (the program's keypair)."; exit 1; }
EXPECTED=$(grep -o 'declare_id!("[^"]*")' programs/fill/src/lib.rs | cut -d'"' -f2)
ACTUAL=$(solana-keygen pubkey "$KEYPAIR")
if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "$KEYPAIR is $ACTUAL, but the program declares $EXPECTED."
  echo "Use the matching keypair, or run 'anchor keys sync' and copy the new IDL to idl/ and the bot."
  exit 1
fi

echo "== build"
anchor build

echo "== local tests (LiteSVM)"
cargo test -p vortex-tests --test fill

echo "== deploy $EXPECTED to devnet"
echo "wallet $(solana-keygen pubkey "$WALLET"): $(solana balance --url devnet --keypair "$WALLET")"
solana program deploy target/deploy/vortex_fill.so \
  --program-id "$KEYPAIR" \
  --upgrade-authority "$WALLET" --keypair "$WALLET" \
  --url devnet
solana program show "$EXPECTED" --url devnet

echo "== smoke test on devnet"
cargo run -q -p vortex-smoke -- fill --url devnet --keypair "$WALLET"
