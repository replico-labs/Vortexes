#!/usr/bin/env bash
# Deploys (or upgrades) every Vortexes program, sets up the hub and approves
# the ten models, then prints what the bot needs. Run from the repository root.
#
#   WALLET=~/.config/solana/id.json bash scripts/deploy.sh             # devnet
#   WALLET=... CLUSTER=mainnet RPC_URL=<your RPC> bash scripts/deploy.sh
#
# Options (environment):
#   WALLET   Pays rent and buffers, becomes every new program's upgrade
#            authority and the hub admin. Default ~/.config/solana/id.json.
#   CLUSTER  devnet (default) or mainnet.
#   RPC_URL  Defaults to the public endpoint of CLUSTER. On mainnet use a
#            paid RPC: the public one rate-limits program uploads.
#   ONLY     Space-separated program names to deploy, e.g. "vortex_vault vortex_hub".
#   BUILD=0  Skip `anchor build` (use what's in target/deploy/).
#   TEST=0   Skip `cargo test`.
#   SMOKE=1  Run the full smoke test afterwards (about 1 SOL, ~15 minutes).
#   ARCH     SBPF version to build: v0 or v3. Default: read from the cluster
#            (v3 where it's enabled; required once SIMD-0500 stops v0-v2 deploys).
#
# A program not on the cluster yet is created from its keypair,
# target/deploy/<name>-keypair.json, which must match the address in the code.
# A program already there is upgraded with WALLET (it must be its upgrade
# authority), and skipped if the bytes on chain are already the same.
# Nothing is sent until every check passes.
set -eo pipefail
cd "$(dirname "$0")/.."

WALLET=${WALLET:-~/.config/solana/id.json}
WALLET=${WALLET/#\~/$HOME}
CLUSTER=${CLUSTER:-devnet}
case "$CLUSTER" in
  devnet) RPC_URL=${RPC_URL:-https://api.devnet.solana.com}; PREFIX=SOLANA_DEVNET ;;
  mainnet) RPC_URL=${RPC_URL:-https://api.mainnet-beta.solana.com}; PREFIX=SOLANA_MAINNET ;;
  *) echo "CLUSTER must be devnet or mainnet"; exit 1 ;;
esac

# Name, address. The stake program first: the hub and every model read it.
PROGRAMS="vortex_stake 7T27h8r13Xmp1m4vQbHFfs1xTqdQaHnxW7ybHvwjqKAd
vortex_hub 5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe
vortex_token_weighted Hf9MbqsbSDXGugmKYAVSLe3Urt3tTUPq5wNEsJgrikV8
vortex_quadratic ES1UEWvh4uHf2nwE3pYSWms8QNEXDn2JtSiNjtAWi3tR
vortex_optimistic Bjcfj2p8da2mjzDzMbcpxSrqayuiDToDVZdaBPmGPpdX
vortex_board 9yERBxsEw5oDDQVX2BEZP7ehpGAr3wArZoT3sbxNoXZE
vortex_conviction 9sxWrySQv33tdf7WZQKW7zkDrjeq1EiphKqBgq2kxrWQ
vortex_delegate 6TVZxo8LeXTsgH8iC39SD1AUeNUYJx3kGf71bEmsUvk2
vortex_liquid CMf1cxiDNmEasmM9VQwJSz5p4RVw1A5GZUjTEy95du1W
vortex_sortition FjuZX6dakUUY2GxqnHiqkpDUADBJyA3GkmoT5gqvp6U5
vortex_sowellian G99YpKvNXVQmTgvXX6XGt1z3LvbEyhhDCENwat1nsrx2
vortex_decision_markets FfUPS88Xoao8N4QTW9SGtua1XyN2q4HyBpNha7AB5t4S
vortex_fill 9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77
vortex_distribution 4FeTS89hg2BtrkV2vN73be93bVvokBRHR7gbC6uUJgAq
vortex_vault GjVM1GkPEzXMRLsrP2FLr9UXJpXDVnwGVsVYbzTvtAU5"

# `solana program show`: its output, nothing if the program isn't there, or stop on any other error
# (a rate-limited RPC must not read as "not deployed").
show() {
  local out
  if out=$(solana program show "$1" --url "$RPC_URL" --keypair "$WALLET" 2>&1); then echo "$out"
  elif echo "$out" | grep -q "Unable to find the account"; then :
  else echo "✗ solana program show $1: $out" >&2; exit 1; fi
}
selected() { [ -z "$ONLY" ] || [[ " $ONLY " == *" $1 "* ]]; }
fail() { echo "✗ $*"; exit 1; }

