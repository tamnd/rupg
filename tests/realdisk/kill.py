#!/usr/bin/env python3
"""The SIGKILL and LazyFS tests of spec/21 section 21.9.3.

Each round runs the workload of rupg-crash on a new file, kills the process with SIGKILL at a random point, and runs `rupg-crash verify`. The tables must hold the acknowledged changes, or those and the change that was in progress. With `--lazyfs`, the file is on a LazyFS mount, and the test drops the data that was not synced after each kill, as a power cut does. See README.md.
"""

import argparse
import os
import random
import signal
import subprocess
import sys
import time

CONFIG = """[faults]
fifo_path="{fifo}"
fifo_path_completed="{done}"
[cache]
apply_eviction=false
[cache.simple]
custom_size="1gb"
blocks_per_page=1
[filesystem]
log_all_operations=false
logfile=""
"""


class LazyFs:
    """A LazyFS mount of `root` on `mnt`, and its two fault pipes."""

    def __init__(self, binary, work):
        self.root = os.path.join(work, "root")
        self.mnt = os.path.join(work, "mnt")
        self.fifo = os.path.join(work, "faults.fifo")
        done = os.path.join(work, "done.fifo")
        for d in (self.root, self.mnt):
            os.makedirs(d, exist_ok=True)
        for f in (self.fifo, done):
            if not os.path.exists(f):
                os.mkfifo(f)
        config = os.path.join(work, "lazyfs.toml")
        with open(config, "w") as f:
            f.write(CONFIG.format(fifo=self.fifo, done=done))
        # The handles stay open while LazyFS runs, and close() closes them.
        self.log = open(os.path.join(work, "lazyfs.log"), "w")  # noqa: SIM115
        self.proc = subprocess.Popen(
            [binary, self.mnt, "--config-path", config, "-o", "modules=subdir", "-o", f"subdir={self.root}", "-f"],
            stdout=self.log,
            stderr=subprocess.STDOUT,
        )
        # LazyFS opens the pipe of finished faults for writing when it starts, so this open waits for it.
        self.done = open(done)  # noqa: SIM115
        for _ in range(600):
            if os.path.ismount(self.mnt):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("LazyFS did not mount")

    def clear_cache(self):
        """Drops the data that was not synced, and waits until LazyFS reports that it is done."""
        with open(self.fifo, "w") as f:
            f.write("lazyfs::clear-cache\n")
        line = self.done.readline().strip()
        if line != "finished::clear-cache":
            raise RuntimeError(f"LazyFS gave {line!r} after clear-cache")

    def close(self):
        subprocess.run(["fusermount3", "-u", self.mnt], check=False)
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.done.close()
        self.log.close()


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--bin", required=True, help="the rupg-crash binary")
    p.add_argument("--work", required=True, help="a directory for the files, which the test makes")
    p.add_argument("--lazyfs", help="the LazyFS binary. Without it, the files are in the work directory.")
    p.add_argument("--rounds", type=int, default=100)
    p.add_argument("--first-seed", type=int, default=1)
    p.add_argument("--steps", type=int, default=200)
    a = p.parse_args()

    os.makedirs(a.work, exist_ok=True)
    fs = LazyFs(a.lazyfs, a.work) if a.lazyfs else None
    where = fs.mnt if fs else a.work
    name = "LazyFS" if fs else "SIGKILL"
    checked = skipped = 0
    try:
        for seed in range(a.first_seed, a.first_seed + a.rounds):
            rng = random.Random(seed)
            file = os.path.join(where, f"db-{seed}.rupg")
            # The kill comes after a random number of output lines and a random wait of up to 20 ms, so that it can fall inside a commit.
            target = rng.randint(0, a.steps * 2)
            proc = subprocess.Popen([a.bin, "workload", "--file", file, "--seed", str(seed), "--steps", str(a.steps)], stdout=subprocess.PIPE, text=True)
            lines = []
            for line in proc.stdout:
                lines.append(line)
                if len(lines) >= target:
                    time.sleep(rng.random() * 0.02)
                    proc.send_signal(signal.SIGKILL)
                    break
            lines += proc.stdout.readlines()
            proc.wait()
            if fs:
                fs.clear_cache()
            ack, pending = {}, {}
            for line in lines:
                word = line.split()
                if word[0] in ("ack", "pending"):
                    (ack if word[0] == "ack" else pending)[int(word[1])] = word[2]
            if not ack:
                # The kill came during the first open. The file can be a part of a database, and the user gets an error.
                skipped += 1
            else:
                j = max(ack)
                expect = [ack[j]] + ([pending[j + 1]] if j + 1 in pending else [])
                r = subprocess.run([a.bin, "verify", "--file", file, "--expect", ",".join(expect)], capture_output=True, text=True, check=False)
                if r.returncode != 0:
                    raise RuntimeError(f"seed {seed}, killed after ack{j}: {r.stderr.strip()}")
                checked += 1
            if os.path.exists(file):
                os.remove(file)
            if (seed - a.first_seed + 1) % 20 == 0:
                print(f"{name}: {checked} rounds pass, {skipped} killed before the first acknowledgement", flush=True)
        print(f"{name}: PASS, {checked} rounds checked, {skipped} killed before the first acknowledgement", flush=True)
    except RuntimeError as e:
        print(f"{name}: FAIL: {e}", file=sys.stderr, flush=True)
        sys.exit(1)
    finally:
        if fs:
            fs.close()


if __name__ == "__main__":
    main()
