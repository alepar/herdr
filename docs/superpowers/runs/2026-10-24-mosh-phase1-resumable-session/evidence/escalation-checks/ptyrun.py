# usage: ptyrun.py LOG CMD  -- runs CMD via sh with stderr (and controlling tty) on a fresh pty; logs pty output with timestamps
import os, sys, time, pty, select
log, cmd = sys.argv[1], sys.argv[2]
m, s = pty.openpty()
pid = os.fork()
if pid == 0:
    os.setsid()
    import fcntl, termios
    fcntl.ioctl(s, termios.TIOCSCTTY, 0)
    os.dup2(s, 2)
    os.close(m)
    os.execvp("sh", ["sh", "-c", cmd])
os.close(s)
t0 = time.time()
with open(log, "ab", buffering=0) as f:
    while True:
        try:
            r, _, _ = select.select([m], [], [], 1)
            if r:
                data = os.read(m, 4096)
                if not data: break
                f.write(b"[t+%.1fs] " % (time.time() - t0) + data + b"\n")
        except OSError:
            break
