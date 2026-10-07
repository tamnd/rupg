# 3. Baselines

Written 7 October 2026. ClickBench numbers are from `github.com/ClickHouse/ClickBench` at `e6bda4e` (6 October 2026). PostgreSQL is pinned to `REL_19_STABLE` at `7d3d2db7` (19beta4). rudb is pinned to `cd9f9676` (v0.8.23-1). The counts of systems in this document are the directories of the ClickBench repository at the pin that have a `c6a.4xlarge.json` result file. When a directory has several result files, the newest one is used.

This document gives the numbers that the targets of document 02 are made from. It also says which of them we must measure again at M0 before a gate can use them. A number in this document is a reference. Document 00 says a baseline is a number that we measured on our machine with our harness. So until M0 is done, rupg has no baselines, only the references in this document. Section 3.12 is the list of runs that turns the references into baselines.

## 3.1 The rules for this document

**Every number has a source.** The source is a result file at the pin, a report in `rudb-bench/reports/`, a note in `../2140/`, or a publication. The source is given next to the number.

**A published number is not a baseline.** It tells us where the target is. It does not decide a gate. A gate is decided on numbers from document 20 that we produced on one machine on one day for both sides.

**Versions that the result files do not record are UNVERIFIED.** The ClickBench result files have no version field. The PostgreSQL scripts at the pin default to `PGVERSION=17`, so the PostgreSQL run of 21 September 2026 is most likely PostgreSQL 17, but the file does not say it. The DuckDB run of 11 May 2026 is most likely DuckDB 1.5.2. Both are UNVERIFIED.

**Queries are numbered from 0.** Q0 is the first query in `queries.sql`, as in `../2140/03-baselines.md`. The query classes C1 to C9 are the classes of document 02 section 2.4.3.

## 3.2 The ClickBench board on `c6a.4xlarge`

The machine is an AWS `c6a.4xlarge`: 16 vCPUs of AMD EPYC 7R13, 32 GiB of memory and a 500 GB gp2 EBS volume. The hot sum is the sum over the 43 queries of the faster of the second and third runs. The cold sum is the sum of the first runs. "Bytes" is the `data_size` field of the result file. "QPS at 10" is the `concurrent_qps` field, which the shared driver `lib/benchmark-common.sh` measures with 10 connections for 600 s after the main runs. Engines without a server skip that test and have no value.

| System | Run date | Hot (s) | Cold (s) | Load (s) | Bytes | QPS at 10 |
|---|---|---|---|---|---|---|
| elosdb | 2026-10-02 | 5.21 | 35.12 | 87 | 7,668,470,543 | 4.778 |
| Umbra 26.09 | 2026-09-18 | 7.41 | 47.27 | 163 | 8,301,994,195 | 7.43 |
| intent-gizmosql (tuned) | 2026-10-02 | 7.96 | 59.94 | 73 | 9,158,809,524 | |
| Pivotlake (Parquet) | 2026-10-01 | 10.64 | 101.55 | 0 | 14,779,976,446 | |
| Rayforce | 2026-10-04 | 11.60 | 92.10 | 518 | 59,042,332,197 | 0.775 |
| Firebolt | 2026-08-29 | 15.57 | | 173 | 15,820,423,266 | |
| ClickHouse | 2026-10-06 | 17.53 | 113.72 | 177 | 9,451,237,939 | 1.608 |
| Salesforce Hyper | 2026-09-14 | 19.96 | 126.99 | 101 | 8,829,796,352 | 0.578 |
| DuckDB (in memory) | 2026-05-11 | 22.08 | | 659 | 31,228,624,896 | |
| DuckDB | 2026-05-11 | 26.25 | 115.19 | 126 | 20,457,467,904 | |
| CedarDB | 2026-08-15 | 28.99 | 200.71 | 187 | 8,455,565,312 | 0.18 |
| ClickHouse (Parquet) | 2026-09-16 | 30.37 | | 0 | 14,779,976,446 | |
| DuckDB (Parquet) | 2026-05-11 | 32.48 | 117.67 | 5 | 14,779,976,446 | |
| pg_clickhouse | 2026-05-11 | 33.60 | 147.67 | 284 | 15,301,016,549 | |
| DuckDB (Vortex) | 2026-05-11 | 40.99 | | 141 | 15,731,820,628 | |
| StarRocks | 2026-09-07 | 44.47 | | 489 | 17,944,611,636 | |
| Polars | 2026-08-24 | 45.35 | | 10 | 14,779,976,446 | |
| DataFusion | 2026-08-20 | 45.57 | | 10 | 14,779,976,446 | |
| Doris | 2026-05-10 | 48.43 | | 205 | 13,781,926,015 | |
| pg_duckdb (Parquet) | 2026-05-11 | 52.49 | | 0 | 14,820,640,783 | |
| pg_mooncake | 2026-05-11 | 61.01 | 217.60 | 600 | 14,623,017,634 | |
| Velox | 2026-08-30 | 81.51 | | 11 | 14,780,002,538 | |
| TimescaleDB | 2026-05-10 | 569.40 | 2,755.58 | 2,930 | 19,310,886,912 | |
| Citus | 2026-05-10 | 1,745.08 | | 1,529 | 18,982,258,549 | |
| pg_duckdb (with indexes) | 2025-09-04 | 3,589.08 | | 10,762 | 123,194,382,472 | |
| PostgreSQL (with indexes, tuned) | 2025-03-10 | 4,085.54 | 4,492.52 | 10,357 | 124,386,147,162 | |
| TimescaleDB (no column store) | 2025-12-13 | 8,826.67 | | 2,633 | 72,021,778,432 | |
| CockroachDB | 2025-05-18 | 10,965.71 | | 813 | 67,948,956,585 | |
| pg_duckdb (heap) | 2025-09-04 | 11,524.80 | | 1,003 | 106,482,563,210 | |
| ParadeDB | 2026-09-21 | 11,822.57 | | 2,805 | 88,348,254,208 | |
| PostgreSQL | 2026-09-21 | 11,875.43 | 12,404.05 | 1,004 | 106,489,690,554 | 0.023 |
| OrioleDB | 2025-08-24 | 18,345.67 | | 1,751 | 77,198,718,500 | |
| YugabyteDB | 2025-06-10 | 51,701.64 | | 4,683 | 76,883,037,634 | |

