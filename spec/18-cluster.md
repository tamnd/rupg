# 18. The cluster

Written 7 October 2026.

This document specifies how many rupg nodes act as one PostgreSQL server. It covers the node roles, the catalog, shards and placement, replication with one Raft group for each shard, time, distributed transactions, distributed queries, rebalancing, failure testing, and the arithmetic behind the two scale-out targets of document 02: TPC-C at least 0.8 N times one node for N = 2, 4, 8 and 16 (G11), and ClickBench at least 0.7 N times one node for N up to 8. The crates are `rupg-raft` and `rupg-cluster` (document 22). Replication is milestone M10 and shards are M11. Document 04 puts the cluster beside the stack: the lower layers define traits, the single node has trivial implementations from M1, and this document gives the cluster implementations.

## 18.1 What the cluster is

A cluster is a set of nodes that run the same `rupg` binary with the `cluster` feature. There is no coordinator tier and no router. A client connects to any node, and that node is the coordinator for the session. Each node has one `.rupg` file, which holds the replicas of the shards placed on the node and their Raft logs. A node is a normal rupg server, and a cluster of one node is a normal single node.

The single-node code calls three traits. This sketch gives their shape. The real signatures are in `rupg-txn`, `rupg-log` and `rupg-catalog`.

```rust
pub trait Placement {
    fn shards_of(&self, table: Oid, key: Option<&KeyRange>) -> ShardSet;
    fn leader(&self, shard: ShardId) -> NodeId;
}

pub trait LogShip {
    fn append(&self, shard: ShardId, block: &CommitBlock) -> Position;
    fn safe(&self, shard: ShardId) -> Position;
}

pub trait Committer {
    fn commit(&self, tx: &mut Txn) -> Result<Hlc, Error>;
}
```

On a single node, `shards_of` returns shard 0, `leader` returns the local node, `append` writes to the worker's ring and `safe` is the safe position of document 11 section 11.14.2. `commit` runs the five steps of document 11 section 11.4.2. In a cluster, each trait has an implementation in `rupg-cluster` that this document specifies.

**Decision: the cluster shards the tables of document 10 as they are.** CockroachDB and YugabyteDB put a key-value layer under the tables, and their ClickBench hot sums on `c6a.4xlarge` are 10,965.71 s and 51,701.64 s (document 01). A shard in rupg is a set of hot B+trees and cold segments.

## 18.2 Nodes, membership and the catalog

**The catalog is shard 0.** Shard 0 is a Raft group on 3 or 5 nodes, chosen at cluster creation. It holds the system catalogs, the shard map, the node list, the roles and the settings that `ALTER SYSTEM` writes.

**Joining.** `rupg cluster join <address>` adds a node to the node list. It holds no shard until the placement moves shards to it (section 18.10). `rupg cluster leave` moves every shard off a node and then removes it.

**OIDs.** User OIDs come from the counter in the catalog (document 04), so an OID is unique in the cluster. A node takes OIDs from shard 0 in batches of 1,024, which is a budget, so DDL does not take a Raft round for each OID.

**DDL uses schema leases.** A node may use a catalog version only while it holds a lease on it. A DDL statement commits a new version on shard 0 and waits until every node has moved to it or its lease has expired. Between two versions, at most two versions are in use, and each DDL change that needs it is split into steps that are safe with two versions in use. This is the online schema change of F1 (Rae and others, PVLDB 6, 2013). The lease is 10 s, which is a budget.

**Transactional DDL** is a distributed transaction that includes shard 0. `CREATE TABLE` followed by `INSERT` in one block commits both, or neither, with the protocol of section 18.6.

## 18.3 Shards and placement

### 18.3.1 Distributing a table

A table is local until the user distributes it. Two functions in the schema `rupg` do it.

