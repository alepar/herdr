#!/bin/bash
# usage: run.sh VARIANT ADDR MUX(0|1) RESTORE_AT(0=never)
set -u
D=$(dirname "$0"); V=$1; A=$2; MUX=$3; RESTORE=$4
cfg=$D/cfg-$V; ctl=/tmp/claude-1001/mp-$V; log=$D/tty-$V.log; ev=$D/ev-$V.log
printf 'Host *\n  HostKeyAlias localhost\n  ServerAliveInterval 15\n  ServerAliveCountMax 4\n' > $cfg
rm -f $ctl $log $ev
muxargs=""; [ "$MUX" = 1 ] && muxargs="-S $ctl -o ControlMaster=auto -o ControlPersist=600"
marker="moshprobe-$V"
inner="sleep 600 | ssh -C -F $cfg $muxargs -T $A 'echo $marker; while :; do sleep 1; date; done' > $D/out-$V.log"
t0=$(date +%s)
ts(){ echo "t+$(( $(date +%s)-t0 ))s $*" >> $ev; }
python3 -I $D/ptyrun.py $log "$inner" </dev/null >/dev/null 2>&1 &
sp=$!
sleep 5; ts "connected? out=$(wc -l < $D/out-$V.log) procs: $(pgrep -af "ssh .*-T $A" | wc -l)"
sudo nft add rule inet moshtest out ip daddr $A tcp dport 22 drop
sudo nft add rule inet moshtest out ip saddr $A tcp sport 22 drop
ts "DROP applied"
sleep 8
cli=$(pgrep -f "ssh -C -F $cfg.*-T $A" | while read p; do ps -o args= -p $p | grep -q "^ssh -C" && [ "$(ps -o ppid= -p $p | tr -d ' ')" != 1 ] && echo $p; done | head -1)
ts "SIGKILL client pid=$cli; all ssh procs for $A: $(pgrep -af "ssh -C -F $cfg" | tr '\n' ';')"
kill -9 $cli 2>/dev/null
last=0
for i in $(seq 1 120); do
  sleep 1
  sz=$(stat -c %s $log); [ "$sz" != "$last" ] && { ts "TTY OUTPUT (size $sz): $(tail -c 300 $log | tr '\r\n' '  ')"; last=$sz; }
  if [ "$RESTORE" != 0 ] && [ $(( $(date +%s)-t0 )) -ge $RESTORE ] && [ ! -e $D/restored-$V ]; then
    for h in $(sudo nft -a list chain inet moshtest out | grep "$A " | sed 's/.*handle //'); do sudo nft delete rule inet moshtest out handle $h; done
    touch $D/restored-$V; ts "RESTORED link"
  fi
done
ts "end; remaining ssh procs: $(pgrep -af "ssh -C -F $cfg" | tr '\n' ';')"
pkill -f "ssh -C -F $cfg"; kill $sp 2>/dev/null
