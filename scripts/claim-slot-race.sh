#!/usr/bin/env bash
# claim-slot-race.sh -- concurrent callers racing claim_slot's sequence on one
# Redis key. Must report 0 admissions without a slot.
#
# The unit tests in crates/oag-server/src/gateway/select.rs own claim_slot's
# decisions, one call at a time. This owns what they cannot see: many callers
# racing on a real key while a batch of leases expires, which is how oag86-1
# admitted requests holding no member (peak seen: 7 on an 8-seat limit).
#
# claim_slot is now one atomic ACQUIRE_SLOT: the refusal and the live count
# come from the same step, so the count-then-retry window oag86-1 lived in no
# longer exists. The fixed model below is that single call. MODEL=buggy is the
# pre-#86 three-step flow (acquire, separate count, retry, admit on refusal),
# kept so this script can still be shown to fail.
#
# The Lua is read from crates/oag-store/src/cache.rs at run time, so this
# always races the scripts production runs. ACQUIRE_SLOT answers {taken, live};
# the first line of redis-cli's reply is `taken`.
#
#   REDIS_PORT=6399 ./scripts/claim-slot-race.sh      # the fixed code: exits 0
#   MODEL=buggy ./scripts/claim-slot-race.sh          # pre-fix code: exits 1 in most runs (a race)
set -euo pipefail

P="${REDIS_PORT:-6399}"
KEY="${SLOT_KEY:-oag:race:slots}"
LIMIT="${LIMIT:-8}"
TTL=3
N="${CALLERS:-140}"
ROUNDS="${ROUNDS:-10}"
MODEL="${MODEL:-fixed}"
CACHE_RS="$(dirname "$0")/../crates/oag-store/src/cache.rs"

# The body of `const NAME: &str = r"...";` -- the raw string ends at the first
# line holding `";`, which is how both scripts are written.
lua() {
  awk -v name="$1" '
    $0 ~ "^const " name ": &str = r\"" { on = 1; sub(/^[^"]*r"/, ""); }
    on && /";/ { sub(/";.*/, ""); print; exit }
    on { print }
  ' "$CACHE_RS"
}
ACQ="$(lua ACQUIRE_SLOT)"
CNT="$(lua SLOTS_IN_USE)"
[ -n "$ACQ" ] && [ -n "$CNT" ] || { echo "could not read the Lua from $CACHE_RS" >&2; exit 2; }
redis-cli -p "$P" ping >/dev/null || { echo "no Redis on port $P (just dev-up)" >&2; exit 2; }

TOTG=0; TOTA=0
for r in $(seq 1 "$ROUNDS"); do
  OUT=$(mktemp -d)
  redis-cli -p "$P" del "$KEY" >/dev/null
  # Start late in a Redis second: the seeds below expire at the next boundary,
  # a few hundred ms into the burst. Unaligned, the burst often missed the
  # boundary and a round tested nothing.
  while :; do
    T=$(redis-cli -p "$P" time | tr '\n' ' ')
    NOW=${T%% *}; USEC=${T#* }; USEC=${USEC%% *}
    [ "$USEC" -ge 700000 ] && break
    sleep 0.02
  done
  # Seed the key at capacity, scored to expire at that boundary, so the burst
  # straddles a batch of leases expiring together.
  for i in $(seq 1 "$LIMIT"); do
    redis-cli -p "$P" zadd "$KEY" $((NOW - TTL + 1)) "seed-$r-$i" >/dev/null
  done

  acquire() {
    redis-cli -p "$P" eval "$ACQ" 1 "$KEY" "$1" "$LIMIT" "$TTL" | head -1
  }
  caller() {
    local id="r$r-req$1" a1 live a2
    a1=$(acquire "$id")
    if [ "$a1" = "1" ]; then echo admitted > "$OUT/$1"; return; fi
    if [ "$MODEL" != buggy ]; then echo refused > "$OUT/$1"; return; fi
    # Pre-#86: count separately, retry on an empty key, admit on refusal.
    live=$(redis-cli -p "$P" eval "$CNT" 1 "$KEY" "$TTL")
    if [ "$live" -gt 0 ]; then echo refused > "$OUT/$1"; return; fi
    a2=$(acquire "$id")
    if [ "$a2" = "1" ]; then echo admitted > "$OUT/$1"
    else echo admitted_without_slot > "$OUT/$1"; fi
  }
  for i in $(seq 1 "$N"); do caller "$i" & done
  wait

  G=$(cat "$OUT"/* | grep -c '^admitted_without_slot$' || true)
  A=$(cat "$OUT"/* | grep -c '^admitted' || true)
  echo "round $r: admitted=$A  without a slot=$G  final ZCARD=$(redis-cli -p "$P" zcard "$KEY")"
  TOTG=$((TOTG + G)); TOTA=$((TOTA + A))
  rm -rf "$OUT"
done
redis-cli -p "$P" del "$KEY" >/dev/null

echo "limit $LIMIT, $N callers, $ROUNDS rounds, model $MODEL"
echo "admitted: $TOTA   admitted holding no slot: $TOTG (must be 0)"
# A script that admitted nobody tested nothing: a Lua that refused every
# acquire would otherwise pass here.
[ "$TOTA" -gt 0 ] || { echo "nobody was admitted: the race did not run" >&2; exit 1; }
[ "$TOTG" -eq 0 ]