| Function | Effect |
|---|---|
| `rupg.rupg_distribute(table regclass, key text, colocate_with regclass DEFAULT NULL)` | splits the table into shards by a hash of `key` |
| `rupg.rupg_replicate(table regclass)` | makes the table a reference table with a full copy on every node |

A local table has one shard. The placement puts it on shard 0 by default, so a cluster that runs an unchanged PostgreSQL schema works, with every table in one Raft group. A table distributed with `colocate_with` gets the same hash ranges and the same placement as the other table. A join or a foreign key on the distribution key of two colocated tables is local to each shard. TPC-C distributes every table except `item` by warehouse id, colocated, and replicates `item`.

**The hash.** The key value is hashed to 64 bits with the hash of the type's hash opclass, which is the hash that PostgreSQL's hash partitioning uses. A shard owns one range of hash values. Range placement on the key value is an option of `rupg_distribute`.

### 18.3.2 Shard numbers and row ids

A row id has the shard number in its high 16 bits (document 10 section 10.2). The shard bits are the shard where the row was first inserted. A row keeps its row id when its shard moves or splits, so `ctid` and every index entry stay valid. A new shard gets a number that no live row uses, so row ids never collide. A cluster has at most 65,536 shards at one time, which is the 16-bit limit of document 04.

**Splits.** A shard splits when it passes 4 GiB of data or when its leader passes a load threshold, and both are budgets. A split is a Raft entry at one log index. After it, two shards cover the two halves of the hash range. At first the two shards share the parent's cold segments and read them with a filter on the hash range. The mover of document 10 section 10.6 writes new segments for each half later.

### 18.3.3 Reference tables

A reference table has a replica on every node, so a join with it is local. A write to it is a transaction on its own Raft group, whose members are all the nodes. Reads must not wait on that group, so a write commits at a timestamp in the future, greater than now plus `rupg.max_clock_offset`, and the writer waits until its own clock passes that timestamp before it acknowledges. A read at any node at a timestamp below the newest closed timestamp of the group is then exact with no uncertainty. This is the design of CockroachDB's global tables. A write then costs at least `rupg.max_clock_offset`, which suits tables that change rarely, such as `item` in TPC-C.

## 18.4 Replication

### 18.4.1 One Raft group for each shard

Each shard has one Raft group (Ongaro and Ousterhout, USENIX ATC 2014) with a replication factor of 3 by default, set by `rupg.replication_factor`.

**The Raft log is the shard's log.** Document 11 has one log ring for each worker. In a cluster, the commit blocks of a shard must have one order for Raft. Each shard leader has a sequencer: a worker that commits a transaction on the shard puts its block on the shard's queue, and the sequencer appends a batch of queued blocks as one Raft entry. The entry is written to the leader's ring and sent to the followers in parallel. A commit is safe when a quorum has the entry durable. The safe-prefix rule of document 11 section 11.14.2 holds inside one shard, and across shards the protocol of section 18.6 replaces it.

**Durability levels.** `synchronous_commit` maps to Raft as document 11 section 11.14.3 gives. With `local`, a failover can lose an acknowledged commit, as with asynchronous standbys in PostgreSQL.

**Followers.** A follower appends entries to its ring and applies them to its copy of the shard with the recovery code of document 11 section 11.16. A follower that falls behind the start of the leader's retained log gets a snapshot: the cold segments, which are immutable and are copied as files, and a dump of the hot B+tree at a timestamp.

### 18.4.2 Leaders and failover

**Leases.** A leader holds a lease, so a read at the leader needs no Raft round. A new leader waits until the old lease has expired before it serves. Lease safety depends on a clock rate bound and not on synchronized clocks.

**Timers.** The heartbeat interval is 100 ms and the election timeout is 1 s, both budgets. A failed leader is replaced within 3 s, which is a budget that M10 measures. A transaction in progress on the failed leader fails with `40001`. A session whose node failed gets `08006`.

**Leader placement.** The placement spreads leaders evenly over nodes.

### 18.4.3 Replicas without shards