Two older rows from the rudb fork of the repository (`../2140/03-baselines.md` section 3.2) complete the picture. pgrust ran on 1 August 2026 at 34.88 s hot, 134.15 s cold and 223 s load, in 17,563,161,742 bytes. Hyper ran at 21.22 s on 3 September 2026, which the 14 September run above replaces.

There are 149 system directories with a `c6a.4xlarge` result at the pin. 117 of them have a newest result that is complete and not tagged tuned. On the combined metric of the ClickBench web page, elosdb is first at 1.46, Umbra second at 1.74, CedarDB sixth at 2.80, ClickHouse tenth at 3.54, DuckDB sixteenth at 4.77, and PostgreSQL 125th of 132 at 1,510.52. These ranks are from the web page on 6 October 2026 and change with each new result.

**What the board says.** The rows fall into three groups with gaps of about 10x between them. Engines built for analytics on their own format run at 5 s to 50 s. Engines that put a column store or a column executor next to PostgreSQL run at 33 s to 570 s. Engines that keep a row store run at 1,700 s and more. Each group is the cost of one design choice that document 01 section 1.3.5 describes. rupg's target of 1.753 s is 3.0x below the first row.

**Concurrent throughput.** The QPS at 10 connections is a new field and few systems have it. It shows something the hot sum hides. Umbra runs 7.43 queries per second but with an error ratio of 0.547, so more than half of its concurrent queries failed. elosdb runs 4.778 per second with no errors. ClickHouse runs 1.608. CedarDB runs 0.18 with an error ratio of 0.107. PostgreSQL runs 0.023. Document 20 measures this field for rupg and treats a nonzero error ratio as a failed run.

## 3.3 The ClickBench board on `c6a.metal`

The `c6a.metal` has 192 vCPUs and 384 GiB of memory with the same gp2 volume type.

| System | Hot (s) | Cold (s) | Load (s) | Bytes |
|---|---|---|---|---|
| elosdb | 2.93 | 30.27 | 41 | |
| Umbra | 3.80 | 46.48 | 36 | 8,405,171,232 |
| ClickHouse | 4.40 | 84.75 | 135 | 9,510,916,330 |
| CedarDB | 4.52 | 85.91 | 40 | |
| pg_clickhouse | 8.90 | | | |
| DuckDB | 17.32 | 162.90 | 102 | 20,635,725,824 |
| pg_duckdb (Parquet) | 30.50 | | | |
| TimescaleDB | 537.06 | | | |
| PostgreSQL (2026-09-19) | 762.63 | 12,180.59 | 928 | 106,489,690,554 |
| ParadeDB | 918.20 | | | |
| Citus | 1,664.85 | | | |

pg_mooncake failed 13 of the 43 queries on this machine.

**What 12x more cores buys.** ClickHouse goes from 17.53 s to 4.40 s, a factor of 3.98 for 12x the vCPUs. Umbra goes from 7.41 s to 3.80 s, a factor of 1.95. elosdb goes from 5.21 s to 2.93 s, a factor of 1.78. DuckDB goes from 26.25 s to 17.32 s, a factor of 1.52. PostgreSQL goes from 11,875.43 s to 762.63 s, a factor of 15.6, because on `c6a.metal` its 106 GB heap fits in 384 GiB of memory and the run is no longer bound by the disk.

