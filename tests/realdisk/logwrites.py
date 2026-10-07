#!/usr/bin/env python3
"""The dm-log-writes test of spec/21 section 21.9.3 on ext4 or XFS.

The test makes a file system on a loop device, puts the dm-log-writes target over it, and runs the workload of rupg-crash on it. The workload puts a mark in the log after each acknowledged change. Then the test replays the log on a copy of the first image, entry by entry. At each check point it mounts a snapshot of the copy, which runs the recovery of the file system, and runs `rupg-crash verify`. The tables must hold the changes up to the last mark, or those and the next change.

The test needs root, the modules dm-log-writes and dm-snapshot, and mkfs for the file system. It uses only loop devices on files in the work directory, and it removes its devices and mounts at the end. See README.md.
"""

import argparse
import os
import random
import shutil
import struct
import subprocess
import sys
import time

MAGIC = 0x6A736677736872
FLUSH, FUA, DISCARD, MARK = 1, 2, 4, 8


def sh(*args, check=True, capture=False):
    r = subprocess.run(args, check=False, text=True, capture_output=capture)
    if check and r.returncode != 0:
        err = r.stderr if capture else ""
        raise RuntimeError(f"{' '.join(args)} failed with status {r.returncode}: {err}")
    return r.stdout.strip() if capture else r.returncode


def entries(path):
    """The entries of a dm-log-writes log, each one (flags, sector, offset of the data, length of the data, mark), with the sector and the lengths in bytes."""
    with open(path, "rb") as f:
        magic, version, count, sector_size = struct.unpack("<QQQI", f.read(28))
        if magic != MAGIC or version != 1:
            raise RuntimeError(f"{path} is not a dm-log-writes log")
        out = []
        pos = sector_size
        for _ in range(count):
            f.seek(pos)
            head = f.read(sector_size)
            sector, nr, flags, data_len = struct.unpack("<QQQQ", head[:32])
            mark = head[32 : 32 + data_len].decode() if flags & MARK else None
            length = nr * sector_size
            out.append((flags, sector * sector_size, pos + sector_size, length, mark))
            pos += sector_size + (0 if flags & DISCARD else length)
        return out


def apply(log, image, entry):
    """Writes one entry of the log to the replay image. A discard writes zeros, as a loop device reads zeros after a discard."""
    flags, offset, data, length, _ = entry
    done = 0
    while done < length:
        n = min(length - done, 1 << 20)
        buf = bytes(n) if flags & DISCARD else os.pread(log, n, data + done)
        os.pwrite(image, buf, offset + done)
        done += n


def acks(path):
    """The digests of the workload output: the acknowledged models by number, and the pending models by number."""
    ack, pending = {}, {}
    with open(path) as f:
        for line in f:
            word = line.split()
            if word[0] in ("ack", "pending"):
                (ack if word[0] == "ack" else pending)[int(word[1])] = word[2]
    return ack, pending


class Devices:
    """The loop devices, device mapper names and mounts of a run, so that the end of the run can remove them in reverse order."""

    def __init__(self):
        self.stack = []

    def loop(self, path, read_only=False):
        dev = sh("losetup", "-f", "--show", *(["-r"] if read_only else []), path, capture=True)
        self.stack.append(("loop", dev))
        return dev

    def dm(self, name, table):
        sh("dmsetup", "create", name, "--table", table)
        self.stack.append(("dm", name))
        return f"/dev/mapper/{name}"

    def mount(self, dev, path):
        sh("mount", dev, path)
        self.stack.append(("mount", path))

    def pop(self, kind):
        """Removes the newest device of a kind. It must be the newest of all."""
        k, name = self.stack.pop()
        assert k == kind, (k, kind)
        if k == "mount":
            # Another process, such as a scan of the disks, can hold the mount for a short time.
            for wait in range(10):
                if sh("umount", name, check=False) == 0:
                    break
                time.sleep(1 + wait)
            else:
                sh("umount", name)
        elif k == "dm":
            # udev can hold the device for a short time after the unmount.
            if shutil.which("udevadm"):
                sh("udevadm", "settle", check=False)
            sh("dmsetup", "remove", "--retry", name)
        else:
            sh("losetup", "-d", name)

    def close(self):
        while self.stack:
            kind = self.stack[-1][0]
            try:
                self.pop(kind)
            except RuntimeError as e:
                print(f"cleanup: {e}", file=sys.stderr)
                self.stack.pop()


