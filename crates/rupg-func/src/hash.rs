//! The hash support functions of the types, from `hashfunc.c` and the files of the types, with `hash_bytes` and `hash_bytes_uint32` of `hashfn.c`. Each hash is the hash of PostgreSQL on a little-endian machine. A function with the suffix `extended` takes a seed and gives a hash of 64 bits. With the seed 0, the low 32 bits of the extended hash are the plain hash.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    Array, NetFamily, Numeric, NumericSign, RANGE_EMPTY, RANGE_LB_INF, RANGE_UB_INF, Range,
    TypeInfo, Value, format_type, oid,
};

use crate::{Kernel, bad_value, not_yet};

/// The hash functions of the types with one fixed type of argument: the `prosrc` of the plain function, the `prosrc` of the extended function and the type.
macro_rules! fixed {
    ($src:expr; $($plain:literal, $seeded:literal => $ty:expr;)*) => {
        match $src {
            $(
                $plain => |_, args| plain($ty, args),
                $seeded => |_, args| seeded($ty, args),
            )*
            _ => return None,
        }
    };
}

/// The kernel of a hash function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    // The type of the argument of these functions comes from the call.
    match src {
        "hash_array" | "hash_range" | "hash_multirange" | "hash_record" => {
            return Some(|call, args| plain(call.args.first().copied().unwrap_or(0), args));
        }
        "hash_array_extended"
        | "hash_range_extended"
        | "hash_multirange_extended"
        | "hash_record_extended" => {
            return Some(|call, args| seeded(call.args.first().copied().unwrap_or(0), args));
        }
        _ => {}
    }
    Some(fixed! { src;
        "hashbool", "hashboolextended" => oid::BOOL;
        "hashchar", "hashcharextended" => oid::CHAR;
        "hashint2", "hashint2extended" => oid::INT2;
        "hashint4", "hashint4extended" => oid::INT4;
        "hashint8", "hashint8extended" => oid::INT8;
        "hashoid", "hashoidextended" => oid::OID;
        // `hashenum` hashes the OID of the label, as `hashoid` does.
        "hashenum", "hashenumextended" => oid::OID;
        "hashoid8", "hashoid8extended" => oid::OID8;
        "hashxid", "hashxidextended" => oid::XID;
        "hashxid8", "hashxid8extended" => oid::XID8;
        "hashcid", "hashcidextended" => oid::CID;
        "hashtid", "hashtidextended" => oid::TID;
        "hashfloat4", "hashfloat4extended" => oid::FLOAT4;
        "hashfloat8", "hashfloat8extended" => oid::FLOAT8;
        "hash_numeric", "hash_numeric_extended" => oid::NUMERIC;
        "hashoidvector", "hashoidvectorextended" => oid::OIDVECTOR;
        "hashname", "hashnameextended" => oid::NAME;
        "hashtext", "hashtextextended" => oid::TEXT;
        "hashbpchar", "hashbpcharextended" => oid::BPCHAR;
        "hashbytea", "hashbyteaextended" => oid::BYTEA;
        "hashdate", "hashdateextended" => oid::DATE;
        "time_hash", "time_hash_extended" => oid::TIME;
        "timetz_hash", "timetz_hash_extended" => oid::TIMETZ;
        "timestamp_hash", "timestamp_hash_extended" => oid::TIMESTAMP;
        "timestamptz_hash", "timestamptz_hash_extended" => oid::TIMESTAMPTZ;
        "interval_hash", "interval_hash_extended" => oid::INTERVAL;
        "uuid_hash", "uuid_hash_extended" => oid::UUID;
        "hashmacaddr", "hashmacaddrextended" => oid::MACADDR;
        "hashmacaddr8", "hashmacaddr8extended" => oid::MACADDR8;
        "hashinet", "hashinetextended" => oid::INET;
        "pg_lsn_hash", "pg_lsn_hash_extended" => oid::PG_LSN;
    })
}

/// A plain hash function: the hash of 32 bits as `integer`.
fn plain(ty: u32, args: &[Value]) -> Result<Value> {
    let value = args.first().ok_or_else(bad_value)?;
    let hash = hash(ty, value, Seed::Plain)?;
    #[allow(clippy::cast_possible_truncation)]
    Ok(Value::Int4((hash as u32).cast_signed()))
}