echo "== tools"
for t in solana solana-keygen cargo; do command -v $t >/dev/null || fail "$t isn't installed"; done
[ "$BUILD" = 0 ] || command -v anchor >/dev/null || fail "anchor isn't installed (or run with BUILD=0)"
[ -f "$WALLET" ] || fail "no wallet at $WALLET"
ME=$(solana-keygen pubkey "$WALLET")
echo "cluster $CLUSTER ($RPC_URL), wallet $ME"

# Every program's declare_id! must match the list above.
while read NAME ID; do
  DIR=programs/$(echo ${NAME#vortex_} | tr _ -)
  CODE=$(grep -o 'declare_id!("[^"]*")' $DIR/src/lib.rs | cut -d'"' -f2)
  [ "$CODE" = "$ID" ] || fail "$DIR declares $CODE, the list says $ID"
done <<< "$PROGRAMS"

# Which SBPF version the cluster takes. Agave 4.x turns on SBPFv3 (SIMD-0178/0189/0377), then
# SIMD-0500 refuses new v0-v2 deployments: a v0 build fails with "invalid account data".
feature() { solana feature status "$1" --url "$RPC_URL" --keypair "$WALLET" 2>/dev/null | grep -q "active since"; }
if [ -z "$ARCH" ]; then
  if feature 5cC3foj77CWun58pC51ebHFUWavHWKarWyR5UUik7dnC; then ARCH=v3; else ARCH=v0; fi
  feature B8JJXCy5amZyWG9r7EnUYLwzXSXTxG7GZ1qZ1qggo83g && OLD_OFF=1
  echo "SBPF: building $ARCH${OLD_OFF:+ (this cluster no longer accepts v0-v2 deployments)}"
fi
WANT_FLAGS=${ARCH#v}

if [ "$BUILD" != 0 ]; then
  echo "== build"
  # anchor build for the IDLs (target/idl/); it passes its own --arch to cargo build-sbf, so
  # the programs (and the test stubs) are then rebuilt for $ARCH with cargo build-sbf directly.
  anchor build --ignore-keys
  if [ "$ARCH" != v0 ]; then
    for d in programs/* scripts/orao-stub scripts/hub-stub; do
      echo "-- $d ($ARCH)"
      OUT=$(cargo build-sbf --manifest-path $d/Cargo.toml --arch $ARCH --sbf-out-dir target/deploy 2>&1) \
        || { echo "$OUT" | tail -30; fail "building $d for $ARCH failed"; }
    done
  fi
fi
if [ "$TEST" != 0 ]; then
  echo "== tests (LiteSVM)"
  cargo test
fi

echo "== checks"
TODO=""
NEED=0
while read NAME ID; do
  selected $NAME || continue
  SO=target/deploy/$NAME.so
  [ -f "$SO" ] || fail "$SO is missing (build first)"
  FLAGS=$(od -An -t u4 -j 48 -N 4 "$SO" | tr -d ' ')
  [ "$FLAGS" = "$WANT_FLAGS" ] || fail "$SO is SBPF v$FLAGS, not $ARCH: rebuild without BUILD=0, or cargo build-sbf --manifest-path programs/<name>/Cargo.toml --arch $ARCH --sbf-out-dir target/deploy"
  SHOW=$(show $ID)
  if [ -n "$SHOW" ]; then
    AUTH=$(echo "$SHOW" | awk '/^Authority:/ {print $2}')
    [ "$AUTH" = "$ME" ] || fail "$NAME ($ID): upgrade authority is $AUTH, not your wallet $ME"
    LEN=$(echo "$SHOW" | awk '/^Data Length:/ {print $3}')
    SIZE=$(stat -c%s "$SO" 2>/dev/null || stat -f%z "$SO")
    [ "$SIZE" -le "$LEN" ] || echo "  $NAME has grown past its account ($SIZE > $LEN bytes): it will be extended"
    DUMP=$(mktemp)
    solana program dump $ID "$DUMP" --url "$RPC_URL" --keypair "$WALLET" >/dev/null 2>&1 || true
    if head -c "$SIZE" "$DUMP" | cmp -s - "$SO"; then
      echo "  $NAME: already up to date"
      rm -f "$DUMP"; continue
    fi
    rm -f "$DUMP"
    echo "  $NAME: upgrade"
  else
    KP=target/deploy/$NAME-keypair.json
    [ -f "$KP" ] || fail "$NAME isn't on $CLUSTER yet and $KP is missing"
    [ "$(solana-keygen pubkey $KP)" = "$ID" ] || fail "$KP is $(solana-keygen pubkey $KP), not $ID"
    echo "  $NAME: new deployment"
  fi
  SIZE=$(stat -c%s "$SO" 2>/dev/null || stat -f%z "$SO")
  NEED=$((NEED + SIZE))
  TODO="$TODO$NAME $ID
"
done <<< "$PROGRAMS"

if [ -n "$TODO" ]; then
  # Rent is about 0.00000696 SOL a byte; the upload buffer needs the same again, returned at the end.
  NEED_SOL=$(awk "BEGIN { printf \"%.1f\", $NEED * 2 * 0.00000696 + 1 }")
  HAVE=$(solana balance "$ME" --url "$RPC_URL" --keypair "$WALLET" | awk '{print $1}')
  echo "balance $HAVE SOL, about $NEED_SOL SOL needed at peak"
  awk "BEGIN { exit !($HAVE < $NEED_SOL) }" && fail "not enough SOL in $ME"
  if [ "$CLUSTER" = mainnet ]; then
    read -r -p "Deploy to MAINNET from $ME? Type 'deploy': " OK
    [ "$OK" = deploy ] || fail "cancelled"
  fi

  echo "== deploy"
  while read NAME ID; do
    [ -n "$NAME" ] || continue
    echo "-- $NAME"
    SO=target/deploy/$NAME.so
    SHOW=$(show $ID)
    if [ -n "$SHOW" ]; then
      LEN=$(echo "$SHOW" | awk '/^Data Length:/ {print $3}')
      SIZE=$(stat -c%s "$SO" 2>/dev/null || stat -f%z "$SO")
      if [ "$SIZE" -gt "$LEN" ]; then
        solana program extend $ID $((SIZE - LEN + 10240)) --url "$RPC_URL" --keypair "$WALLET"
      fi
      PROGRAM=$ID
    else
      PROGRAM=target/deploy/$NAME-keypair.json
    fi
    solana program deploy "$SO" --program-id "$PROGRAM" \
      --upgrade-authority "$WALLET" --keypair "$WALLET" \
      --url "$RPC_URL" --max-sign-attempts 50 --with-compute-unit-price 10000 \
      || fail "$NAME failed. Rerun the script: it picks up where it stopped. Leftover buffers: solana program close --buffers --url $RPC_URL --keypair $WALLET"
  done <<< "$TODO"
else
  echo "nothing to deploy"
fi

echo "== hub: set up and approve the models"
cargo run -q -p vortex-smoke -- approve --url "$RPC_URL" --keypair "$WALLET"

echo "== IDLs"
CHANGED=""
for f in target/idl/vortex_*.json; do
  [ -f "$f" ] || continue
  cmp -s "$f" "idl/$(basename $f)" || { cp "$f" idl/; CHANGED="$CHANGED $(basename $f)"; }
done
if [ -n "$CHANGED" ]; then
  echo "updated in idl/:$CHANGED"
  echo "copy them to the bot too: cp idl/*.json <protean-bot>/src/solana/idl/"
else
  echo "idl/ is current"
fi

if [ "$SMOKE" = 1 ]; then
  echo "== smoke test"
  cargo run -q -p vortex-smoke -- run --url "$RPC_URL" --keypair "$WALLET"
fi

echo
echo "== bot settings (Railway) for $CLUSTER"
if [ "$CLUSTER" = devnet ]; then
  echo "# Program IDs default to these addresses on devnet; only the fill program needs setting:"
  echo "${PREFIX}_FILL_PROGRAM_ID=9QavLTD5JqCtemTJ1GLwxqeaByWJrs6LQtawwjtWfw77"
else
  while read NAME ID; do
    [ "$NAME" = vortex_fill ] && { echo "${PREFIX}_FILL_PROGRAM_ID=$ID"; continue; }
    echo "${PREFIX}_$(echo ${NAME#vortex_} | tr a-z A-Z)_PROGRAM_ID=$ID"
  done <<< "$PROGRAMS"
fi
echo "${PREFIX}_RPC_URL=$RPC_URL"
echo
echo "Done. Upgrade authority and hub admin: $ME. Keep that wallet and target/deploy/*-keypair.json safe."