The faster an engine is on `c6a.4xlarge`, the less it gains from more cores. At 5 s for 43 queries, the fixed costs of each query are a large share: parsing, planning, starting the threads, and the parts of each query that do not run in parallel. The metal target of 0.440 s, which is 10.2 ms per query, is mostly a target on those fixed costs. Document 20 measures them on their own (section 20.6).

**PostgreSQL on `c6a.4xlarge` is bound by the disk.** Of the 43 PostgreSQL hot times, 25 are between 258.3 s and 259.5 s (section 3.4). The heap is 106 GB and the machine has 32 GiB of memory, so each query reads most of the heap from the volume again, whatever the query does. So the PostgreSQL hot sum on this machine measures the size of the heap, not the executor. The target against PostgreSQL is 6,774x on hot time and is met as a side effect of the ClickHouse target. Document 02 does not use PostgreSQL as the ClickBench reference for this reason.

## 3.4 Hot time per query

This is the faster of the second and third runs, in seconds, on `c6a.4xlarge`. The result files are `postgresql/results/20260921`, `clickhouse/results/20261006`, `duckdb/results/20260511`, `umbra/results/20260918` and `elosdb/results/20261002`. "Best" is the lowest time for the query in any complete result that is not tagged tuned, and "Holder" is the directory of that result. Section 3.6 explains how to read the best column.

| Q | Class | PG | CH | DuckDB | Umbra | elosdb | Best | Holder |
|---|---|---|---|---|---|---|---|---|
| Q0 | C1 | 258.348 | 0.001 | 0.018 | 0.001 | 0.001 | 0.000 | doris |
| Q1 | C1 | 258.405 | 0.001 | 0.041 | 0.004 | 0.004 | 0.001 | clickhouse |
| Q2 | C1 | 258.393 | 0.034 | 0.074 | 0.009 | 0.008 | 0.000 | rayforce |
| Q3 | C1 | 258.399 | 0.041 | 0.086 | 0.013 | 0.021 | 0.013 | umbra |
| Q4 | C1 | 272.345 | 0.235 | 0.348 | 0.125 | 0.058 | 0.058 | elosdb |
| Q5 | C1 | 278.518 | 0.410 | 0.338 | 0.166 | 0.018 | 0.018 | elosdb |
| Q6 | C1 | 258.363 | 0.009 | 0.030 | 0.001 | 0.006 | 0.000 | rayforce |
| Q7 | C2 | 258.412 | 0.010 | 0.038 | 0.004 | 0.005 | 0.004 | umbra |
| Q8 | C2 | 284.296 | 0.462 | 0.443 | 0.150 | 0.196 | 0.150 | umbra |
| Q9 | C2 | 286.070 | 0.519 | 0.612 | 0.209 | 0.235 | 0.209 | umbra |
| Q10 | C2 | 259.033 | 0.141 | 0.150 | 0.023 | 0.039 | 0.023 | umbra |
| Q11 | C2 | 259.470 | 0.157 | 0.170 | 0.025 | 0.042 | 0.025 | umbra |
| Q12 | C2 | 263.448 | 0.409 | 0.408 | 0.162 | 0.060 | 0.060 | elosdb |
| Q13 | C2 | 267.169 | 0.594 | 0.781 | 0.305 | 0.210 | 0.210 | elosdb |
| Q14 | C2 | 265.007 | 0.469 | 0.473 | 0.180 | 0.067 | 0.067 | elosdb |
| Q15 | C3 | 272.606 | 0.290 | 0.389 | 0.165 | 0.240 | 0.149 | cedardb |
| Q16 | C3 | 438.935 | 1.137 | 0.892 | 0.359 | 0.345 | 0.345 | elosdb |
| Q17 | C3 | 268.767 | 0.423 | 0.659 | 0.186 | 0.140 | 0.028 | opteryx |
| Q18 | C3 | 302.215 | 2.157 | 1.650 | 0.729 | 0.587 | 0.587 | elosdb |
| Q19 | C4 | 258.343 | 0.002 | 0.049 | 0.002 | 0.004 | 0.000 | rayforce |
| Q20 | C5 | 258.435 | 0.387 | 0.711 | 0.136 | 0.103 | 0.103 | elosdb |
| Q21 | C5 | 258.397 | 0.093 | 0.769 | 0.055 | 0.068 | 0.055 | umbra |
| Q22 | C5 | 258.397 | 0.535 | 1.169 | 0.065 | 0.112 | 0.065 | umbra |
| Q23 | C5 | 258.406 | 0.050 | 0.349 | 0.025 | 0.011 | 0.011 | elosdb |
| Q24 | C6 | 258.391 | 0.015 | 0.072 | 0.004 | 0.004 | 0.003 | rayforce |
| Q25 | C6 | 258.414 | 0.189 | 0.166 | 0.010 | 0.014 | 0.002 | mongodb |
| Q26 | C6 | 258.405 | 0.016 | 0.069 | 0.005 | 0.003 | 0.003 | elosdb |
| Q27 | C7 | 258.405 | 0.162 | 0.645 | 0.101 | 0.028 | 0.028 | elosdb |
| Q28 | C7 | 268.473 | 1.512 | 6.478 | 1.371 | 0.403 | 0.403 | elosdb |
| Q29 | C8 | 258.348 | 0.037 | 0.068 | 0.009 | 0.007 | 0.000 | rayforce |
| Q30 | C3 | 262.555 | 0.256 | 0.407 | 0.084 | 0.101 | 0.084 | umbra |
| Q31 | C3 | 265.027 | 0.346 | 0.611 | 0.124 | 0.143 | 0.124 | umbra |
| Q32 | C3 | 459.208 | 2.187 | 2.035 | 0.991 | 1.077 | 0.991 | umbra |
| Q33 | C3 | 347.642 | 1.938 | 2.054 | 0.719 | 0.285 | 0.274 | rayforce |
| Q34 | C3 | 346.650 | 1.926 | 2.198 | 0.717 | 0.286 | 0.274 | rayforce |
| Q35 | C3 | 265.033 | 0.204 | 0.468 | 0.129 | 0.226 | 0.129 | umbra |
| Q36 | C9 | 258.385 | 0.033 | 0.052 | 0.011 | 0.008 | 0.008 | elosdb |
| Q37 | C9 | 258.365 | 0.017 | 0.039 | 0.005 | 0.005 | 0.005 | elosdb |
| Q38 | C9 | 258.370 | 0.022 | 0.039 | 0.003 | 0.005 | 0.003 | cedardb |
| Q39 | C9 | 258.406 | 0.073 | 0.086 | 0.022 | 0.022 | 0.018 | hyper-web |
| Q40 | C9 | 258.400 | 0.012 | 0.041 | 0.003 | 0.005 | 0.002 | hyper |
| Q41 | C9 | 258.408 | 0.010 | 0.038 | 0.003 | 0.009 | 0.003 | cedardb |
| Q42 | C9 | 258.373 | 0.009 | 0.039 | 0.004 | 0.004 | 0.004 | cedardb |
| Sum | | 11,875.43 | 17.53 | 26.25 | 7.41 | 5.21 | 4.539 | |

