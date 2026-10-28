# ProxyJump teardown spike: mirrors spawn_leg (stdin/stdout/stderr piped), SIGKILLs ssh, times stderr EOF + jump proc exit.
import subprocess, sys, time, threading, os, signal
cfg, jump, target, drop = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "drop"
cmd = ["ssh", "-C", "-F", cfg, "-o", "BatchMode=yes", "-o", "ControlPath=none", "-J", jump, "-T", target, "echo READY; cat"]
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
print("ready line:", p.stdout.readline().strip(), flush=True)
def kids(pid):
    out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
    return [int(x) for x in out]
jumps = kids(p.pid)
print("ssh pid", p.pid, "jump children", [(j, open(f"/proc/{j}/cmdline").read().replace("\0"," ")) for j in jumps], flush=True)
eof = {}
def drain():
    data = p.stderr.read(); eof["t"] = time.time(); eof["data"] = data
th = threading.Thread(target=drain, daemon=True); th.start()
if drop:
    for rule in (f"ip daddr {jump} tcp dport 22 drop", f"ip saddr {jump} tcp sport 22 drop"):
        subprocess.run(["sudo","nft","add","rule","inet","pjtest","out"] + rule.split(), check=True)
    time.sleep(3)
t0 = time.time(); p.kill(); p.wait()
print("ssh reaped after %.2fs" % (time.time()-t0), flush=True)
alive_since = None
for i in range(1500):  # up to 150 s
    alive = [j for j in jumps if os.path.exists(f"/proc/{j}") and open(f"/proc/{j}/stat").read().split()[2] != "Z"]
    if "t" in eof and not alive: break
    time.sleep(0.1)
print("stderr EOF:", ("%.2fs" % (eof["t"]-t0)) if "t" in eof else "NOT after %.0fs" % (time.time()-t0), "| stderr:", eof.get("data", b"")[:300], flush=True)
print("jump procs alive at end:", alive, "ppid:", [open(f"/proc/{j}/stat").read().split()[3] for j in alive], flush=True)
for j in alive: os.kill(j, signal.SIGKILL)