M10 comes before M11, so M10 has one shard on 3 or 5 nodes. A node outside the leader serves reads. With `rupg.node_role = replica`, a node holds a replica that never becomes leader, `pg_is_in_recovery()` returns true, and a write fails with `25006`, as on a PostgreSQL standby. A node with the default role forwards a write to the leader and runs reads at the closed timestamp.

`pg_stat_replication` on the leader has one row for each follower, with the LSN columns of document 11 section 11.12 and the lag intervals.

### 18.4.4 Follower reads

A leader publishes a closed timestamp: a promise that it will accept no new commit at or below it. The leader closes a timestamp 200 ms behind its clock, every 100 ms, and both are budgets. A follower that has applied the log up to a closed timestamp serves reads at that timestamp locally. `SET TRANSACTION READ ONLY` with `rupg.follower_reads = on` uses it, and the session sees data that is at most a few hundred milliseconds old.

## 18.5 Time

**HLC.** Every node runs the hybrid logical clock of document 04: 48 bits of milliseconds and 16 bits of logical counter. Every message between nodes carries the sender's HLC, and the receiver moves its clock forward to it. So causality through rupg is always ordered.

**Snapshots.** A transaction that touches one shard takes its snapshot on the shard leader, with the `visible` word of document 11 section 11.3, and it has no clock problem. A transaction that touches more shards takes its snapshot from the coordinator's HLC. This is Clock-SI (Du, Elnikety and Zwaenepoel, SRDS 2013).

**Uncertainty.** A client can commit on node A and then, through another channel, tell a client on node B to read. If B's clock is behind A's, B's snapshot could miss the commit. rupg bounds the clock offset by `rupg.max_clock_offset`, default 250 ms, which is a budget. A read that finds a version with a timestamp above its snapshot but within the offset of it cannot know the order, so the read restarts at a higher timestamp.

**What a restart means to the client.** In `READ COMMITTED`, the statement restarts in the server, and the client sees nothing. In `REPEATABLE READ` and `SERIALIZABLE`, the transaction restarts in the server if it has returned no row to the client. Otherwise it fails with `40001`. PostgreSQL never fails a read-only `REPEATABLE READ` transaction, so this is a difference from PostgreSQL, and document 05 lists it for the cluster. Document 24 asks how often it happens in the client suites.

**Tight bounds.** When the node has a clock bound service, such as AWS ClockBound over the Amazon Time Sync Service, rupg uses the bound that the service reports in place of `rupg.max_clock_offset`. A node whose clock is more than the offset away from the others, as Raft heartbeats measure it, stops serving and logs an error.

## 18.6 Distributed transactions

### 18.6.1 One shard

A transaction whose writes are all on one shard commits with one Raft round on that shard. The coordinator sends the commit to the shard leader, which runs the steps of document 11 section 11.4.2 with `append` going to the shard's Raft group.

### 18.6.2 More than one shard

The coordinator runs two-phase commit, and the prepare records go into the Raft logs of the shards. This follows Mako (Shen, Cui, Sen, Angel and Mu, OSDI 2025) and CockroachDB's parallel commits (Taft and others, SIGMOD 2020).

1. The coordinator takes a proposed commit timestamp from its HLC.
2. It sends a prepare to the leader of every shard it wrote. The leader checks the transaction's locks, moves its proposed timestamp up if the shard has served a read above it, and appends a prepare entry with the transaction's records, its final shard timestamp and the list of all participant shards.
3. The transaction is committed when every prepare entry is durable on a quorum. Its commit timestamp is the largest of the shard timestamps.
4. The coordinator acknowledges the client, and then sends the decision to each shard, which appends a commit entry in the background.

**The decision is not replicated.** Step 3 is a fact about the Raft logs, not a record. If the coordinator fails, any node can read the prepare entries of all participants, listed in each one, and decide: committed if every participant has a durable prepare, aborted if one has none and cannot get one. So commit costs one replication round, not two. This is the point of Mako, which reports 3.66 million TPC-C transactions per second on 10 shards (document 01).