/// An extended hash function: the hash of 64 bits with the seed of the second argument as `bigint`.
fn seeded(ty: u32, args: &[Value]) -> Result<Value> {
    let (Some(value), Some(Value::Int8(seed))) = (args.first(), args.get(1)) else {
        return Err(bad_value());
    };
    Ok(Value::Int8(hash(ty, value, Seed::Extended(seed.cast_unsigned()))?.cast_signed()))
}

/// The kind of a hash: the plain hash of 32 bits, or the extended hash of 64 bits with a seed.
#[derive(Clone, Copy)]
enum Seed {
    Plain,
    Extended(u64),
}

impl Seed {
    /// `hash_any` or `hash_any_extended` of some bytes.
    fn bytes(self, data: &[u8]) -> u64 {
        match self {
            Seed::Plain => u64::from(hash_bytes(data, 0).1),
            Seed::Extended(seed) => {
                let (b, c) = hash_bytes(data, seed);
                u64::from(b) << 32 | u64::from(c)
            }
        }
    }

    /// `hash_uint32` or `hash_uint32_extended` of a word.
    fn word(self, word: u32) -> u64 {
        match self {
            Seed::Plain => u64::from(hash_word(word, 0).1),
            Seed::Extended(seed) => {
                let (b, c) = hash_word(word, seed);
                u64::from(b) << 32 | u64::from(c)
            }
        }
    }

    /// `hashint8` or `hashint8extended`: the hash of the low half with the high half, or its complement for a negative value. So an equal `integer` has the same hash.
    fn int8(self, value: i64) -> u64 {
        #[allow(clippy::cast_possible_truncation)]
        let (low, high) = (value as u32, (value >> 32) as u32);
        self.word(low ^ if value >= 0 { high } else { !high })
    }

    /// The hash of a value that has no hash of its own, such as a zero or a `NaN`: 0 for a plain hash and the seed for an extended hash.
    fn zero(self) -> u64 {
        match self {
            Seed::Plain => 0,
            Seed::Extended(seed) => seed,
        }
    }

    /// The step of `hash_array`, `hash_multirange` and `hash_record` that adds the hash of one more element.
    fn combine(self, result: u64, element: u64) -> u64 {
        let next = (result << 5).wrapping_sub(result).wrapping_add(element);
        match self {
            Seed::Plain => next & u64::from(u32::MAX),
            Seed::Extended(_) => next,
        }
    }
}

/// `compute_partition_hash_value`: the hash of the key of a row of a table that `PARTITION BY HASH` partitions. Each value that is not null gets the extended hash of its type with the seed `HASH_PARTITION_SEED`, and `hash_combine64` adds it to the hash. A row goes in the partition whose remainder is this hash modulo its modulus.
///
/// # Errors
///
/// `42883` for a type with no hash function.
pub fn partition_hash(values: &[(u32, Value)]) -> Result<u64> {
    const HASH_PARTITION_SEED: u64 = 0x7A5B_2236_7996_DCFD;
    let mut row = 0u64;
    for (ty, value) in values {
        if value.is_null() {
            continue;
        }
        let one = hash(crate::io::base_type(*ty), value, Seed::Extended(HASH_PARTITION_SEED))?;
        row ^=
            one.wrapping_add(0x49a0_f4dd_15e5_a8e3).wrapping_add(row << 54).wrapping_add(row >> 7);
    }
    Ok(row)
}

