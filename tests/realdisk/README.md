# Crash tests on real kernels

These tests do spec/21 section 21.9.3. The T8 test of `tests/crash` uses a model of the disk, and the model can be wrong about a real file system. These tests run the same workload on real file systems on Linux. They run each night in the `realdisk` job of `.github/workflows/nightly.yml`.

Each test uses two modes of `rupg-crash`:

- `rupg-crash workload --file F --seed S --steps N` runs the seeded workload of the T8 test on the file `F` with the operating system. After each acknowledged commit or new table it prints `ack <n> <digest>`. The digest is a checksum of the tables and rows. Before each change it prints `pending <n> <digest>`.
- `rupg-crash verify --file F --expect D1,D2` opens the file, which runs recovery, reads every table and runs `rupg check`. The digest of the tables and rows must be one of the given digests.

After a crash, the file must hold the last acknowledged change, or that change and the next one. A crash before the first acknowledgement is not checked, because a cut during the first open can leave a file that is not a database.

## The tests

| Script | What it does | Needs |
|---|---|---|
| `logwrites.py --fs ext4` and `--fs xfs` | Runs the workload on a dm-log-writes device, with a mark in the log after each acknowledged change. Then it replays the log on a copy of the image, and checks the file at flush, FUA and mark entries and at points between them. | root, dm-log-writes, dm-snapshot |
| `kill.py` | Kills the workload with `SIGKILL` at a random point in each round and checks the file. The data that was not synced stays in the page cache, so this tests recovery at a high rate. | nothing |
| `kill.py --lazyfs` | The same rounds on a LazyFS mount. After each kill, LazyFS drops the data that was not synced. | FUSE, LazyFS |

Spec/21 asks for the `SIGKILL` test during TPC-C. At M1 there is no SQL, so the test uses the key-value workload. TPC-C replaces it when the SQL layer is ready.

## dm-log-writes

The test makes the image files in the work directory, attaches them to free loop devices, and makes the file system before the log starts. The replay starts from a copy of the image after `mkfs`. At each check point the test makes a dm-snapshot of the replay image and mounts the snapshot, so the recovery of the file system writes to the snapshot and not to the replay image.

The check points are a seeded sample, half from the flush, FUA and mark entries and half from the other entries, and always the last entry. A write that completed before a point and was not flushed can be on the disk after a power cut, so a point between flushes is also a state that a crash can give.

```sh
cargo build --release -p rupg-crash
sudo modprobe dm-log-writes
sudo python3 tests/realdisk/logwrites.py --bin target/release/rupg-crash --fs ext4 --work /var/tmp/rupg-lw --seed 1
```

The test removes its mounts, device mapper devices and loop devices at the end. After a pass it also removes the images. After a failure it keeps them, and `workload.txt` in the work directory has the output of the workload.

## LazyFS

Build LazyFS 0.3.1 from <https://github.com/dsrhaslab/lazyfs>. It needs the fuse3 headers, CMake and a C++17 compiler:

```sh
sudo apt-get install libfuse3-dev fuse3 cmake g++
git clone --depth 1 --branch 0.3.1 https://github.com/dsrhaslab/lazyfs
(cd lazyfs/libs/libpcache && ./build.sh)
(cd lazyfs/lazyfs && ./build.sh)
python3 tests/realdisk/kill.py --bin target/release/rupg-crash --work /var/tmp/rupg-lazyfs --lazyfs lazyfs/lazyfs/build/lazyfs
```

The test does not need root. The workload runs as the same user as LazyFS, so the mount does not need the option `allow_other`.