**Readers wait on prepared rows.** A reader at a timestamp above a prepared row's proposed timestamp waits until the decision is known. The wait is bounded by one Raft round after the last prepare.

**Locks.** Row locks are taken at the leader of the row's shard, as document 11 section 11.7 gives. They are held through the prepare and released at the commit entry, so a distributed transaction holds its locks for about one replication round longer than a local one. Section 18.9 gives the cost for TPC-C.

**User two-phase commit.** `PREPARE TRANSACTION` runs steps 1 and 2, and `COMMIT PREPARED` runs steps 3 and 4.

### 18.6.3 Isolation in the cluster

**Snapshot isolation** is the default, as document 02 requires. `REPEATABLE READ` gives first-updater-wins on each shard, as document 11 section 11.5.2 gives.

**Serializable** uses timestamp ordering in place of the SSI of document 11 section 11.5.3. Each shard leader keeps a read timestamp cache: for each key range read, the highest timestamp of a reader. A write below a cached read timestamp moves its transaction's timestamp up. At commit, a transaction whose timestamp moved must validate its reads: it checks that no write committed in its read set between its first and its final timestamp. If one did, the transaction fails with `40001` "could not serialize access due to read/write dependencies among transactions". This is commit-time read validation as in CockroachDB. The read cache is in memory and is bounded at 64 MiB for each node, which is a budget. When it fills, it merges ranges and keeps the higher timestamp.

The two methods prevent the same anomalies but can choose different victims, so the isolation specs get cluster alternate files at M11, and document 05 counts them.

### 18.6.4 Deadlocks, sequences, advisory locks and LISTEN

**Deadlocks.** Each node searches its local wait-for graph as document 11 section 11.8.3 gives. A transaction that waits more than `deadlock_timeout` on a remote shard sends its wait edges to the shard 0 leader, which searches the global graph and fails the transaction that sent the edge that closed the cycle, with `40P01`.

**Sequences.** A sequence lives on shard 0. Each node fetches values in blocks of the sequence's `CACHE` value and at least 32, which is a budget, so `nextval()` rarely takes a round. Values are unique but not ordered across nodes, as with `CACHE` greater than 1 in PostgreSQL.

**Advisory locks** live on shard 0, so they are global, and each acquisition costs one round to the shard 0 leader.

**LISTEN and NOTIFY.** A notification is sent to every node at commit, and each node delivers it to its local listeners.

## 18.7 Distributed queries

**Plans.** The optimizer of document 15 asks the placement for the shards of each table and adds exchange operators: gather to the coordinator, broadcast, and repartition by hash. The coordinator sends one plan fragment to each node that holds a leader or, for follower reads, a replica of a needed shard.

**Point routing.** A statement with an equality on the distribution key goes to one shard. When the coordinator holds the shard leader, the statement runs on the point path of document 14 section 14.9 with no network step.

**Aggregates.** Each node computes partial aggregates over its shards, and the coordinator merges them. A group key that contains the distribution key is local to one shard, so its partials need no merge. `COUNT(DISTINCT x)` where `x` is the distribution key is the sum of the local counts.

**Wire format between nodes.** Exchange operators send vectors in the compressed forms of document 09, so a column that is a constant or a run in a segment stays small on the wire. Segment scalars of document 09 section 9.7 and segment summaries of document 12 section 12.6 prune segments on each node before any byte is sent. Every connection between nodes uses TLS 1.3 with mutual authentication by a cluster certificate authority.

**Strings across nodes.** The global string codes of document 09 section 9.5 are global in one file. In a cluster, each node has its own dictionary for each table, so a code is not valid on another node. An exchange sends a string with a 64-bit hash for partitioning and the bytes for exactness. For top-k on a string key, a node sends counts with local codes and hashes first, and the coordinator asks for the bytes of the winners only. A dictionary that is shared across the cluster would remove the bytes, at the cost of a coordination step at every new string, and document 24 keeps it open.

