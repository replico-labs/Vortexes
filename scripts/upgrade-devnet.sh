#!/usr/bin/env bash
# Upgrades the eleven devnet programs in place from target/deploy/ (run `anchor build` first).
# WALLET is the programs' upgrade authority; it also pays for the temporary buffers.
set -eo pipefail
WALLET=${WALLET:-~/.config/solana/id.json}

while read NAME ID; do
  echo "== $NAME"
  solana program deploy target/deploy/$NAME.so \
    --program-id $ID \
    --upgrade-authority $WALLET --keypair $WALLET \
    --url devnet
done <<'LIST'
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
LIST