/// The hash of a value of the type `ty`. A plain hash has only the low 32 bits.
fn hash(ty: u32, value: &Value, seed: Seed) -> Result<u64> {
    Ok(match (ty, value) {
        (oid::BOOL, Value::Bool(v)) => seed.word(u32::from(*v)),
        // `char` is signed on the machines of the hashes.
        (oid::CHAR, Value::Char(v)) => seed.word(i32::from(v.cast_signed()).cast_unsigned()),
        (oid::INT2, Value::Int2(v)) => seed.word(i32::from(*v).cast_unsigned()),
        (oid::INT4 | oid::DATE, Value::Int4(v) | Value::Date(v)) => seed.word(v.cast_unsigned()),
        (oid::INT8 | oid::OID8 | oid::XID8 | oid::PG_LSN, Value::Int8(v))
        | (oid::TIME, Value::Time(v))
        | (oid::TIMESTAMP, Value::Timestamp(v))
        | (oid::TIMESTAMPTZ, Value::TimestampTz(v)) => seed.int8(*v),
        (_, Value::Oid(v)) if is_oid(ty) => seed.word(*v),
        (oid::TID, Value::Int8(v)) => {
            let (block, offset) = crate::io::tid_parts(*v)?;
            let mut data = [0; 6];
            #[allow(clippy::cast_possible_truncation)]
            {
                data[..2].copy_from_slice(&((block >> 16) as u16).to_le_bytes());
                data[2..4].copy_from_slice(&(block as u16).to_le_bytes());
            }
            data[4..].copy_from_slice(&offset.to_le_bytes());
            seed.bytes(&data)
        }
        (oid::FLOAT4, Value::Float4(v)) => float(f64::from(*v), seed),
        (oid::FLOAT8, Value::Float8(v)) => float(*v, seed),
        (oid::NUMERIC, Value::Numeric(v)) => numeric(v, seed),
        (oid::OIDVECTOR, Value::Array(array)) => {
            let mut data = Vec::with_capacity(array.values.len() * 4);
            for value in &array.values {
                let Some(Value::Oid(v)) = value else { return Err(bad_value()) };
                data.extend_from_slice(&v.to_le_bytes());
            }
            seed.bytes(&data)
        }
        (oid::TEXT | oid::VARCHAR | oid::NAME, Value::Text(v)) => seed.bytes(v.as_bytes()),
        (oid::BPCHAR, Value::Text(v)) => seed.bytes(v.trim_end_matches(' ').as_bytes()),
        (oid::BYTEA, Value::Bytea(v)) => seed.bytes(v),
        (oid::TIMETZ, Value::TimeTz(time, zone)) => {
            seed.int8(*time) ^ seed.word(zone.cast_unsigned())
        }
        (oid::INTERVAL, Value::Interval(v)) => {
            // The low 64 bits of the length, as `interval_hash` keeps them.
            #[allow(clippy::cast_possible_truncation)]
            seed.int8(crate::compare::span(v) as i64)
        }
        (oid::UUID, Value::Uuid(v)) => seed.bytes(v),
        (oid::MACADDR, Value::Int8(v)) => seed.bytes(&v.to_be_bytes()[2..]),
        (oid::MACADDR8, Value::Int8(v)) => seed.bytes(&v.to_be_bytes()),
        (oid::INET | oid::CIDR, Value::Inet(v)) => {
            // The family is `PGSQL_AF_INET` or `PGSQL_AF_INET6` of Linux.
            let (family, size) = match v.family {
                NetFamily::V4 => (2, 4),
                NetFamily::V6 => (3, 16),
            };
            let mut data = vec![family, v.bits];
            data.extend_from_slice(&v.addr[..size]);
            seed.bytes(&data)
        }
        (_, Value::Array(array)) => self::array(ty, array, seed)?,
        (_, Value::Range(range)) => {
            let subtype = builtin::range_subtype(ty).ok_or_else(|| no_hash(ty))?;
            self::range(subtype, range, seed)?
        }
        (_, Value::Multirange(ranges)) => {
            let subtype = builtin::multirange_range(ty)
                .and_then(builtin::range_subtype)
                .ok_or_else(|| no_hash(ty))?;
            let mut result = 1;
            for range in ranges {
                result = seed.combine(result, self::range(subtype, range, seed)?);
            }
            result
        }
        (_, Value::Record(record)) => {
            let mut result = 0;
            for (&ty, value) in record.types.iter().zip(&record.values) {
                let element = if value.is_null() { 0 } else { hash(ty, value, seed)? };
                result = seed.combine(result, element);
            }
            result
        }
        (oid::JSONB, _) => return Err(not_yet("the hash of jsonb")),
        (oid::ACLITEM, _) => return Err(not_yet("the hash of aclitem")),
        _ if has_hash(ty) => return Err(bad_value()),
        _ => return Err(no_hash(ty)),
    })
}