## 18.8 ClickBench on N nodes: the arithmetic

### 18.8.1 The target

Document 20 shards `hits` by a hash of `UserID` over N `c6a.4xlarge` nodes, and the client connects to one node. The ratio is T1 divided by N times T_N, where T1 is the hot sum on one node and T_N the hot sum on N nodes. It must be at least 0.7. So T_N must be at most T1 / (0.7 N).

This section takes T1 as the single-node target of 1.753 s (document 02). The ideal T_N is T1 / N. The allowance is the difference: T1 / (0.7 N) minus T1 / N, which is 0.42857 times T1 / N.

| N | Target T_N | Ideal T1 / N | Allowance | Allowance per query, 43 queries |
|---|---|---|---|---|
| 2 | 1,252.1 ms | 876.5 ms | 375.6 ms | 8.74 ms |
| 4 | 626.1 ms | 438.2 ms | 187.8 ms | 4.37 ms |
| 8 | 313.0 ms | 219.1 ms | 93.9 ms | 2.18 ms |

The allowance falls as 1 / N, so N = 8 decides the design. If T1 is below the target, the allowance shrinks with it.

### 18.8.2 Costs that do not divide

**Coordination.** Each query sends its plan to N nodes, starts the fragments and gathers small results. The budget is 0.5 ms for each query, so 21.5 ms for 43 queries.

**C4, the point query.** Q19 finds one `UserID`. The distribution key routes it to one node, so it takes its 1 ms budget of document 02 on any N. Against the ideal of 1/8 ms, it costs 0.875 ms more at N = 8.

**C1 and C9, the short queries.** C1 (Q0 to Q6) has 14 ms and C9 (Q36 to Q42) has 35 ms on one node (document 02). We assume that half of their time does not divide, which is a prediction. At N = 8 that costs 24.5 ms times 7/8, which is 21.4 ms more than the ideal.

**Total.** At N = 8 the costs that do not divide are 21.5 plus 0.9 plus 21.4, which is 43.8 ms. That leaves 50.1 ms of the 93.9 ms allowance for the classes below. At N = 4 they are 40.6 ms of 187.8 ms.

### 18.8.3 C3: top-k on large keys

C3 is Q15 to Q18 and Q30 to Q35, with 700 ms on one node (document 02). With 70 ms per query, a node at N = 8 has 8.75 ms for its share.

**Keys that contain `UserID` are local.** Q15 groups by `UserID`, Q16 and Q17 by `UserID` and `SearchPhrase`, and Q18 by `UserID`, a minute and `SearchPhrase`. Every group is on one node, so the global top 10 is the top 10 of the nodes' top 10 lists. These four queries divide by N with no exchange cost.

**Other keys use certified synopses on each node.** Q30 to Q35 group by `ClientIP`, `WatchID`, `URL` or `SearchEngineID` with `ClientIP`. Each node holds the synopses of document 12 section 12.8 for its own rows. The merge is the threshold algorithm of Cao and Wang (PODC 2004) with the certificate:

1. Each node runs its candidates and sends the candidate values with exact local counts, and its bound B_i on any value outside its candidate set.
2. The coordinator forms the union C of all candidate values. Each node counts the values of C that it did not send, exactly, from row values, and sends those counts.
3. The coordinator sums the counts of each value of C. Let c_k be the k-th largest sum. A value outside C has a count of at most the sum of the B_i. If c_k is strictly greater than that sum, the top k of C is the answer.

Each step sends a few thousand values for each node. Counts come from row values, as document 12 section 12.8 requires. Each B_i bounds only 1/N of the rows, so for a key spread evenly over `UserID` the sum stays near the single-node bound.

