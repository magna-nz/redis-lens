#!/usr/bin/env bash
# Keeps the demo servers busy so the live screens have something to show: sessions and
# carts being written and updated, reads, deletes, a queue, a stream, Pub/Sub messages
# and the odd slow command.
#
#   make demo-up        start and seed the demo servers first
#   make demo-traffic   run this; ctrl-c stops it
#
# Usage: demo/traffic.sh [redis|valkey|both]      (default: both)
#        RATE=40 demo/traffic.sh redis             (commands a second per server, default 15)
#
# It only talks to the two demo containers (redis-lens-demo-redis-1 as the demo `admin`
# user, redis-lens-demo-valkey-1), through `docker exec`, so it needs Docker and nothing
# else. The password is the one in demo/users.acl, which is for this demo only. The demo
# data is throwaway: `make demo-down` deletes it.
set -euo pipefail
set -m # each background job gets its own process group, so ctrl-c can stop all of it

target="${1:-both}"
rate="${RATE:-15}"
case "$target" in redis | valkey | both) ;; *)
  echo "usage: $0 [redis|valkey|both]" >&2
  exit 2
  ;;
esac
pause="$(awk -v r="$rate" 'BEGIN { if (r < 1) r = 1; printf "%.3f", 1 / r }')"

redis_c=redis-lens-demo-redis-1
valkey_c=redis-lens-demo-valkey-1

running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" = true ]; }
want=()
[ "$target" != valkey ] && want+=("$redis_c")
[ "$target" != redis ] && want+=("$valkey_c")
for c in "${want[@]}"; do
  running "$c" || {
    echo "$c is not running: start the demo with 'make demo-up'." >&2
    exit 1
  }
done

# One command per line, for redis-cli to read from stdin. Quotes are redis-cli's own.
gen() {
  local tick=0 id n ip key
  local -a recent=()
  echo "CLIENT SETNAME demo-traffic"
  while true; do
    tick=$((tick + 1))
    id=$((RANDOM % 20000 + 1))
    n=$((RANDOM % 40))
    case $((RANDOM % 13)) in
      0) # a new session, kept in a list of recent ones so some are read back
        key="session:$(printf '%08x-%04x' $((RANDOM * RANDOM)) "$RANDOM")"
        recent+=("$key")
        [ "${#recent[@]}" -gt 60 ] && recent=("${recent[@]:1}")
        echo "SET $key '{\"user_id\":$id,\"roles\":[\"member\"],\"cart\":$((RANDOM % 9))}' EX 1800"
        ;;
      1) # read a recent session back
        if [ "${#recent[@]}" -gt 0 ]; then echo "GET ${recent[RANDOM % ${#recent[@]}]}"; else echo "GET feature:checkout-v2"; fi
        ;;
      2) # update a cart; three in ten are written without a TTL, like the planted problem
        echo "HSET user:$id:cart sku:$((1000 + n)) '{\"qty\":$((RANDOM % 5 + 1)),\"price\":$((RANDOM % 90 + 5)).99}'"
        [ $((RANDOM % 10)) -ge 3 ] && echo "EXPIRE user:$id:cart 604800"
        ;;
      3) echo "HGETALL user:$id:cart" ;;
      4) # a rate limiter
        ip="203.0.$((RANDOM % 4)).$((RANDOM % 250 + 1))"
        echo "INCR rl:$ip:login"
        echo "EXPIRE rl:$ip:login 60"
        ;;
      5) # a work queue: mostly in, sometimes out
        if [ $((RANDOM % 4)) -eq 0 ]; then echo "RPOP queue:emails"; else echo "LPUSH queue:emails '{\"template\":\"order-shipped\",\"order\":$((RANDOM % 90000))}'"; fi
        ;;
      6) echo "XADD events:orders MAXLEN '~' 1000 '*' type order.created order $((RANDOM % 90000)) total $((RANDOM % 400 + 10))" ;;
      7) # Pub/Sub
        if [ $((RANDOM % 3)) -eq 0 ]; then echo "PUBLISH cache.invalidate 'user:$id:profile'"; else echo "PUBLISH orders.created '{\"id\":$((RANDOM % 90000))}'"; fi
        ;;
      8) # a feed that keeps its last hundred
        echo "ZADD user:$id:feed $(date +%s) post:$((RANDOM % 90000))"
        echo "ZREMRANGEBYRANK user:$id:feed 0 -101"
        ;;
      9) echo "GET feature:checkout-v2" ;; # the hot key
      10) # a flag that gets updated
        echo "SET feature:demo-flag $((RANDOM % 100))"
        ;;
      11) # deletes
        if [ $((RANDOM % 2)) -eq 0 ]; then echo "DEL user:$id:cart"; else echo "UNLINK user:$id:feed"; fi
        ;;
      12) echo "EXPIRE user:$id:cart 3600" ;;
    esac
    # about once in a hundred commands, something that shows up in the slow log
    [ $((tick % 100)) -eq 99 ] && echo "EVAL 'local x = 0 for i = 1, 4000000 do x = x + i end return x' 0"
    sleep "$pause"
  done
}

# The Pub/Sub subscribers run inside the containers and write their pid to a file there,
# so they can be stopped even if this script is killed and run again.
subs_pid_file=/tmp/lens-demo-subs.pid
stop_subs() {
  docker exec "$1" sh -c "kill \$(cat $subs_pid_file 2>/dev/null) 2>/dev/null; rm -f $subs_pid_file; true" >/dev/null 2>&1 || true
}

pids=()
cleanup() {
  trap - INT TERM EXIT
  for pid in "${pids[@]:-}"; do [ -n "$pid" ] && kill -- "-$pid" 2>/dev/null || true; done
  for c in "${want[@]}"; do stop_subs "$c"; done
  echo
  echo "stopped."
}
trap cleanup INT TERM HUP EXIT

cli_for() { if [ "$1" = "$redis_c" ]; then echo "redis-cli --user admin --pass admin-demo-only --no-auth-warning"; else echo "valkey-cli"; fi; }

for c in "${want[@]}"; do
  cli="$(cli_for "$c")"
  # two subscribers, so Pub/Sub lists channels and has someone to deliver to
  stop_subs "$c"
  docker exec "$c" sh -c "echo \$\$ >> $subs_pid_file; exec $cli psubscribe 'orders.*'" >/dev/null 2>&1 &
  pids+=("$!")
  docker exec "$c" sh -c "echo \$\$ >> $subs_pid_file; exec $cli subscribe cache.invalidate" >/dev/null 2>&1 &
  pids+=("$!")
  # shellcheck disable=SC2086
  (gen | docker exec -i "$c" $cli >/dev/null 2>&1) &
  pids+=("$!")
  echo "sending about $rate commands a second to $c"
done
echo "ctrl-c stops it."
wait