/// True for `oid`, the types of `xid` and `cid`, and the types of the OIDs of objects, which `hashoid` hashes as the OID.
fn is_oid(ty: u32) -> bool {
    matches!(
        ty,
        oid::OID
            | oid::XID
            | oid::CID
            | oid::REGPROC
            | oid::REGPROCEDURE
            | oid::REGOPER
            | oid::REGOPERATOR
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::REGCONFIG
            | oid::REGDICTIONARY
            | oid::REGNAMESPACE
            | oid::REGROLE
            | oid::REGCOLLATION
            | oid::REGDATABASE
    )
}

/// True for a type that has a hash function in the catalog.
fn has_hash(ty: u32) -> bool {
    matches!(
        ty,
        oid::JSONB
            | oid::ACLITEM
            | oid::BOOL
            | oid::CHAR
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::OID8
            | oid::XID8
            | oid::PG_LSN
            | oid::DATE
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::TID
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::NUMERIC
            | oid::OIDVECTOR
            | oid::TEXT
            | oid::VARCHAR
            | oid::NAME
            | oid::BPCHAR
            | oid::BYTEA
            | oid::TIMETZ
            | oid::INTERVAL
            | oid::UUID
            | oid::MACADDR
            | oid::MACADDR8
            | oid::INET
            | oid::CIDR
    ) || is_oid(ty)
}

/// The error of a type with no hash function.
fn no_hash(ty: u32) -> Error {
    Error::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("could not identify a hash function for type {}", format_type(ty)),
    )
}

/// `hashfloat8` and `hashfloat4`, which widens the value to `double precision` first: a zero of each sign has the hash of [`Seed::zero`], and each `NaN` has the hash of the standard `NaN`.
fn float(value: f64, seed: Seed) -> u64 {
    if value == 0.0 {
        return seed.zero();
    }
    let value = if value.is_nan() { f64::NAN } else { value };
    seed.bytes(&value.to_le_bytes())
}

/// `hashfloat8` of the seed of `TABLESAMPLE ... REPEATABLE`.
pub(crate) fn float8_hash(value: f64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let hash = float(value, Seed::Plain) as u32;
    hash
}

/// The hash of the number of a row and the seed of a sample, which `TABLESAMPLE` compares with its cutoff. `tsm_bernoulli.c` hashes the block, the offset and the seed of a row. A row of rupg has no block, so its number takes the place of the block and the offset.
pub(crate) fn sample_hash(row: u64, seed: u32) -> u32 {
    let mut data = [0; 12];
    data[..8].copy_from_slice(&row.to_le_bytes());
    data[8..].copy_from_slice(&seed.to_le_bytes());
    hash_bytes(&data, 0).1
}

/// `hash_numeric`: the hash of the digits of base 10000 with no zeros at the two ends, then the weight of the first digit that is not zero. A special value has the hash of [`Seed::zero`], and zero has the hash of [`Seed::zero`] less 1.
fn numeric(value: &Numeric, seed: Seed) -> u64 {
    if matches!(
        value.sign(),
        NumericSign::NaN | NumericSign::Infinity | NumericSign::NegativeInfinity
    ) {
        return seed.zero();
    }
    let digits = value.digits();
    let Some(first) = digits.iter().position(|&d| d != 0) else {
        return match seed {
            Seed::Plain => u64::from(u32::MAX),
            Seed::Extended(seed) => seed.wrapping_sub(1),
        };
    };
    let last = digits.iter().rposition(|&d| d != 0).unwrap_or(first);
    let data: Vec<u8> = digits[first..=last].iter().flat_map(|d| d.to_le_bytes()).collect();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let weight = i64::from(value.weight()) - first as i64;
    let weight = weight.cast_unsigned();
    match seed {
        Seed::Plain => (seed.bytes(&data) ^ weight) & u64::from(u32::MAX),
        Seed::Extended(_) => seed.bytes(&data) ^ weight,
    }
}

/// `hash_array`: the hashes of the elements in order, with 0 for a null element.
fn array(ty: u32, array: &Array<Value>, seed: Seed) -> Result<u64> {
    let element = TypeInfo::get(ty).filter(|info| info.is_array()).ok_or_else(|| no_hash(ty))?.elem;
    if element != oid::RECORD && !has_hash(element) && !is_container(element) {
        return Err(no_hash(element));
    }
    let mut result = 1;
    for value in &array.values {
        let hash = match value {
            None => 0,
            Some(value) => hash(element, value, seed)?,
        };
        result = seed.combine(result, hash);
    }
    Ok(result)
}