The result files round to 1 ms. A time of 0.000 means less than 0.5 ms. Rayforce reports 0.000 for Q2, Q6, Q19 and Q29. Q29 is 90 sums over the full table, so a time below 0.5 ms means that Rayforce answers it from data that it computed before the query. We did not check how.

## 3.5 Hot time per class

This table gives the sum of each class in seconds, and the budget of document 02 section 2.4.3.

| Class | Queries | PG | CH | DuckDB | Umbra | elosdb | Budget |
|---|---|---|---|---|---|---|---|
| C1 | Q0 to Q6 | 1,842.771 | 0.731 | 0.935 | 0.319 | 0.116 | 0.014 |
| C2 | Q7 to Q14 | 2,142.905 | 2.761 | 3.075 | 1.058 | 0.854 | 0.200 |
| C3 | Q15 to Q18, Q30 to Q35 | 3,228.638 | 10.864 | 11.363 | 4.203 | 3.430 | 0.700 |
| C4 | Q19 | 258.343 | 0.002 | 0.049 | 0.002 | 0.004 | 0.001 |
| C5 | Q20 to Q23 | 1,033.635 | 1.065 | 2.998 | 0.281 | 0.294 | 0.240 |
| C6 | Q24 to Q26 | 775.210 | 0.220 | 0.307 | 0.019 | 0.021 | 0.090 |
| C7 | Q27, Q28 | 526.878 | 1.674 | 7.123 | 1.472 | 0.431 | 0.200 |
| C8 | Q29 | 258.348 | 0.037 | 0.068 | 0.009 | 0.007 | 0.005 |
| C9 | Q36 to Q42 | 1,808.707 | 0.176 | 0.334 | 0.051 | 0.058 | 0.035 |

**How far each baseline is from each budget.** The next table divides the class sum of the system by the budget. A value of 1.0 or less means that the system already meets the budget for that class.

| Class | CH | Umbra | elosdb |
|---|---|---|---|
| C1 | 52.2 | 22.8 | 8.3 |
| C2 | 13.8 | 5.3 | 4.3 |
| C3 | 15.5 | 6.0 | 4.9 |
| C4 | 2.0 | 2.0 | 4.0 |
| C5 | 4.4 | 1.2 | 1.2 |
| C6 | 2.4 | 0.2 | 0.2 |
| C7 | 8.4 | 7.4 | 2.2 |
| C8 | 7.4 | 1.8 | 1.4 |
| C9 | 5.0 | 1.5 | 1.7 |

ClickHouse spends 62 percent of its hot sum in C3. Umbra spends 57 percent there and elosdb 66 percent. C3 is where the target is won or lost. C6 is the only class where a published system is already well inside the budget, which tells us the budget for C6 has room that the reserve of document 02 can use.

