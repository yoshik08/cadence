#!/usr/bin/env bash
# Lyrics-page screenshot harness (no Spotify playback). Usage:
#   scripts/demo.sh <out-dir> <WxH> <start-pos-ms> <expanded 0|1> <karaoke 0|1> "<ms:name.png,...>"
# Build first: cargo build --features karaoke,smoke
out=$1; size=$2; pos=$3; exp=$4; kar=$5; shots=$6
art=$(curl -s "https://open.spotify.com/oembed?url=https://open.spotify.com/track/${DEMO_ID:-09CtPGIpYB4BrO8qb1RGsF}" | sed -n 's/.*"thumbnail_url":"\([^"]*\)".*/\1/p')
shots_abs=$(echo "$shots" | tr ',' '\n' | sed "s|:|:$out/|" | paste -sd, -)
CADENCE_DEMO=1 CADENCE_SIZE=$size CADENCE_DEMO_POS=$pos CADENCE_EXPANDED=$exp CADENCE_KARAOKE=$kar \
CADENCE_DEMO_TITLE="${DEMO_TITLE:-Sorry}" CADENCE_DEMO_ARTIST="${DEMO_ARTIST:-Justin Bieber}" CADENCE_DEMO_ALBUM="${DEMO_ALBUM:-Purpose (Deluxe)}" \
CADENCE_DEMO_URI="spotify:track:${DEMO_ID:-09CtPGIpYB4BrO8qb1RGsF}" CADENCE_DEMO_DURATION=${DEMO_DUR:-200786} CADENCE_DEMO_ART="$art" \
CADENCE_SHOTS="$shots_abs" ${EXE:-target/debug/cadence.exe}