**When the certificate fails, the fallback is a shuffle.** The worst key is `URL`, with 27,374,884 distinct values (document 09). Each distinct URL is a partial aggregate on at least one node. With a 64-bit hash and an 8-byte count, the partials are at least 438 MB in total, and with the strings they are more. At N = 8, a node sends 7/8 of its partials, at least 47.9 MB. AWS lists the network of `c6a.4xlarge` as "up to 12.5 Gigabit", which is 1.5625 GB/s at most, so the shuffle takes at least 30.7 ms. That is 3.5 times the 8.75 ms share of the query, and it uses 22 ms of extra time against the 50.1 ms that section 18.8.2 leaves. An "up to" rate is a burst rate in AWS documentation, so the sustained rate must be measured.

**Consequence.** At N = 8, the cluster target holds only if the certificate of document 12 section 12.8 holds for every C3 query on every node, as it must on one node for G7. One fallback shuffle at N = 8 uses most of the slack. The cluster target therefore depends on H3, and M11 reports the certificate result of each C3 query for each N.

### 18.8.4 C5 and C7: work for each distinct value

C5 (Q20 to Q23, `LIKE` on `URL` and `Title`) has 240 ms, and C7 (Q27 and Q28) has 200 ms on one node (document 02). On one node, part of this work is done once for each distinct value: the gram summaries of document 12 section 12.9 test each dictionary entry, and the function evaluation of document 14 section 14.6 runs once for each distinct value.

A node in the cluster has its own dictionary. Let s(N) be the sum over the N nodes of the distinct values on each node, divided by the distinct values of the whole table. s(1) is 1, and s(N) is at most N. So the work for each distinct value does not divide by N. It divides by N / s(N).

Let d be the share of a query's single-node time T that is work for each distinct value. The extra time on N nodes is d times T times (s(N) minus 1) / N. With d = 0.5, which is a prediction, and T = 440 ms for C5 and C7 together, the extra at N = 8 is 27.5 ms times (s(8) minus 1). Against the 50.1 ms that remains, s(8) must be at most 2.82, and that holds only if C3 has no fallback. At N = 4 the limit is s(4) at most 3.68, and at N = 2 every possible s(2) passes.

**s(N) must be measured before M11.** Hash `UserID` of `hits` into 8 buckets, count the distinct `URL`, `Title` and `Referer` values in each, and sum. If s(8) for `URL` is above about 2.8, the target at N = 8 needs a dictionary that is shared across the cluster or a smaller d. Document 24 holds this question.

### 18.8.5 Summary

| Item at N = 8 | Time |
|---|---|
| ideal T1 / 8 | 219.1 ms |
| coordination, 43 queries | 21.5 ms |
| C4 that does not divide | 0.9 ms |
| C1 and C9 halves that do not divide | 21.4 ms |
| C3 with every certificate holding | 0 ms extra |
| C5 and C7, d = 0.5, s(8) = 2 | 27.5 ms |
| total | 290.4 ms |
| target | 313.0 ms |

The total is a prediction. It meets the target with 22.6 ms to spare, under three conditions: every C3 certificate holds, s(8) is near 2, and the coordination stays at 0.5 ms for each query. Each condition is measured at M11, and the run states its load case as document 20 section 20.3 requires. Each node holds about 1.2 GB of `hits`, from rudb's 9.65 GB divided by 8 (document 02). Section 21.8 of document 21 runs the suite with all summaries deleted, and the cluster run takes the same test, where it is expected to miss the target.

## 18.9 TPC-C on N nodes: the arithmetic

### 18.9.1 The distributed share

The rules are those of TPC-C revision 5.11. A New-Order has 5 to 15 items, and each item comes from a remote warehouse with probability 0.01 (clause 2.4.1.5). The chance that a New-Order touches a remote warehouse is the mean of 1 minus 0.99 to the power n over n = 5 to 15, which is 0.0952. A Payment is for a customer of a remote warehouse with probability 0.15 (clause 2.5.1.2). The mix gives Payment at least 43 percent and three other types at least 4 percent each (clause 5.2.3), so New-Order is about 45 percent. Order-Status, Delivery and Stock-Level are always local.

