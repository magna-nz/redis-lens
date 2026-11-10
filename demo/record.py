#!/usr/bin/env python3
"""Drive redis-lens in a pseudo-terminal and record an asciinema v2 cast.

Usage: demo/record.py <out.cast> <steps.json> -- <command...>

steps.json is a list of [seconds_to_wait, "keys"]. Keys use names in braces:
{enter} {esc} {up} {down} {left} {right} {tab}, and {paste:TEXT} sends TEXT as a
bracketed paste. Everything else is typed as-is.

Render the cast with `agg out.cast out.gif`.
"""

import fcntl
import json
import os
import pty
import re
import select
import struct
import sys
import termios
import time

COLS, ROWS = int(os.environ.get("REC_COLS", "120")), int(os.environ.get("REC_ROWS", "34"))
KEYS = {
    "enter": "\r",
    "esc": "\x1b",
    "up": "\x1b[A",
    "down": "\x1b[B",
    "right": "\x1b[C",
    "left": "\x1b[D",
    "tab": "\t",
}


def encode(keys: str) -> bytes:
    out = []
    for m in re.finditer(r"\{paste:(.*?)\}|\{(\w+)\}|([^{]+|\{)", keys):
        if m.group(1) is not None:
            out.append("\x1b[200~" + m.group(1) + "\x1b[201~")
        elif m.group(2) is not None:
            out.append(KEYS[m.group(2)])
        else:
            out.append(m.group(3))
    return "".join(out).encode()


def main() -> None:
    sep = sys.argv.index("--")
    out_path, steps_path, cmd = sys.argv[1], sys.argv[2], sys.argv[sep + 1 :]
    steps = json.load(open(steps_path))

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.execvp(cmd[0], cmd)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    start = time.monotonic()
    events = []

    def pump(until: float) -> bool:
        while True:
            left = until - time.monotonic()
            if left <= 0:
                return True
            r, _, _ = select.select([fd], [], [], min(left, 0.05))
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                events.append([round(time.monotonic() - start, 4), "o", data.decode("utf-8", "replace")])

    alive = True
    for wait, keys in steps:
        if not alive:
            break
        alive = pump(time.monotonic() + wait)
        if alive and keys:
            os.write(fd, encode(keys))
    if alive:
        pump(time.monotonic() + 1.0)
    try:
        os.kill(pid, 15)
    except ProcessLookupError:
        pass

    with open(out_path, "w") as f:
        f.write(json.dumps({"version": 2, "width": COLS, "height": ROWS, "env": {"TERM": "xterm-256color"}}) + "\n")
        for e in events:
            f.write(json.dumps(e) + "\n")
    print(f"wrote {out_path}: {len(events)} events, {events[-1][0] if events else 0:.1f}s")


if __name__ == "__main__":
    main()