## 3.6 The best published time for each query

The "Best" column of section 3.4 takes the lowest time for each query over the 117 complete untuned results. Its sum is 4.539 s. No real system runs at 4.539 s. It is what a system would get if it was as fast as the fastest system on every query at the same time. The holders are elosdb on 14 queries, Umbra on 12, Rayforce on 7, CedarDB on 4, and Doris, ClickHouse, Opteryx, MongoDB, Hyper and Hyper on the web one each.

The target of 1.753 s is 2.6x below this sum. When we include the results tagged tuned, which are intent-gizmosql and PostgreSQL with indexes, the sum falls to 3.572 s on `c6a.4xlarge`. On `c6a.metal` it is 1.0 s, so the metal target of 0.440 s is 2.3x below it.

| Class | Best sum (s) | Budget (s) | Best over budget |
|---|---|---|---|
| C1 | 0.090 | 0.014 | 6.43 |
| C2 | 0.748 | 0.200 | 3.74 |
| C3 | 2.985 | 0.700 | 4.26 |
| C4 | 0.000 | 0.001 | 0.00 |
| C5 | 0.234 | 0.240 | 0.97 |
| C6 | 0.008 | 0.090 | 0.09 |
| C7 | 0.431 | 0.200 | 2.16 |
| C8 | 0.000 | 0.005 | 0.00 |
| C9 | 0.043 | 0.035 | 1.23 |

**This is the most important table in the folder.** It says where rupg must do something that no published system does. Four classes are beyond the best published time:

- C3 must be 4.3x faster than the best time of any system for each query. The best times of C3 are 0.991 s for Q32 (Umbra), 0.587 s for Q18 (elosdb), 0.345 s for Q16 (elosdb) and 0.274 s each for Q33 and Q34 (Rayforce). These are group-by queries on keys with millions of distinct values. H2 and H3 of document 02 section 2.4.4 must carry this class.
- C1 must be 6.4x faster. Nearly all of the gap is Q4 at 58 ms and Q5 at 18 ms, which are `count(DISTINCT UserID)` and `count(DISTINCT SearchPhrase)` over the whole table. H1 covers Q5 because `SearchPhrase` has a dictionary. Q4 is on a `BIGINT` column with about 17.6 million distinct values, so the set of codes in each segment does not give the answer without a merge of all of them. Document 24 has the question.
- C2 must be 3.7x faster. Q8, Q9 and Q13 are each above 150 ms at best. They group by a small key and count distinct `UserID`, so they have the same problem as Q4 inside each group.
- C7 must be 2.2x faster. Q28 is the regular expression on `Referer`. elosdb already runs it at 0.403 s, against 6.478 s for DuckDB and 1.371 s for Umbra, which suggests that elosdb already runs the expression once for each distinct value, as H6 does.

C5, C6, C8 and C4 are within the budget at the best published time. C9 is 1.2x over. The 268 ms reserve of document 02 is smaller than the excess of C1 alone (76 ms) plus C2 (548 ms). So the reserve cannot absorb a class that misses.

**The rule this gives.** A design lever that only matches the best published system on a class does not reach the target. Each of H1, H2, H3 and H6 must be a different method from the ones the board uses today, and document 20 measures each lever on its class against the "Best" column, not against ClickHouse.

## 3.7 What the per-query times say

**Time is concentrated.** The 16 slowest queries take 84.6 percent of DuckDB's hot sum. Q28 alone is 24.7 percent of DuckDB's and 17.2 percent of Umbra's sum (`../2140/03-baselines.md` section 3.3). In the ClickHouse run, Q18, Q32, Q33, Q34 and Q28 take 9.72 s, which is 55 percent of the sum.

**The substring family shows the value of a better method.** DuckDB over Umbra is 5.3x on Q20, 14.0x on Q21, 18.3x on Q22, 14.5x on Q23 and 5.7x on Q27 (`../2140/03-baselines.md` section 3.4). The gap is method, not hardware, because both ran on the same machine. ClickHouse is between the two.

**The top-N family is cheap in absolute time.** Q19 and Q24 to Q26 show ratios of 14x to 24.5x between systems, but on times of a few milliseconds. These queries do not decide the sum. They decide whether rupg can claim the class budgets of C4 and C6.

**The group-by on a near-unique key is the worst case.** Q32 groups by `WatchID` and `ClientIP` with about 100 million groups. Umbra is only 1.5x ahead of DuckDB there, and the best time on the board is 0.991 s. A top-k synopsis does not help when almost every key is unique and the counts are all 1 or 2. H3 must certify the result from the synopsis bounds. If it cannot, Q32 alone takes more than the C3 budget.

**PostgreSQL's times have no shape.** Every PostgreSQL time is at least 258 s, which is the time for a full read of the heap. Q16, Q18, Q32, Q33 and Q34 are slower, at 302 s to 459 s. They are the queries that group by `UserID`, `WatchID` and `URL`, which are keys with millions of distinct values. PostgreSQL on this machine gives no information about the executor.