def sectors(path):
    return os.path.getsize(path) // 512


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--bin", required=True, help="the rupg-crash binary")
    p.add_argument("--fs", required=True, choices=["ext4", "xfs"])
    p.add_argument("--work", required=True, help="a directory for the images, which the test makes")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--steps", type=int, default=200)
    p.add_argument("--points", type=int, default=200, help="the largest number of check points")
    a = p.parse_args()

    os.makedirs(a.work, exist_ok=True)
    data, log, replay, cow = (os.path.join(a.work, n) for n in ("data.img", "log.img", "replay.img", "cow.img"))
    mnt = os.path.join(a.work, "mnt")
    os.makedirs(mnt, exist_ok=True)
    tag = f"rupg-{os.getpid()}"
    for path, size in ((data, 512 << 20), (log, 2 << 30), (cow, 256 << 20)):
        with open(path, "wb") as f:
            f.truncate(size)
    dev = Devices()
    try:
        # The workload, with a mark after each acknowledged change. The file system is made before the log starts, so the replay starts from a copy of the image after mkfs.
        data_dev = dev.loop(data)
        sh("mkfs." + a.fs, "-q", "-F" if a.fs == "ext4" else "-f", data_dev)
        sh("cp", "--sparse=always", data, replay)
        log_dev = dev.loop(log)
        dev.dm(f"{tag}-lw", f"0 {sectors(data)} log-writes {data_dev} {log_dev}")
        dev.mount(f"/dev/mapper/{tag}-lw", mnt)
        out = os.path.join(a.work, "workload.txt")
        with open(out, "w") as f:
            r = subprocess.run(
                [a.bin, "workload", "--file", os.path.join(mnt, "db.rupg"), "--seed", str(a.seed), "--steps", str(a.steps), "--mark", f"dmsetup message {tag}-lw 0 mark {{}}"],
                stdout=f,
                check=False,
            )
        if r.returncode != 0:
            raise RuntimeError(f"the workload failed with status {r.returncode}")
        dev.pop("mount")
        dev.pop("dm")
        dev.pop("loop")
        dev.pop("loop")
        ack, pending = acks(out)
        log_entries = entries(log)

        # The check points. Each point is the number of entries to replay. A point before the mark ack0 is not checked, because a cut during the first open can leave a file that is not a database.
        last = {}
        j = None
        for i, e in enumerate(log_entries):
            if e[4] and e[4].startswith("ack"):
                j = int(e[4][3:])
            last[i + 1] = j
        ordered = [i + 1 for i, e in enumerate(log_entries) if e[0] & (FLUSH | FUA | MARK) and last[i + 1] is not None]
        between = [i + 1 for i, e in enumerate(log_entries) if not e[0] & (FLUSH | FUA | MARK) and last[i + 1] is not None]
        rng = random.Random(a.seed)
        half = a.points // 2
        points = rng.sample(ordered, min(len(ordered), half))
        points += rng.sample(between, min(len(between), a.points - len(points)))
        points = sorted(set(points) | {len(log_entries)})
        print(f"{a.fs} seed {a.seed}: {len(ack)} acknowledged changes, {len(log_entries)} log entries, {len(ordered)} flush, FUA and mark entries, {len(points)} check points", flush=True)

        # The replay. The origin of each snapshot is the replay image, and the recovery of the file system writes only to the snapshot.
        cow_dev = dev.loop(cow)
        origin = dev.loop(replay)
        snap = f"{tag}-snap"
        done = 0
        with open(log, "rb") as lf, open(replay, "r+b") as rf:
            for n, point in enumerate(points):
                for e in log_entries[done:point]:
                    apply(lf.fileno(), rf.fileno(), e)
                done = point
                j = last[point]
                expect = [ack[j]] + ([pending[j + 1]] if j + 1 in pending else [])
                dev.dm(snap, f"0 {sectors(replay)} snapshot {origin} {cow_dev} N 8")
                try:
                    dev.mount(f"/dev/mapper/{snap}", mnt)
                except RuntimeError as e:
                    raise RuntimeError(f"point {point}: the file system does not mount: {e}") from e
                r = subprocess.run([a.bin, "verify", "--file", os.path.join(mnt, "db.rupg"), "--expect", ",".join(expect)], capture_output=True, text=True, check=False)
                dev.pop("mount")
                dev.pop("dm")
                if r.returncode != 0:
                    flags = log_entries[point - 1][0]
                    raise RuntimeError(f"point {point} of {len(log_entries)}, after mark ack{j}, entry flags {flags}: {r.stderr.strip()}")
                if (n + 1) % 50 == 0:
                    print(f"{a.fs} seed {a.seed}: {n + 1} of {len(points)} points pass", flush=True)
        print(f"{a.fs} seed {a.seed}: PASS, {len(points)} check points", flush=True)
    except RuntimeError as e:
        print(f"{a.fs} seed {a.seed}: FAIL: {e}", file=sys.stderr, flush=True)
        sys.exit(1)
    finally:
        dev.close()
    for path in (data, log, replay, cow):
        os.remove(path)

if __name__ == "__main__":
    main()