The share of transactions that touch a remote warehouse is 0.45 times 0.0952 plus 0.43 times 0.15, which is 0.1073. With warehouses spread evenly over N nodes, a remote warehouse is on another node with probability about (N minus 1) / N. So the distributed share is f(N) = 0.1073 times (N minus 1) / N.

### 18.9.2 What the gate allows

Let a local transaction cost 1 unit of node CPU and a distributed one cost k units, counted over all the nodes it touches. The efficiency is E = 1 / (1 + f (k minus 1)). G11 needs E of at least 0.8, so f (k minus 1) must be at most 0.25.

| N | f(N) | Largest k for E of 0.8 | E at k = 1.2 | E at k = 2 |
|---|---|---|---|---|
| 2 | 0.0537 | 5.66 | 0.989 | 0.949 |
| 4 | 0.0805 | 4.11 | 0.984 | 0.926 |
| 8 | 0.0939 | 3.66 | 0.982 | 0.914 |
| 16 | 0.1006 | 3.48 | 0.980 | 0.909 |

**The expected k.** G6 is about 90,000 NOPM for each core (document 02), which is 1,500 New-Orders each second, or 3,333 transactions of all types at a 45 percent New-Order share. So a transaction costs about 300 µs of CPU at the 10x target. A distributed transaction adds a prepare message to each remote shard, a prepare entry in each Raft log, and a decision message. We predict about 60 µs of CPU for these, which is k = 1.2 and E of 0.98 at N = 16. Even at k = 2, E stays above 0.9. CPU is not the risk.

### 18.9.3 The risk: locks held across the network

A remote Payment updates the home warehouse row and district row, which every Payment of that warehouse updates. A local transaction releases its row locks at install, with early lock release (document 11 section 11.4.2). A distributed one holds them through the prepare round, which we predict at about 300 µs in one placement group. Chardonnay measured two-phase commit over Paxos at about 150 µs in one Azure datacenter (document 01), so 300 µs leaves room.

On the `oltp` machine of document 20, with 32 cores at the G6 rate, a node runs about 106,667 transactions each second, so about 45,867 Payments. Of those, 15 percent hold a warehouse lock for about 300 µs, which is 2.06 s of lock time each second over all warehouses of the node. With W warehouses on the node, the lock utilization of one warehouse row is 2.06 / W. With at least 100 warehouses on each node it is near 0.02, so waits are rare, and the gate run must keep at least 100.

### 18.9.4 Replication and the gate

The ratio of G11 compares N nodes with one node, so both sides must have the same replication. With a replication factor of 3, each node applies the log of the two other replicas of its shards. If a follower applies a transaction for 15 percent of the leader's cost, which is a budget, a node's capacity falls to 1 / (1 + 2 times 0.15), which is 0.77 of a node with no replicas. Against one node with no replication, G11 at factor 3 would fail on that cost alone.

**Decision: G11 is measured at a replication factor of 1 on both sides,** with the durability of document 11 on each node. A second row is published at factor 3, against a single shard of 3 nodes as the base. Document 24 asks the owner of document 02 to confirm this reading.

**Network.** A transaction writes about 300 bytes of log, which is a prediction for M5 to measure. At 106,667 transactions each second, of which about 92 percent write, a node writes about 29 MB of log each second, and at factor 3 sends about 59 MB each second to followers.

## 18.10 Rebalancing and splits

**Moving a shard.** The placement moves a replica of a shard from node A to node B in four steps.

