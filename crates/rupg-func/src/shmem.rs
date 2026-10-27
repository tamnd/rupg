//! `pg_get_shmem_allocations`, which the view `pg_shmem_allocations` reads: the named allocations of the shared memory of the server, the anonymous allocations and the free space.
//!
//! rupg runs in one process and has no shared memory of PostgreSQL. The function gives the rows of PostgreSQL 19 with the default configuration, in the order of the hash table `ShmemIndex`, with their offsets and sizes. The names tell which parts of the server a client can expect, and the offsets and sizes are the values of a server with `shared_buffers` of 128 MB.

use rupg_common::Result;
use rupg_types::Value;

use crate::Call;

/// The named allocations of `ShmemIndex`: the name, the offset, the size that the caller asked for and the size with the alignment.
const ALLOCATIONS: [(&str, i64, i64, i64); 77] = [
    ("subtransaction", 5077248, 267424, 267520),
    ("serializable", 152322944, 267424, 267536),
    ("PROCLOCK hash", 145943168, 3517824, 3517872),
    ("Backend Client Host Name Buffer", 153021312, 11136, 11136),
    ("Prepared Transaction Table", 153267072, 2736, 2808),
    ("KnownAssignedXidsValid", 152923264, 9490, 9546),
    ("Slot Sync Data", 153911936, 24, 72),
    ("AioWorkerSubmissionQueue", 160350208, 268, 268),
    ("WaitEventCustom hash by wait event information", 154370176, 17088, 17208),
    ("CommitTs shared", 5077120, 32, 128),
    ("AioHandleData", 158924800, 1425408, 1425408),
    ("Buffer Blocks", 6799360, 134217728, 134221184),
    ("XLogPrefetchStats", 4279680, 72, 144),
    ("AioBackend", 154413312, 57072, 57144),
    ("Async Queue Control", 153914496, 5512, 5624),
    ("commit_timestamp", 4809600, 267424, 267520),
    ("multixact_member", 5479808, 267424, 267424),
    ("multixact_offset", 5346048, 133760, 133856),
    ("Main LWLock array", 42880, 27904, 28024),
    ("Shared MultiXact State", 5344768, 1184, 1280),
    ("Proc Header", 152862848, 128, 128),
    ("LWLock tranches", 25344, 17416, 17528),
    ("ReplicationSlot Ctl", 153902336, 4440, 4496),
    ("BTree Vacuum State", 153912064, 1644, 1748),
    ("Semaphores", 152862976, 22272, 22272),
    ("PREDICATELOCKTARGET hash", 149465216, 583536, 583660),
    ("Backend Status Array", 152933504, 76560, 76580),
    ("Buffer IO Condition Variables", 141017088, 262144, 262144),
    ("Proc Array", 152932864, 620, 730),
    ("AioWorkerControl", 160350592, 156, 272),
    ("PredXactList", 151660544, 245368, 245416),
    ("Fast Path Strong Relation Lock Data", 149460992, 4100, 4100),
    ("TransamVariables", 70912, 72, 184),
    ("Sync Scan Locations List", 153913728, 656, 676),
    ("notify", 153920128, 133760, 133880),
    ("Shared Memory Stats", 154053888, 316128, 316128),
    ("WaitLSNState", 154404480, 8480, 8544),
    ("FinishedSerializableTransactions", 152322816, 16, 56),
    ("KnownAssignedXids", 152885248, 37960, 37960),
    ("Backend Application Name Buffer", 153010176, 11136, 11248),
    ("ProcSignal", 153352320, 19496, 19580),
    ("Logical Replication Launcher Data", 153911296, 592, 712),
    ("PREDICATELOCK hash", 150048768, 1611728, 1611744),
    ("AioHandleIOV", 156073984, 2850816, 2850816),
    ("Wal Summarizer Ctl", 153911040, 48, 104),
    ("WaitEventCustomCounterData", 154370048, 8, 40),
    ("transaction", 4279936, 529568, 529592),
    ("Buffer Strategy Status", 141606912, 20, 20),
    ("SerialControlData", 152590464, 16, 112),
    ("shmInvalBuffer", 153281920, 69048, 69160),
    ("Backend Activity Buffer", 153032448, 178176, 178176),
    ("Background Worker Data", 153269888, 11920, 12000),
    ("LOCK hash", 142533120, 3410000, 3410080),
    ("WaitEventCustom hash by name", 154387328, 17088, 17152),
    ("AioCtl", 154413184, 56, 181),
    ("AioHandle", 154470400, 1603584, 1603600),
    ("Buffer Descriptors", 5747328, 1048576, 1048672),
    ("Backend SSL Status Buffer", 153210624, 56376, 56376),
    ("Wal Sender Ctl", 153907584, 1072, 1192),
    ("AutoVacuum Data", 153896320, 5960, 6024),
    ("XLOG Ctl", 71040, 4208200, 4208256),
    ("Logical decoding control", 154413056, 3, 99),
    ("Archiver Data", 153911168, 8, 88),
    ("XLOG Recovery Ctl", 4279808, 104, 160),
    ("Checkpoint BufferIds", 141279232, 327680, 327680),
    ("Wal Receiver Ctl", 153908736, 2248, 2328),
    ("ShmemIndex", 128, 25104, 25104),
    ("DSM Registry Data", 70784, 16, 16),
    ("Shared Buffer Lookup Table", 141607040, 926000, 926108),
    ("PMSignalState", 153351040, 1196, 1268),
    ("Fast-Path Lock Array", 152756864, 105984, 106104),
    ("RWConflictPool", 151972352, 350424, 350472),
    ("PGPROC structures", 152590592, 166152, 166264),
    ("Control File", 4279296, 312, 368),
    ("ReplicationOriginState", 153906816, 648, 688),
    ("SERIALIZABLEXID hash", 151905920, 66384, 66392),
    ("Checkpointer Data", 153371904, 524352, 524440),
];

/// The anonymous allocations, which have no offset in `ShmemIndex`: the size and the size with the alignment.
const ANONYMOUS: (i64, i64) = (128, 128);

/// The free space at the end of the segment: the offset and the size.
const FREE: (i64, i64) = (160350748, 105956);

/// `pg_get_shmem_allocations`: a row for each named allocation, then the row `<anonymous>` and the row of the free space, whose name is null, as `pg_get_shmem_allocations` gives them.
pub(crate) fn allocations(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let mut rows: Vec<Vec<Value>> = ALLOCATIONS
        .iter()
        .map(|&(name, offset, size, allocated)| {
            vec![Value::text(name), Value::Int8(offset), Value::Int8(size), Value::Int8(allocated)]
        })
        .collect();
    let (size, allocated) = ANONYMOUS;
    rows.push(vec![
        Value::text("<anonymous>"),
        Value::Null,
        Value::Int8(size),
        Value::Int8(allocated),
    ]);
    let (offset, size) = FREE;
    rows.push(vec![Value::Null, Value::Int8(offset), Value::Int8(size), Value::Int8(size)]);
    Ok(rows)
}
