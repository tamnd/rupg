#!/usr/bin/env python3
"""The `pg_dump` round trip of spec/21 section 21.4.1.

The test runs `schema.sql` on rupg and on PostgreSQL. It dumps the rupg database, restores the dump into a new PostgreSQL database and dumps that database. The dump of rupg, the dump of the restore and the dump of PostgreSQL must be the same text. See README.md.
"""

import argparse
import difflib
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))

# The lines of a dump that change from one run to the next or from one server to the other.
SKIP = ("\\restrict", "\\unrestrict", "-- Dumped from")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_socket(directory, port, proc=None, seconds=120):
    path = os.path.join(directory, ".s.PGSQL.%d" % port)
    end = time.time() + seconds
    while time.time() < end:
        if proc is not None and proc.poll() is not None:
            sys.exit("the server stopped with code %d" % proc.returncode)
        if os.path.exists(path):
            return
        time.sleep(0.2)
    sys.exit("no socket at %s after %d seconds" % (path, seconds))


class Tools:
    """The PostgreSQL programs in one directory."""

    def __init__(self, bindir):
        self.bindir = bindir

    def run(self, program, *args):
        cmd = [os.path.join(self.bindir, program), *args]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            sys.exit("%s failed with code %d:\n%s%s" % (" ".join(cmd), r.returncode, r.stdout[-3000:], r.stderr[-3000:]))
        return r.stdout


def conn(host, port, db):
    return ["-h", host, "-p", str(port), "-U", "postgres", "-d", db]


def dump_text(path):
    with open(path) as f:
        return [line for line in f.read().split("\n") if not line.startswith(SKIP)]


def compare(a, b, labels):
    lines = list(difflib.unified_diff(a, b, labels[0], labels[1], n=1, lineterm=""))
    print("%s and %s: %s" % (labels[0], labels[1], "same" if not lines else "%d lines differ" % len(lines)))
    for line in lines[:200]:
        print(line)
    return not lines


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pg-bin", required=True, help="the bin directory of PostgreSQL 19")
    ap.add_argument("--rupg", required=True, help="the rupg program")
    ap.add_argument("--work", help="a new directory for the files of the test, else a temporary directory")
    ap.add_argument("--oracle", help="HOST:PORT of a running PostgreSQL 19 server, else the test makes one with initdb")
    ap.add_argument("--schema", default=os.path.join(HERE, "schema.sql"))
    args = ap.parse_args()

    work = args.work or tempfile.mkdtemp(prefix="rupg-dumprt-")
    os.makedirs(work, exist_ok=True)
    pg = Tools(args.pg_bin)
    procs = []
    data = None
    # The PostgreSQL databases of the test, with the options to connect to the server. The test drops them at the end.
    made = []
    try:
        if args.oracle:
            ohost, oport = args.oracle.rsplit(":", 1)
            oport = int(oport)
        else:
            data = os.path.join(work, "data")
            ohost = os.path.join(work, "pgsock")
            os.makedirs(ohost)
            oport = free_port()
            pg.run("initdb", "-D", data, "-U", "postgres", "-A", "trust", "-E", "UTF8", "--no-locale", "--no-sync")
            opts = "-p %d -k %s -c listen_addresses=" % (oport, ohost)
            pg.run("pg_ctl", "-D", data, "-l", os.path.join(work, "postgres.log"), "-o", opts, "-w", "start")

        rhost = os.path.join(work, "rupgsock")
        os.makedirs(rhost)
        rport = free_port()
        log = open(os.path.join(work, "rupg.log"), "w")
        rupg = subprocess.Popen(
            [args.rupg, "serve", "--listen", "127.0.0.1:%d" % rport, "-c", "port=%d" % rport, "-c", "unix_socket_directories=" + rhost],
            stdout=log, stderr=subprocess.STDOUT)
        procs.append(rupg)
        wait_socket(rhost, rport, rupg)

        # The names of the PostgreSQL databases have the port of rupg, so two runs on one server do not meet.
        source = "dumprt_%d_source" % rport
        restored = "dumprt_%d_restored" % rport
        server = conn(ohost, oport, "postgres")[:-2]
        for db in (source, restored):
            pg.run("dropdb", "--if-exists", *server, db)
            pg.run("createdb", *server, db)
            made.append((server, db))

        psql = ["-X", "-q", "-v", "ON_ERROR_STOP=1", "-f", args.schema]
        pg.run("psql", *psql, *conn(rhost, rport, "postgres"))
        pg.run("psql", *psql, *conn(ohost, oport, source))

        files = {name: os.path.join(work, name) for name in ("rupg.sql", "rupg.dump", "restored.sql", "postgres.sql")}
        pg.run("pg_dump", "-f", files["rupg.sql"], *conn(rhost, rport, "postgres"))
        pg.run("pg_dump", "-Fc", "-f", files["rupg.dump"], *conn(rhost, rport, "postgres"))
        pg.run("pg_restore", "--exit-on-error", *conn(ohost, oport, restored), files["rupg.dump"])
        pg.run("pg_dump", "-f", files["restored.sql"], *conn(ohost, oport, restored))
        pg.run("pg_dump", "-f", files["postgres.sql"], *conn(ohost, oport, source))

        a = dump_text(files["rupg.sql"])
        print("dump of rupg: %d lines" % len(a))
        same = compare(a, dump_text(files["restored.sql"]), ("rupg", "restored"))
        same = compare(a, dump_text(files["postgres.sql"]), ("rupg", "postgres")) and same
        return 0 if same else 1
    finally:
        for server, db in made:
            subprocess.run([os.path.join(args.pg_bin, "dropdb"), "--if-exists", *server, db], capture_output=True)
        for p in procs:
            p.terminate()
            p.wait()
        if data is not None:
            subprocess.run([os.path.join(args.pg_bin, "pg_ctl"), "-D", data, "-m", "fast", "-w", "stop"], capture_output=True)
        if not args.work:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