## 3.8 The data set

The `hits` table has 99,997,497 rows and 105 columns: 48 `SMALLINT`, 26 `TEXT`, 19 `INTEGER`, 6 `BIGINT`, 3 `TIMESTAMP`, 1 `DATE`, 1 `VARCHAR(255)` and 1 `CHAR` (`../2140/03-baselines.md` section 3.5). Without encoding, the fixed-width columns take 24.8 GB: 9.6 GB of `SMALLINT`, 7.6 GB of `INTEGER`, 4.8 GB of `BIGINT`, 2.4 GB of `TIMESTAMP` and 0.4 GB of `DATE`.

The string columns hold most of the information. These are the distinct counts that rudb measured:

| Column | Distinct values |
|---|---|
| `URL` | 27,374,884 |
| `Referer` | 26,590,624 |
| `URLHash` | 30,106,942 |
| `HID` | 84,728,013 |

The source files are `hits.parquet` at 14,779,976,446 bytes, in 100 partitioned Parquet files at 14,737,666,736 bytes, and the TSV and CSV files. Parquet with Snappy over the full table, written by rudb's test tool, is 13.76 GB.

## 3.9 Disk

| System | Bytes for `hits` | Source |
|---|---|---|
| elosdb | 7,668,470,543 | ClickBench, 2026-10-02 |
| Umbra | 8,301,994,195 | ClickBench, 2026-09-18 |
| CedarDB | 8,455,565,312 | ClickBench, 2026-08-15 |
| Salesforce Hyper | 8,829,796,352 | ClickBench, 2026-09-14 |
| ClickHouse | 9,451,237,939 | ClickBench, 2026-10-06 |
| rudb M1 encoding | 9.65 GB | `../2140/03-baselines.md` section 3.5.1 |
| Parquet with Snappy | 13.76 GB | `../2140/03-baselines.md` section 3.5.1 |
| `hits.parquet` | 14,779,976,446 | ClickBench |
| pgrust | 17,563,161,742 | rudb fork of ClickBench, 2026-08-01 |
| DuckDB | 20,457,467,904 | ClickBench, 2026-05-11 |
| PostgreSQL | 106,489,690,554 | ClickBench, 2026-09-21 |

**Where the bytes are.** In rudb's M1 encoding, `URL`, `Referer` and `OriginalURL` take 4.44 GB of the 9.65 GB, which is 46 percent. Before front coding they took 6.11 GB. Front coding took the whole file from 11.65 GB to 9.65 GB. As a fraction of their Parquet size, the three columns went from 0.99, 0.92 and 1.26 to 0.70, 0.67 and 0.95. The encoding of `URL` is a dictionary whose strings are front coded and then FSST coded, with the codes in a delta, run-length and bit-packed cascade (`../2140/02-the-goal.md` section 2.6.1).

**Where the bytes are not.** rudb tested the rules that link columns. A dictionary shared between `URL` and `Referer` saved 4 KB. The rule from `URL` to `URLHash` fails on 11,586,966 rows. Only `ClientIP` to `IPNetworkID` holds, and it saves 18 MB. So there is no large saving from links between columns in this data set.

**What this means for the gates.** G3 at M4 asks for less than 9.45 GB. rudb's 9.65 GB is 0.2 GB above it without any summaries. G8 at M8 asks for less than 7.67 GB with the summaries of H1 to H6 included. elosdb's file is 7.67 GB and we do not know what it stores. The disk gate is therefore an encoding gate on the three URL columns. Document 09 section 9.5 owns the encoding, and document 20 reports the bytes per column so that the gap is visible.

## 3.10 Memory and CPU

ClickBench publishes no memory and no CPU numbers. So there is no reference for G8, and the first values come from our own runs at M0. The nearest numbers are from rudb at 10 million rows on an Apple M4 with 6 threads (`rudb-bench/reports/2026-09-28/main-2b6efff5-macos-clickbench.md`):

| Measurement | rudb 0.8.5 | DuckDB | Ratio |
|---|---|---|---|
| Hot sum, 43 queries | 2.026 s | 9.300 s | 4.59x |
| Peak resident memory | 287 MiB | 1,144 MiB | 3.99x |
| File size, latest run | 1,400 MiB | 2,242 MiB | 1.60x |
| Peak resident memory of the streaming load (storage v3) | 33.6 MiB | 1,953 MiB | 58x |

A later run on the same day gave 5.51x on the hot sum. These are not on the gate machine and they are at one tenth of the data. The load row shows that a load which streams and does not build the whole table in memory can use 58x less memory than DuckDB. The 10x memory target of G8 on the query runs has no evidence of this size yet.

