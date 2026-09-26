#!/usr/bin/env bash
# claim-slot-race.sh -- concurrent callers racing claim_slot's sequence on one
# Redis key. Must report 0 admissions without a slot.
#
# The unit tests in crates/oag-server/src/gateway/select.rs own claim_slot's
# decisions, one call at a time. This owns what they cannot see: many callers
# interleaving on a real key while a batch of leases expires, which is how
# oag86-1 admitted requests holding no member (peak seen: 7 on an 8-seat limit).
#
# The Lua is read from crates/oag-store/src/cache.rs at run time, so this
# always races the scripts production runs. The caller below is a second copy
# of claim_slot's control flow (acquire, count, retry); if claim_slot changes
# shape, change `caller` with it.
#
#   REDIS_PORT=6399 ./scripts/claim-slot-race.sh      # the fixed code: exits 0
#   MODEL=buggy ./scripts/claim-slot-race.sh          # pre-fix code: exits 1
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

  # claim_slot: acquire; on refusal count; on an empty key retry once.
  caller() {
    local id="r$r-req$1" a1 live a2
    a1=$(redis-cli -p "$P" eval "$ACQ" 1 "$KEY" "$id" "$LIMIT" "$TTL")
    if [ "$a1" = "1" ]; then echo admitted > "$OUT/$1"; return; fi
    live=$(redis-cli -p "$P" eval "$CNT" 1 "$KEY" "$TTL")
    if [ "$live" -gt 0 ]; then echo refused > "$OUT/$1"; return; fi
    a2=$(redis-cli -p "$P" eval "$ACQ" 1 "$KEY" "$id" "$LIMIT" "$TTL")
    if [ "$a2" = "1" ]; then echo admitted > "$OUT/$1"
    elif [ "$MODEL" = buggy ]; then echo admitted_without_slot > "$OUT/$1"
    else echo refused > "$OUT/$1"; fi
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
[ "$TOTG" -eq 0 ]