/// True for an array, a range or a multirange type, whose hash is the hash of its parts.
fn is_container(ty: u32) -> bool {
    builtin::range_subtype(ty).is_some()
        || builtin::multirange_range(ty).is_some()
        || TypeInfo::get(ty).is_some_and(TypeInfo::is_array)
}

/// `hash_range`: the hash of the flags, then the hash of the lower bound, a rotation of one bit, and the hash of the upper bound. A missing bound has the hash 0. For an extended hash, the rotation is of each half of 32 bits.
fn range(subtype: u32, range: &Range, seed: Seed) -> Result<u64> {
    let bound = |value: &Value, missing: u8| -> Result<u64> {
        if range.flags & (RANGE_EMPTY | missing) != 0 { Ok(0) } else { hash(subtype, value, seed) }
    };
    let lower = bound(&range.lower, RANGE_LB_INF)?;
    let upper = bound(&range.upper, RANGE_UB_INF)?;
    let result = seed.word(u32::from(range.flags)) ^ lower;
    let result = match seed {
        Seed::Plain =>
        {
            #[allow(clippy::cast_possible_truncation)]
            u64::from((result as u32).rotate_left(1))
        }
        Seed::Extended(_) => {
            (result << 1) & 0xffff_fffe_ffff_fffe | (result >> 31) & 0x0000_0001_0000_0001
        }
    };
    Ok(result ^ upper)
}

/// The `mix` step of `hashfn.c`.
fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c) ^ c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a) ^ a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b) ^ b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c) ^ c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a) ^ a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b) ^ b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

/// The `final` step of `hashfn.c`.
fn finish(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c = (*c ^ *b).wrapping_sub(b.rotate_left(14));
    *a = (*a ^ *c).wrapping_sub(c.rotate_left(11));
    *b = (*b ^ *a).wrapping_sub(a.rotate_left(25));
    *c = (*c ^ *b).wrapping_sub(b.rotate_left(16));
    *a = (*a ^ *c).wrapping_sub(c.rotate_left(4));
    *b = (*b ^ *a).wrapping_sub(a.rotate_left(14));
    *c = (*c ^ *b).wrapping_sub(b.rotate_left(24));
}

/// The start of the state for some bytes of the length `len`, with a seed that is not zero mixed in.
fn start(len: u32, seed: u64) -> (u32, u32, u32) {
    let init = 0x9e37_79b9_u32.wrapping_add(len).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        #[allow(clippy::cast_possible_truncation)]
        {
            a = a.wrapping_add((seed >> 32) as u32);
            b = b.wrapping_add(seed as u32);
        }
        mix(&mut a, &mut b, &mut c);
    }
    (a, b, c)
}

/// `hash_bytes_extended` of little-endian words: the words `b` and `c` of the state at the end. `hash_bytes` is `c`, with the seed 0.
fn hash_bytes(data: &[u8], seed: u64) -> (u32, u32) {
    #[allow(clippy::cast_possible_truncation)]
    let (mut a, mut b, mut c) = start(data.len() as u32, seed);
    let word = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let (chunks, tail) = data.as_chunks::<12>();
    for chunk in chunks {
        a = a.wrapping_add(word(&chunk[..4]));
        b = b.wrapping_add(word(&chunk[4..8]));
        c = c.wrapping_add(word(&chunk[8..]));
        mix(&mut a, &mut b, &mut c);
    }
    // The last bytes. The lowest byte of `c` is for the length.
    let mut rest = [0; 12];
    rest[..tail.len()].copy_from_slice(tail);
    a = a.wrapping_add(word(&rest[..4]));
    b = b.wrapping_add(word(&rest[4..8]));
    c = c.wrapping_add(word(&rest[8..]) << 8);
    finish(&mut a, &mut b, &mut c);
    (b, c)
}

/// `hash_bytes_uint32_extended`: the words `b` and `c` of the state at the end. `hash_bytes_uint32` is `c`, with the seed 0.
fn hash_word(word: u32, seed: u64) -> (u32, u32) {
    let (mut a, mut b, mut c) = start(4, seed);
    a = a.wrapping_add(word);
    finish(&mut a, &mut b, &mut c);
    (b, c)
}