The 100 million row references from 16 September 2026 on rudb's test server are: DuckDB 1.5.5 at 16.804 s hot, 54.3 s load and 20.45 GB, and ClickHouse at 9.618 s hot, 46.9 s load and 9.64 GB. They show that the test server is about 1.6x to 1.8x faster than `c6a.4xlarge`. They are references only and are not used for any ratio.

## 3.11 TPC-H and JOB

All numbers in this section are rudb's.

| Suite | Measurement | DuckDB | rudb | Ratio | Source |
|---|---|---|---|---|---|
| TPC-H SF1 | hot total | 560 ms | 238 ms | 2.35x | `rudb-bench/reports/2026-09-26/compiler-baseline-tpch.md` |
| TPC-H SF10 | hot total | 2.76 s | 1.72 s | 1.60x | same |
| TPC-H SF10 | cold total | | | 0.75x | same |
| TPC-H SF1 | instructions retired, one thread | 20.11 G | 37.15 G | 0.54x | `../2140/tenx/` |
| JOB | g2 gate total | 75,649 ms | 7,048 ms | 10.73x | `../2140/bench/job/17-what-the-runs-taught.md` section 17.72 |

The JOB run was on server2 with 6 threads. The instructions-retired row is the one that matters for G9. rudb's TPC-H speed came from more threads used well, not from less work. At SF1 on one thread rudb retired 1.85x more instructions than DuckDB. To reach 10x by that metric, rupg must retire 18.5x fewer instructions than rudb. The JOB result reached 10.73x because exact semi-join reduction over links removed most of the rows before the joins. TPC-H has fewer of the chains of joins where that method pays.

There is no TPC-H SF100 reference on our machines. DuckDB 2.0.0 is planned for 21 October 2026 and becomes the TPC-H baseline at M0. The current release is 1.5.6.

## 3.12 TPC-C

### 3.12.1 Published numbers

HammerDB TPROC-C, from Small Datum on 15 February 2026. The machine is an AMD EPYC 9454P with 48 cores, SMT off, 128 GB of memory and NVMe in RAID 1. fsync on commit is off and each run is 60 minutes. The values are NOPM.

| Warehouses and virtual users | PostgreSQL 18.1 | PostgreSQL 17.7 |
|---|---|---|
| 1,000 and 10 | 341,688 | 356,469 |
| 1,000 and 20 | 422,397 | 423,986 |
| 1,000 and 40 | 431,648 | 444,364 |
| 2,000 and 40 | 303,403 | 323,454 |
| 4,000 and 40 | 236,986 | 247,610 |

The best value, 444,364 NOPM on 48 cores, is about 9,000 NOPM per core. Document 02 section 2.7.2 sets the target at 10x this per core with procedures, about 90,000 NOPM per core. The table also shows that PostgreSQL loses 47 percent of its throughput from 1,000 to 4,000 warehouses, which is when the data stops fitting in memory. Document 20 runs TPC-C at two sizes for this reason.

The Google AlloyDB benchmark guide, updated 5 October 2026, gives 716,138 NOPM at 64 vCPU fully cached and 589,598 NOPM with 30 percent cached. At 16 vCPU it gives 428,316 and 252,970. These are for AlloyDB, not PostgreSQL, and are context only.

### 3.12.2 The commit floor on our machine

On `gpc` (i9-13900K under WSL2), on 23 September 2026, PostgreSQL 18.6 gave 371 durable commits a second at one client, 1,739 at 8 and 5,554 at 32. SQLite in WAL mode with full sync gave about 300 at one client. DuckDB gave about 190 at one client, and it fails concurrent updates of the same rows (`../2140/bench/tpc-c/07-the-baselines.md` section 7.1).

The `fdatasync` p50 on that device is 2,247 µs. A closed loop of 32 terminals that each wait for a sync can finish at most 32 / 2,247 µs transactions a second, and New-Order is 45 percent of them, which gives about 385,000 NOPM as an upper bound for any engine on that device at 32 terminals. The bound is set by the device and the number of terminals, not by the engine. Document 20 prints it next to every durable TPC-C result.

rudb has no TPC-C number.

## 3.13 YCSB and the statement floor

These are the probes of `../2140/bench/ycsb/06-the-baselines.md` on `gpc`.

| System | Operation | Clients | Durability | Latency | Throughput |
|---|---|---|---|---|---|
| PostgreSQL 18.6 | read | 1 | any | 35 µs | 28,207/s |
| PostgreSQL 18.6 | read | 16 | any | 70 µs | 227,753/s |
| PostgreSQL 18.6 | update | 1 | sync on | 2,251 µs | 444/s |
| PostgreSQL 18.6 | update | 16 | sync on | 5,657 µs | 2,828/s |
| PostgreSQL 18.6 | update | 1 | sync off | | 24,428/s |
| PostgreSQL 18.6 | update | 16 | sync off | 135 µs | 118,606/s |
| SQLite 3.53.1 | read | 1 | any | 4.4 µs | |
| DuckDB | read | 1 | any | about 670 µs | |