1. Copy the shard's cold segments from A to B. Segments are immutable, so the copy needs no lock and no snapshot of rows. It is a file copy at `rupg.rebalance_rate`, default 200 MB/s for each node, which is a budget.
2. Add B to the shard's Raft group as a learner, which receives the log and does not vote. B receives a snapshot of the hot B+tree at a timestamp and then the log from that timestamp.
3. When B is within one second of the leader, change the membership with joint consensus to add B and remove A.
4. If A was the leader, transfer leadership to another voter first.

Writes continue during all four steps. The only pause is the leadership transfer in step 4, and the budget for it is 500 ms. M11 measures the longest write pause during a move under TPC-C load.

**When to move.** The placement moves shards when a node joins or leaves, when one node holds more than 1.2 times the mean data or the mean load of the nodes, and when a split creates a new shard. All three numbers are budgets. `rupg.rebalance_rate` limits the copy so that a move costs the running workload less than 10 percent of its throughput, which document 14 section 14.11 also bounds.

**Locality.** Shards of colocated tables move together.

## 18.11 Failure testing

**Jepsen and Elle.** From M10, Jepsen runs on 3 to 16 nodes with partitions, kills, pauses, clock skew and membership changes, and Elle (Kingsbury and Alvaro, PVLDB 14, 2020) checks the histories of list-append and register workloads for every anomaly. The bank workload checks that a total does not change, and a TPC-C workload checks the consistency conditions of TPC-C clause 3.3. A Jepsen failure blocks M10 and M11 (document 21).

**Deterministic simulation.** `rupg-raft` and `rupg-cluster` use only the platform traits for time, network and disk, so the simulator of document 21 runs many nodes in one process with seeded faults. Every simulation run checks its history with Elle. A failing seed replays exactly.

**Clock faults.** The simulator moves clocks by up to twice `rupg.max_clock_offset`, and no history may show an anomaly.

## 18.12 What we take from other systems

| System | Source | What we take | What we leave |
|---|---|---|---|
| Mako | OSDI 2025 | prepares in the Raft logs, decision without a replication round | its own storage engine |
| CockroachDB | SIGMOD 2020 | parallel commits, read timestamp cache, closed timestamps, global tables | the key-value layer under the tables |
| Spanner | OSDI 2012 | commit wait with a clock bound, for reference tables only | waits on every commit |
| F1 | PVLDB 6, 2013 | online schema change with leases | |
| Aurora DSQL | arXiv 2607.13276 | coordinate only at commit, a target for point latency | optimistic concurrency only, snapshot isolation only |
| Chardonnay | OSDI 2023 | a measured base for two-phase commit in one datacenter | |
| Tiga | SOSP 2025 | future timestamps from synchronized clocks, after M11 | |
| Calvin, Detock | SIGMOD 2012, SIGMOD 2023 | nothing | they need the read and write set before a transaction runs, which a PostgreSQL session does not give |
| Neon, BtrLog | no paper, VLDB 2026 | a log tier apart from storage, as an option after M11 | |

**DSQL's numbers are the target for point latency.** The DSQL paper gives a p99 of about 2 ms for a primary-key `SELECT`, about 3 ms for an `UPDATE`, 7.4 ms for a commit in one region, and less than 1.5 ms at p99 for a read-only transaction (document 01). A rupg cluster in one placement group must be faster on each, and M11 publishes the same four numbers.

**Cloudspecs (Steinert, Kuschewski and Leis, CIDR 2026)** found that network bandwidth per dollar rose about 10x from 2015 to 2025, while instance NVMe has not improved since 2016 (document 01). So a second node adds network and memory at a better price than a larger disk.

**Backup and point-in-time recovery** in a cluster take a consistent cut at one HLC timestamp. Each shard leader writes a backup of its shard as of that timestamp, with the method of document 11 section 11.18, and `recovery_target_time` restores every shard to the same timestamp. **Logical replication out of a cluster** merges the streams of all shards in commit timestamp order, up to the lowest closed timestamp of all shards, so a subscriber sees transactions in one order.
