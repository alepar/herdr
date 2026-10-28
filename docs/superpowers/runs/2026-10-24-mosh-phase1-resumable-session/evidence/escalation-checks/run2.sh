#!/bin/bash
# usage: run2.sh VARIANT ADDR ; env: MUX=0|1 PRE=0|1 (pre-create master) ACT=drop|reject ALIVE="15 4" JUMP=addr|"" KILLAT=8 WATCH=100
set -u
D=$(dirname "$0"); V=$1; A=$2
cfg=$D/cfg-$V; ctl=/tmp/claude-1001/mp-$V; log=$D/tty-$V.log; ev=$D/ev-$V.log
read ai ac <<< "${ALIVE:-15 4}"
printf 'Host *\n  HostKeyAlias localhost\n  ServerAliveInterval %s\n  ServerAliveCountMax %s\n' $ai $ac > $cfg
rm -f $ctl $log $ev
muxargs=""; [ "${MUX:-0}" = 1 ] && muxargs="-S $ctl -o ControlMaster=auto -o ControlPersist=600"
jarg=""; [ -n "${JUMP:-}" ] && jarg="-J ${JUMP}"
t0=$(date +%s); ts(){ echo "t+$(( $(date +%s)-t0 ))s $*" >> $ev; }
[ "${PRE:-0}" = 1 ] && { ssh -C -F $cfg $muxargs $jarg -T $A true 2>/dev/null; ts "pre-created master"; }
inner="while :; do echo hb; sleep 2; done | exec ssh -C -F $cfg $muxargs $jarg -T $A 'echo hi; while :; do sleep 1; date; done' > $D/out-$V.log"
python3 -I $D/ptyrun.py $log "$inner" </dev/null >/dev/null 2>&1 &
sleep 4
leg=$(pgrep -f "^ssh -C -F $cfg .*-T $A" | head -1)
ts "leg0 ssh pid=$leg out-lines=$(wc -l < $D/out-$V.log)"
blk=${JUMP:-$A}
if [ "${ACT:-drop}" = reject ]; then
  sudo nft add rule inet t2 out ip daddr $blk tcp dport 22 reject with tcp reset
  sudo nft add rule inet t2 out ip saddr $blk tcp sport 22 reject with tcp reset
else
  sudo nft add rule inet t2 out ip daddr $blk tcp dport 22 drop
  sudo nft add rule inet t2 out ip saddr $blk tcp sport 22 drop
fi
ts "${ACT:-drop} applied on $blk"
killat=${KILLAT:-8}; last=0; killed=0
for i in $(seq 1 ${WATCH:-100}); do
  sleep 1
  if [ $killed = 0 ] && [ $i -ge $killat ]; then
    if kill -0 $leg 2>/dev/null; then kill -9 $leg; ts "SIGKILL leg0 (herdr 8s silence)"; else ts "leg0 already exited before herdr kill"; fi; killed=1
  fi
  sz=$(stat -c %s $log); [ "$sz" != "$last" ] && { ts "TTY: $(tail -c 300 $log | tr '\r\n' '  ')"; last=$sz; }
done
ts "end"
[ "${MUX:-0}" = 1 ] && ssh -S $ctl -O exit $A 2>/dev/null
pkill -f "^ssh -C -F $cfg"