Under a zipfian key choice, the hottest key in PostgreSQL can be updated only about 480 times a second with sync on, because the row lock is held across the log flush. `SELECT 1` takes about 13 µs over a Unix socket and about 30 µs over TCP. So of the 35 µs of a PostgreSQL point read, about 22 µs is PostgreSQL's own work (document 02 section 2.7.1).

PostgreSQL reads at 16 clients run at about 14,000 per core. That is the number that the CPU targets of document 02 section 2.7.3 are measured against.

### 3.13.1 pgbench references

ClickHouse's PostgresBench runs pgbench with 256 clients at scale 6,849 and 34,247. Self-managed PostgreSQL 18.3 gave 27,012, 28,344 and 28,668 tps, and about 23,000 with two synchronous standbys. PlanetScale gave about 16,000, Aurora about 13,000 to 14,600, Crunchy 8,500 to 14,800, Neon about 8,500 and RDS on gp3 about 7,200 to 8,100. The AlloyDB guide gives 21,750 tps for TPC-B on 16 vCPU fully cached and 17,460 partly cached, and 467,583 tps select-only on 64 vCPU. These are managed services on different hardware. They tell us what a PostgreSQL user pays today. Document 20 runs pgbench as a smoke test, not as a gate.

## 3.14 Versions

| Product | Version for baselines | Date | Note |
|---|---|---|---|
| PostgreSQL | 19.0 | GA planned 29 October 2026 | RC1 planned 15 October 2026. Until GA, baselines use 19beta4 at the pin. |
| PostgreSQL | 18.6 | 13 August 2026 | The version of all `gpc` probes. |
| PostgreSQL | 14 | end of life 12 November 2026 | Lowest version of the shim. |
| DuckDB | 2.0.0 | planned 21 October 2026 | 1.5.6 is current. |
| ClickHouse | 26.9 or newer | | 26.9 made the new analyzer the only analyzer. |
| Umbra | 26.09 | 18 September 2026 | Licensing for our own runs is UNVERIFIED. |
| CedarDB | release of 29 September 2026 | | Licensing for our own runs is UNVERIFIED. |
| SQLite | 3.53.1 | | Library mode comparison. |

## 3.15 The M0 remeasurement list

M0 turns the references of this document into baselines. Each run below uses the harness and the measurement rules of document 20. Each run records the exact version, the configuration file, the machine and the date in the result.

1. ClickBench on `c6a.4xlarge` and `c6a.metal`, hot, cold and load, with memory, CPU and disk as document 20 section 20.4 defines them, for:
   - PostgreSQL 19 with the default configuration of the ClickBench scripts.
   - PostgreSQL 19 tuned, with `shared_buffers`, `work_mem`, `max_parallel_workers_per_gather` and `effective_io_concurrency` set for the machine, and `io_method = io_uring`. This row is labeled tuned.
   - The newest ClickHouse release.
   - DuckDB 2.0.0, or the newest release if 2.0.0 is late.
   - Umbra or CedarDB, where the license permits a published comparison. If it does not, the published numbers of section 3.2 stay as references and no gate uses them.
2. TPC-H at SF1, SF10 and SF100 for DuckDB 2.0.0 and PostgreSQL 19, hot and cold.
3. TPC-C with HammerDB for PostgreSQL 19 at two warehouse counts on the gate machine of document 20, with procedures and with plain statements, at the same durability as rupg.
4. YCSB A, B, C and F for PostgreSQL 19, with and without pipelining.
5. The commit curve and the `fdatasync` latency of the gate machine.
6. The idle connection memory of PostgreSQL 19 at 1,000 connections with the same `shared_buffers` as rupg.
7. The version shim suites of document 05 section 5.6 for PostgreSQL 14 to 18, so that each shim has a reference output.

The ClickBench runs for PostgreSQL, ClickHouse and DuckDB use the scripts of the ClickBench repository at the pin without change, so that our numbers can be compared with the published ones. When our number differs from the published one by more than 10 percent, the difference is recorded in the M0 report with its cause.

## 3.16 What the baselines decide

1. The ClickBench target is 1.753 s and the best published time for each query sums to 4.539 s. So rupg must beat the best system on most queries, not only on average.
2. At the best published times, the classes over budget exceed it by 3.15 s in total, and C3 is 2.29 s of that, or 73 percent. The design work for group-by on large keys (H2, H3) comes first after M4.
3. C1 and C2 have a problem that H1 does not solve: exact distinct counts on a non-dictionary column. It is open in document 24.
4. The disk gate G3 is 0.2 GB below rudb's measured encoding. The URL columns decide it.
5. There is no memory or CPU reference for G8. M0 must produce one before M4 starts, so that every M4 run can be compared with it.
6. The TPC-C target per core is inside what LeanStore has shown, and on our test machine the commit bound is about 385,000 NOPM at 32 terminals. Document 20 must run TPC-C on a machine whose sync latency does not set the result.
