//! `cargo xtask contrib` makes this file from `vendor/postgres-19/contrib`. Do not edit it.

/// The files of `contrib/btree_gin`.
pub(crate) const BTREE_GIN: &[(&str, &str)] = &[
    (
        "btree_gin--1.0--1.1.sql",
        r#"/* contrib/btree_gin/btree_gin--1.0--1.1.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gin UPDATE TO '1.1'" to load this file. \quit

-- macaddr8 datatype support new in 10.0.
CREATE FUNCTION gin_extract_value_macaddr8(macaddr8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_macaddr8(macaddr8, macaddr8, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_macaddr8(macaddr8, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS macaddr8_ops
DEFAULT FOR TYPE macaddr8 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       macaddr8_cmp(macaddr8, macaddr8),
    FUNCTION        2       gin_extract_value_macaddr8(macaddr8, internal),
    FUNCTION        3       gin_extract_query_macaddr8(macaddr8, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_macaddr8(macaddr8, macaddr8, int2, internal),
STORAGE         macaddr8;
"#,
    ),
    (
        "btree_gin--1.0.sql",
        r#"/* contrib/btree_gin/btree_gin--1.0.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "CREATE EXTENSION btree_gin" to load this file. \quit

CREATE FUNCTION gin_btree_consistent(internal, int2, anyelement, int4, internal, internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_value_int2(int2, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_int2(int2, int2, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_int2(int2, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS int2_ops
DEFAULT FOR TYPE int2 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btint2cmp(int2,int2),
    FUNCTION        2       gin_extract_value_int2(int2, internal),
    FUNCTION        3       gin_extract_query_int2(int2, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_int2(int2,int2,int2, internal),
STORAGE         int2;

CREATE FUNCTION gin_extract_value_int4(int4, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_int4(int4, int4, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_int4(int4, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS int4_ops
DEFAULT FOR TYPE int4 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btint4cmp(int4,int4),
    FUNCTION        2       gin_extract_value_int4(int4, internal),
    FUNCTION        3       gin_extract_query_int4(int4, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_int4(int4,int4,int2, internal),
STORAGE         int4;

CREATE FUNCTION gin_extract_value_int8(int8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_int8(int8, int8, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_int8(int8, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS int8_ops
DEFAULT FOR TYPE int8 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btint8cmp(int8,int8),
    FUNCTION        2       gin_extract_value_int8(int8, internal),
    FUNCTION        3       gin_extract_query_int8(int8, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_int8(int8,int8,int2, internal),
STORAGE         int8;

CREATE FUNCTION gin_extract_value_float4(float4, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_float4(float4, float4, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_float4(float4, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS float4_ops
DEFAULT FOR TYPE float4 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btfloat4cmp(float4,float4),
    FUNCTION        2       gin_extract_value_float4(float4, internal),
    FUNCTION        3       gin_extract_query_float4(float4, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_float4(float4,float4,int2, internal),
STORAGE         float4;

CREATE FUNCTION gin_extract_value_float8(float8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_float8(float8, float8, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_float8(float8, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS float8_ops
DEFAULT FOR TYPE float8 USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btfloat8cmp(float8,float8),
    FUNCTION        2       gin_extract_value_float8(float8, internal),
    FUNCTION        3       gin_extract_query_float8(float8, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_float8(float8,float8,int2, internal),
STORAGE         float8;

CREATE FUNCTION gin_extract_value_money(money, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_money(money, money, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_money(money, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS money_ops
DEFAULT FOR TYPE money USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       cash_cmp(money,money),
    FUNCTION        2       gin_extract_value_money(money, internal),
    FUNCTION        3       gin_extract_query_money(money, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_money(money,money,int2, internal),
STORAGE         money;

CREATE FUNCTION gin_extract_value_oid(oid, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_oid(oid, oid, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_oid(oid, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS oid_ops
DEFAULT FOR TYPE oid USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btoidcmp(oid,oid),
    FUNCTION        2       gin_extract_value_oid(oid, internal),
    FUNCTION        3       gin_extract_query_oid(oid, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_oid(oid,oid,int2, internal),
STORAGE         oid;

CREATE FUNCTION gin_extract_value_timestamp(timestamp, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_timestamp(timestamp, timestamp, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_timestamp(timestamp, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS timestamp_ops
DEFAULT FOR TYPE timestamp USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       timestamp_cmp(timestamp,timestamp),
    FUNCTION        2       gin_extract_value_timestamp(timestamp, internal),
    FUNCTION        3       gin_extract_query_timestamp(timestamp, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_timestamp(timestamp,timestamp,int2, internal),
STORAGE         timestamp;

CREATE FUNCTION gin_extract_value_timestamptz(timestamptz, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_timestamptz(timestamptz, timestamptz, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_timestamptz(timestamptz, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS timestamptz_ops
DEFAULT FOR TYPE timestamptz USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       timestamptz_cmp(timestamptz,timestamptz),
    FUNCTION        2       gin_extract_value_timestamptz(timestamptz, internal),
    FUNCTION        3       gin_extract_query_timestamptz(timestamptz, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_timestamptz(timestamptz,timestamptz,int2, internal),
STORAGE         timestamptz;

CREATE FUNCTION gin_extract_value_time(time, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_time(time, time, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_time(time, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS time_ops
DEFAULT FOR TYPE time USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       time_cmp(time,time),
    FUNCTION        2       gin_extract_value_time(time, internal),
    FUNCTION        3       gin_extract_query_time(time, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_time(time,time,int2, internal),
STORAGE         time;

CREATE FUNCTION gin_extract_value_timetz(timetz, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_timetz(timetz, timetz, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_timetz(timetz, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS timetz_ops
DEFAULT FOR TYPE timetz USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       timetz_cmp(timetz,timetz),
    FUNCTION        2       gin_extract_value_timetz(timetz, internal),
    FUNCTION        3       gin_extract_query_timetz(timetz, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_timetz(timetz,timetz,int2, internal),
STORAGE         timetz;

CREATE FUNCTION gin_extract_value_date(date, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_date(date, date, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_date(date, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS date_ops
DEFAULT FOR TYPE date USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       date_cmp(date,date),
    FUNCTION        2       gin_extract_value_date(date, internal),
    FUNCTION        3       gin_extract_query_date(date, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_date(date,date,int2, internal),
STORAGE         date;

CREATE FUNCTION gin_extract_value_interval(interval, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_interval(interval, interval, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_interval(interval, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS interval_ops
DEFAULT FOR TYPE interval USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       interval_cmp(interval,interval),
    FUNCTION        2       gin_extract_value_interval(interval, internal),
    FUNCTION        3       gin_extract_query_interval(interval, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_interval(interval,interval,int2, internal),
STORAGE         interval;

CREATE FUNCTION gin_extract_value_macaddr(macaddr, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_macaddr(macaddr, macaddr, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_macaddr(macaddr, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS macaddr_ops
DEFAULT FOR TYPE macaddr USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       macaddr_cmp(macaddr,macaddr),
    FUNCTION        2       gin_extract_value_macaddr(macaddr, internal),
    FUNCTION        3       gin_extract_query_macaddr(macaddr, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_macaddr(macaddr,macaddr,int2, internal),
STORAGE         macaddr;

CREATE FUNCTION gin_extract_value_inet(inet, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_inet(inet, inet, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_inet(inet, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS inet_ops
DEFAULT FOR TYPE inet USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       network_cmp(inet,inet),
    FUNCTION        2       gin_extract_value_inet(inet, internal),
    FUNCTION        3       gin_extract_query_inet(inet, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_inet(inet,inet,int2, internal),
STORAGE         inet;

CREATE FUNCTION gin_extract_value_cidr(cidr, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_cidr(cidr, cidr, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_cidr(cidr, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS cidr_ops
DEFAULT FOR TYPE cidr USING gin
AS
    OPERATOR        1       <(inet,inet),
    OPERATOR        2       <=(inet,inet),
    OPERATOR        3       =(inet,inet),
    OPERATOR        4       >=(inet,inet),
    OPERATOR        5       >(inet,inet),
    FUNCTION        1       network_cmp(inet,inet),
    FUNCTION        2       gin_extract_value_cidr(cidr, internal),
    FUNCTION        3       gin_extract_query_cidr(cidr, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_cidr(cidr,cidr,int2, internal),
STORAGE         cidr;

CREATE FUNCTION gin_extract_value_text(text, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_text(text, text, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_text(text, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS text_ops
DEFAULT FOR TYPE text USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       bttextcmp(text,text),
    FUNCTION        2       gin_extract_value_text(text, internal),
    FUNCTION        3       gin_extract_query_text(text, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_text(text,text,int2, internal),
STORAGE         text;

CREATE OPERATOR CLASS varchar_ops
DEFAULT FOR TYPE varchar USING gin
AS
    OPERATOR        1       <(text,text),
    OPERATOR        2       <=(text,text),
    OPERATOR        3       =(text,text),
    OPERATOR        4       >=(text,text),
    OPERATOR        5       >(text,text),
    FUNCTION        1       bttextcmp(text,text),
    FUNCTION        2       gin_extract_value_text(text, internal),
    FUNCTION        3       gin_extract_query_text(text, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_text(text,text,int2, internal),
STORAGE         varchar;

CREATE FUNCTION gin_extract_value_char("char", internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_char("char", "char", int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_char("char", internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS char_ops
DEFAULT FOR TYPE "char" USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btcharcmp("char","char"),
    FUNCTION        2       gin_extract_value_char("char", internal),
    FUNCTION        3       gin_extract_query_char("char", internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_char("char","char",int2, internal),
STORAGE         "char";

CREATE FUNCTION gin_extract_value_bytea(bytea, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_bytea(bytea, bytea, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_bytea(bytea, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS bytea_ops
DEFAULT FOR TYPE bytea USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       byteacmp(bytea,bytea),
    FUNCTION        2       gin_extract_value_bytea(bytea, internal),
    FUNCTION        3       gin_extract_query_bytea(bytea, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_bytea(bytea,bytea,int2, internal),
STORAGE         bytea;

CREATE FUNCTION gin_extract_value_bit(bit, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_bit(bit, bit, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_bit(bit, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS bit_ops
DEFAULT FOR TYPE bit USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       bitcmp(bit,bit),
    FUNCTION        2       gin_extract_value_bit(bit, internal),
    FUNCTION        3       gin_extract_query_bit(bit, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_bit(bit,bit,int2, internal),
STORAGE         bit;

CREATE FUNCTION gin_extract_value_varbit(varbit, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_varbit(varbit, varbit, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_varbit(varbit, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS varbit_ops
DEFAULT FOR TYPE varbit USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       varbitcmp(varbit,varbit),
    FUNCTION        2       gin_extract_value_varbit(varbit, internal),
    FUNCTION        3       gin_extract_query_varbit(varbit, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_varbit(varbit,varbit,int2, internal),
STORAGE         varbit;

CREATE FUNCTION gin_extract_value_numeric(numeric, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_numeric(numeric, numeric, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_numeric(numeric, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_numeric_cmp(numeric, numeric)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS numeric_ops
DEFAULT FOR TYPE numeric USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       gin_numeric_cmp(numeric,numeric),
    FUNCTION        2       gin_extract_value_numeric(numeric, internal),
    FUNCTION        3       gin_extract_query_numeric(numeric, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_numeric(numeric,numeric,int2, internal),
STORAGE         numeric;
"#,
    ),
    (
        "btree_gin--1.1--1.2.sql",
        r#"/* contrib/btree_gin/btree_gin--1.1--1.2.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gin UPDATE TO '1.1'" to load this file. \quit

--
--
--
-- enum ops
--
--


CREATE FUNCTION gin_extract_value_anyenum(anyenum, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_anyenum(anyenum, anyenum, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_anyenum(anyenum, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_enum_cmp(anyenum, anyenum)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS enum_ops
DEFAULT FOR TYPE anyenum USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       gin_enum_cmp(anyenum,anyenum),
    FUNCTION        2       gin_extract_value_anyenum(anyenum, internal),
    FUNCTION        3       gin_extract_query_anyenum(anyenum, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_anyenum(anyenum,anyenum,int2, internal),
STORAGE         anyenum;
"#,
    ),
    (
        "btree_gin--1.2--1.3.sql",
        r#"/* contrib/btree_gin/btree_gin--1.2--1.3.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gin UPDATE TO '1.3'" to load this file. \quit

-- uuid datatype support new in 1.3.
CREATE FUNCTION gin_extract_value_uuid(uuid, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_uuid(uuid, uuid, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_uuid(uuid, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS uuid_ops
DEFAULT FOR TYPE uuid USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       uuid_cmp(uuid,uuid),
    FUNCTION        2       gin_extract_value_uuid(uuid, internal),
    FUNCTION        3       gin_extract_query_uuid(uuid, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_uuid(uuid,uuid,int2, internal),
STORAGE         uuid;

-- name datatype support new in 1.3.
CREATE FUNCTION gin_extract_value_name(name, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_name(name, name, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_name(name, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS name_ops
DEFAULT FOR TYPE name USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btnamecmp(name,name),
    FUNCTION        2       gin_extract_value_name(name, internal),
    FUNCTION        3       gin_extract_query_name(name, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_name(name,name,int2, internal),
STORAGE         name;

-- bool datatype support new in 1.3.
CREATE FUNCTION gin_extract_value_bool(bool, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_bool(bool, bool, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_bool(bool, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS bool_ops
DEFAULT FOR TYPE bool USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       btboolcmp(bool,bool),
    FUNCTION        2       gin_extract_value_bool(bool, internal),
    FUNCTION        3       gin_extract_query_bool(bool, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_bool(bool,bool,int2, internal),
STORAGE         bool;

-- bpchar datatype support new in 1.3.
CREATE FUNCTION gin_extract_value_bpchar(bpchar, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_compare_prefix_bpchar(bpchar, bpchar, int2, internal)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION gin_extract_query_bpchar(bpchar, internal, int2, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR CLASS bpchar_ops
DEFAULT FOR TYPE bpchar USING gin
AS
    OPERATOR        1       <,
    OPERATOR        2       <=,
    OPERATOR        3       =,
    OPERATOR        4       >=,
    OPERATOR        5       >,
    FUNCTION        1       bpcharcmp(bpchar, bpchar),
    FUNCTION        2       gin_extract_value_bpchar(bpchar, internal),
    FUNCTION        3       gin_extract_query_bpchar(bpchar, internal, int2, internal, internal),
    FUNCTION        4       gin_btree_consistent(internal, int2, anyelement, int4, internal, internal),
    FUNCTION        5       gin_compare_prefix_bpchar(bpchar,bpchar,int2, internal),
STORAGE         bpchar;
"#,
    ),
    (
        "btree_gin--1.3--1.4.sql",
        r#"/* contrib/btree_gin/btree_gin--1.3--1.4.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gin UPDATE TO '1.4'" to load this file. \quit

--
-- Cross-type operator support is new in 1.4.  We only need to worry
-- about this for cross-type operators that exist in core.
--
-- Because the opclass extractQuery and consistent methods don't directly
-- get any information about the datatype of the RHS value, we have to
-- encode that in the operator strategy numbers.  The strategy numbers
-- are the operator's normal btree strategy (1-5) plus 16 times a code
-- for the RHS datatype.
--

ALTER OPERATOR FAMILY int2_ops USING gin
ADD
    -- Code 1: RHS is int4
    OPERATOR        0x11    < (int2, int4),
    OPERATOR        0x12    <= (int2, int4),
    OPERATOR        0x13    = (int2, int4),
    OPERATOR        0x14    >= (int2, int4),
    OPERATOR        0x15    > (int2, int4),
    -- Code 2: RHS is int8
    OPERATOR        0x21    < (int2, int8),
    OPERATOR        0x22    <= (int2, int8),
    OPERATOR        0x23    = (int2, int8),
    OPERATOR        0x24    >= (int2, int8),
    OPERATOR        0x25    > (int2, int8)
;

ALTER OPERATOR FAMILY int4_ops USING gin
ADD
    -- Code 1: RHS is int2
    OPERATOR        0x11    < (int4, int2),
    OPERATOR        0x12    <= (int4, int2),
    OPERATOR        0x13    = (int4, int2),
    OPERATOR        0x14    >= (int4, int2),
    OPERATOR        0x15    > (int4, int2),
    -- Code 2: RHS is int8
    OPERATOR        0x21    < (int4, int8),
    OPERATOR        0x22    <= (int4, int8),
    OPERATOR        0x23    = (int4, int8),
    OPERATOR        0x24    >= (int4, int8),
    OPERATOR        0x25    > (int4, int8)
;

ALTER OPERATOR FAMILY int8_ops USING gin
ADD
    -- Code 1: RHS is int2
    OPERATOR        0x11    < (int8, int2),
    OPERATOR        0x12    <= (int8, int2),
    OPERATOR        0x13    = (int8, int2),
    OPERATOR        0x14    >= (int8, int2),
    OPERATOR        0x15    > (int8, int2),
    -- Code 2: RHS is int4
    OPERATOR        0x21    < (int8, int4),
    OPERATOR        0x22    <= (int8, int4),
    OPERATOR        0x23    = (int8, int4),
    OPERATOR        0x24    >= (int8, int4),
    OPERATOR        0x25    > (int8, int4)
;

ALTER OPERATOR FAMILY float4_ops USING gin
ADD
    -- Code 1: RHS is float8
    OPERATOR        0x11    < (float4, float8),
    OPERATOR        0x12    <= (float4, float8),
    OPERATOR        0x13    = (float4, float8),
    OPERATOR        0x14    >= (float4, float8),
    OPERATOR        0x15    > (float4, float8)
;

ALTER OPERATOR FAMILY float8_ops USING gin
ADD
    -- Code 1: RHS is float4
    OPERATOR        0x11    < (float8, float4),
    OPERATOR        0x12    <= (float8, float4),
    OPERATOR        0x13    = (float8, float4),
    OPERATOR        0x14    >= (float8, float4),
    OPERATOR        0x15    > (float8, float4)
;

ALTER OPERATOR FAMILY text_ops USING gin
ADD
    -- Code 1: RHS is name
    OPERATOR        0x11    < (text, name),
    OPERATOR        0x12    <= (text, name),
    OPERATOR        0x13    = (text, name),
    OPERATOR        0x14    >= (text, name),
    OPERATOR        0x15    > (text, name)
;

ALTER OPERATOR FAMILY name_ops USING gin
ADD
    -- Code 1: RHS is text
    OPERATOR        0x11    < (name, text),
    OPERATOR        0x12    <= (name, text),
    OPERATOR        0x13    = (name, text),
    OPERATOR        0x14    >= (name, text),
    OPERATOR        0x15    > (name, text)
;

ALTER OPERATOR FAMILY date_ops USING gin
ADD
    -- Code 1: RHS is timestamp
    OPERATOR        0x11    < (date, timestamp),
    OPERATOR        0x12    <= (date, timestamp),
    OPERATOR        0x13    = (date, timestamp),
    OPERATOR        0x14    >= (date, timestamp),
    OPERATOR        0x15    > (date, timestamp),
    -- Code 2: RHS is timestamptz
    OPERATOR        0x21    < (date, timestamptz),
    OPERATOR        0x22    <= (date, timestamptz),
    OPERATOR        0x23    = (date, timestamptz),
    OPERATOR        0x24    >= (date, timestamptz),
    OPERATOR        0x25    > (date, timestamptz)
;

ALTER OPERATOR FAMILY timestamp_ops USING gin
ADD
    -- Code 1: RHS is date
    OPERATOR        0x11    < (timestamp, date),
    OPERATOR        0x12    <= (timestamp, date),
    OPERATOR        0x13    = (timestamp, date),
    OPERATOR        0x14    >= (timestamp, date),
    OPERATOR        0x15    > (timestamp, date),
    -- Code 2: RHS is timestamptz
    OPERATOR        0x21    < (timestamp, timestamptz),
    OPERATOR        0x22    <= (timestamp, timestamptz),
    OPERATOR        0x23    = (timestamp, timestamptz),
    OPERATOR        0x24    >= (timestamp, timestamptz),
    OPERATOR        0x25    > (timestamp, timestamptz)
;

ALTER OPERATOR FAMILY timestamptz_ops USING gin
ADD
    -- Code 1: RHS is date
    OPERATOR        0x11    < (timestamptz, date),
    OPERATOR        0x12    <= (timestamptz, date),
    OPERATOR        0x13    = (timestamptz, date),
    OPERATOR        0x14    >= (timestamptz, date),
    OPERATOR        0x15    > (timestamptz, date),
    -- Code 2: RHS is timestamp
    OPERATOR        0x21    < (timestamptz, timestamp),
    OPERATOR        0x22    <= (timestamptz, timestamp),
    OPERATOR        0x23    = (timestamptz, timestamp),
    OPERATOR        0x24    >= (timestamptz, timestamp),
    OPERATOR        0x25    > (timestamptz, timestamp)
;
"#,
    ),
    (
        "btree_gin.control",
        r#"# btree_gin extension
comment = 'support for indexing common datatypes in GIN'
default_version = '1.4'
module_pathname = '$libdir/btree_gin'
relocatable = true
trusted = true
"#,
    ),
];

/// The files of `contrib/btree_gist`.
pub(crate) const BTREE_GIST: &[(&str, &str)] = &[
    (
        "btree_gist--1.0--1.1.sql",
        r#"/* contrib/btree_gist/btree_gist--1.0--1.1.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.1'" to load this file. \quit

-- Index-only scan support new in 9.5.
CREATE FUNCTION gbt_oid_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_int2_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_int4_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_int8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_float4_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_float8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_ts_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_time_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_date_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_intv_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_cash_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_var_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

ALTER OPERATOR FAMILY gist_oid_ops USING gist ADD
	FUNCTION	9 (oid, oid) gbt_oid_fetch (internal) ;

ALTER OPERATOR FAMILY gist_int2_ops USING gist ADD
	FUNCTION	9 (int2, int2) gbt_int2_fetch (internal) ;

ALTER OPERATOR FAMILY gist_int4_ops USING gist ADD
	FUNCTION	9 (int4, int4) gbt_int4_fetch (internal) ;

ALTER OPERATOR FAMILY gist_int8_ops USING gist ADD
	FUNCTION	9 (int8, int8) gbt_int8_fetch (internal) ;

ALTER OPERATOR FAMILY gist_float4_ops USING gist ADD
	FUNCTION	9 (float4, float4) gbt_float4_fetch (internal) ;

ALTER OPERATOR FAMILY gist_float8_ops USING gist ADD
	FUNCTION	9 (float8, float8) gbt_float8_fetch (internal) ;

ALTER OPERATOR FAMILY gist_timestamp_ops USING gist ADD
	FUNCTION	9 (timestamp, timestamp) gbt_ts_fetch (internal) ;

ALTER OPERATOR FAMILY gist_timestamptz_ops USING gist ADD
	FUNCTION	9 (timestamptz, timestamptz) gbt_ts_fetch (internal) ;

ALTER OPERATOR FAMILY gist_time_ops USING gist ADD
	FUNCTION	9 (time, time) gbt_time_fetch (internal) ;

ALTER OPERATOR FAMILY gist_date_ops USING gist ADD
	FUNCTION	9 (date, date) gbt_date_fetch (internal) ;

ALTER OPERATOR FAMILY gist_interval_ops USING gist ADD
	FUNCTION	9 (interval, interval) gbt_intv_fetch (internal) ;

ALTER OPERATOR FAMILY gist_cash_ops USING gist ADD
	FUNCTION	9 (money, money) gbt_cash_fetch (internal) ;

ALTER OPERATOR FAMILY gist_macaddr_ops USING gist ADD
	FUNCTION	9 (macaddr, macaddr) gbt_macad_fetch (internal) ;

ALTER OPERATOR FAMILY gist_text_ops USING gist ADD
	FUNCTION	9 (text, text) gbt_var_fetch (internal) ;

ALTER OPERATOR FAMILY gist_bpchar_ops USING gist ADD
	FUNCTION	9 (bpchar, bpchar) gbt_var_fetch (internal) ;

ALTER OPERATOR FAMILY gist_bytea_ops USING gist ADD
	FUNCTION	9 (bytea, bytea) gbt_var_fetch (internal) ;

ALTER OPERATOR FAMILY gist_numeric_ops USING gist ADD
	FUNCTION	9 (numeric, numeric) gbt_var_fetch (internal) ;

ALTER OPERATOR FAMILY gist_bit_ops USING gist ADD
	FUNCTION	9 (bit, bit) gbt_var_fetch (internal) ;

ALTER OPERATOR FAMILY gist_vbit_ops USING gist ADD
	FUNCTION	9 (varbit, varbit) gbt_var_fetch (internal) ;
"#,
    ),
    (
        "btree_gist--1.1--1.2.sql",
        r#"/* contrib/btree_gist/btree_gist--1.1--1.2.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.2'" to load this file. \quit

-- Update procedure signatures the hard way.
-- We use to_regprocedure() so that query doesn't fail if run against 9.6beta1 definitions,
-- wherein the signatures have been updated already.  In that case to_regprocedure() will
-- return NULL and no updates will happen.

DO LANGUAGE plpgsql
$$
DECLARE
  my_schema pg_catalog.text := pg_catalog.quote_ident(pg_catalog.current_schema());
  old_path pg_catalog.text := pg_catalog.current_setting('search_path');
BEGIN
-- for safety, transiently set search_path to just pg_catalog+pg_temp
PERFORM pg_catalog.set_config('search_path', 'pg_catalog, pg_temp', true);

UPDATE pg_catalog.pg_proc SET
  proargtypes = pg_catalog.array_to_string(newtypes::pg_catalog.oid[], ' ')::pg_catalog.oidvector,
  pronargs = pg_catalog.array_length(newtypes, 1)
FROM (VALUES
(NULL::pg_catalog.text, NULL::pg_catalog.text[]), -- establish column types
('gbt_oid_distance(internal,oid,int2,oid)', '{internal,oid,int2,oid,internal}'),
('gbt_oid_union(bytea,internal)', '{internal,internal}'),
('gbt_oid_same(internal,internal,internal)', '{SCH.gbtreekey8,SCH.gbtreekey8,internal}'),
('gbt_int2_distance(internal,int2,int2,oid)', '{internal,int2,int2,oid,internal}'),
('gbt_int2_union(bytea,internal)', '{internal,internal}'),
('gbt_int2_same(internal,internal,internal)', '{SCH.gbtreekey4,SCH.gbtreekey4,internal}'),
('gbt_int4_distance(internal,int4,int2,oid)', '{internal,int4,int2,oid,internal}'),
('gbt_int4_union(bytea,internal)', '{internal,internal}'),
('gbt_int4_same(internal,internal,internal)', '{SCH.gbtreekey8,SCH.gbtreekey8,internal}'),
('gbt_int8_distance(internal,int8,int2,oid)', '{internal,int8,int2,oid,internal}'),
('gbt_int8_union(bytea,internal)', '{internal,internal}'),
('gbt_int8_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_float4_distance(internal,float4,int2,oid)', '{internal,float4,int2,oid,internal}'),
('gbt_float4_union(bytea,internal)', '{internal,internal}'),
('gbt_float4_same(internal,internal,internal)', '{SCH.gbtreekey8,SCH.gbtreekey8,internal}'),
('gbt_float8_distance(internal,float8,int2,oid)', '{internal,float8,int2,oid,internal}'),
('gbt_float8_union(bytea,internal)', '{internal,internal}'),
('gbt_float8_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_ts_distance(internal,timestamp,int2,oid)', '{internal,timestamp,int2,oid,internal}'),
('gbt_tstz_distance(internal,timestamptz,int2,oid)', '{internal,timestamptz,int2,oid,internal}'),
('gbt_ts_union(bytea,internal)', '{internal,internal}'),
('gbt_ts_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_time_distance(internal,time,int2,oid)', '{internal,time,int2,oid,internal}'),
('gbt_time_union(bytea,internal)', '{internal,internal}'),
('gbt_time_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_date_distance(internal,date,int2,oid)', '{internal,date,int2,oid,internal}'),
('gbt_date_union(bytea,internal)', '{internal,internal}'),
('gbt_date_same(internal,internal,internal)', '{SCH.gbtreekey8,SCH.gbtreekey8,internal}'),
('gbt_intv_distance(internal,interval,int2,oid)', '{internal,interval,int2,oid,internal}'),
('gbt_intv_union(bytea,internal)', '{internal,internal}'),
('gbt_intv_same(internal,internal,internal)', '{SCH.gbtreekey32,SCH.gbtreekey32,internal}'),
('gbt_cash_distance(internal,money,int2,oid)', '{internal,money,int2,oid,internal}'),
('gbt_cash_union(bytea,internal)', '{internal,internal}'),
('gbt_cash_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_macad_union(bytea,internal)', '{internal,internal}'),
('gbt_macad_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}'),
('gbt_text_union(bytea,internal)', '{internal,internal}'),
('gbt_text_same(internal,internal,internal)', '{SCH.gbtreekey_var,SCH.gbtreekey_var,internal}'),
('gbt_bytea_union(bytea,internal)', '{internal,internal}'),
('gbt_bytea_same(internal,internal,internal)', '{SCH.gbtreekey_var,SCH.gbtreekey_var,internal}'),
('gbt_numeric_union(bytea,internal)', '{internal,internal}'),
('gbt_numeric_same(internal,internal,internal)', '{SCH.gbtreekey_var,SCH.gbtreekey_var,internal}'),
('gbt_bit_union(bytea,internal)', '{internal,internal}'),
('gbt_bit_same(internal,internal,internal)', '{SCH.gbtreekey_var,SCH.gbtreekey_var,internal}'),
('gbt_inet_union(bytea,internal)', '{internal,internal}'),
('gbt_inet_same(internal,internal,internal)', '{SCH.gbtreekey16,SCH.gbtreekey16,internal}')
) AS update_data (oldproc, newtypestext),
LATERAL (
  SELECT array_agg(replace(typ, 'SCH', my_schema)::regtype) as newtypes FROM unnest(newtypestext) typ
) ls
WHERE oid = to_regprocedure(my_schema || '.' || replace(oldproc, 'SCH', my_schema));

PERFORM pg_catalog.set_config('search_path', old_path, true);
END
$$;
"#,
    ),
    (
        "btree_gist--1.2--1.3.sql",
        r#"/* contrib/btree_gist/btree_gist--1.2--1.3.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.3'" to load this file. \quit

-- Add support for indexing UUID columns

-- define the GiST support methods
CREATE FUNCTION gbt_uuid_consistent(internal,uuid,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_union(internal, internal)
RETURNS gbtreekey32
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_uuid_same(gbtreekey32, gbtreekey32, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

-- Create the operator class
CREATE OPERATOR CLASS gist_uuid_ops
DEFAULT FOR TYPE uuid USING gist
AS
	OPERATOR	1	<   ,
	OPERATOR	2	<=  ,
	OPERATOR	3	=   ,
	OPERATOR	4	>=  ,
	OPERATOR	5	>   ,
	FUNCTION	1	gbt_uuid_consistent (internal, uuid, int2, oid, internal),
	FUNCTION	2	gbt_uuid_union (internal, internal),
	FUNCTION	3	gbt_uuid_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_uuid_penalty (internal, internal, internal),
	FUNCTION	6	gbt_uuid_picksplit (internal, internal),
	FUNCTION	7	gbt_uuid_same (gbtreekey32, gbtreekey32, internal),
	STORAGE		gbtreekey32;

-- These are "loose" in the opfamily for consistency with the rest of btree_gist
ALTER OPERATOR FAMILY gist_uuid_ops USING gist ADD
	OPERATOR	6	<>  (uuid, uuid) ,
	FUNCTION	9 (uuid, uuid) gbt_uuid_fetch (internal) ;
"#,
    ),
    (
        "btree_gist--1.3--1.4.sql",
        r#"/* contrib/btree_gist/btree_gist--1.3--1.4.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.4'" to load this file. \quit

-- Add support for indexing macaddr8 columns

-- define the GiST support methods
CREATE FUNCTION gbt_macad8_consistent(internal,macaddr8,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_macad8_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

-- Create the operator class
CREATE OPERATOR CLASS gist_macaddr8_ops
DEFAULT FOR TYPE macaddr8 USING gist
AS
	OPERATOR	1	< ,
	OPERATOR	2	<= ,
	OPERATOR	3	= ,
	OPERATOR	4	>= ,
	OPERATOR	5	> ,
	FUNCTION	1	gbt_macad8_consistent (internal, macaddr8, int2, oid, internal),
	FUNCTION	2	gbt_macad8_union (internal, internal),
	FUNCTION	3	gbt_macad8_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_macad8_penalty (internal, internal, internal),
	FUNCTION	6	gbt_macad8_picksplit (internal, internal),
	FUNCTION	7	gbt_macad8_same (gbtreekey16, gbtreekey16, internal),
	STORAGE		gbtreekey16;

ALTER OPERATOR FAMILY gist_macaddr8_ops USING gist ADD
	OPERATOR	6	<> (macaddr8, macaddr8) ,
	FUNCTION	9 (macaddr8, macaddr8) gbt_macad8_fetch (internal);
"#,
    ),
    (
        "btree_gist--1.4--1.5.sql",
        r#"/* contrib/btree_gist/btree_gist--1.4--1.5.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.5'" to load this file. \quit

--
--
--
-- enum ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_enum_consistent(internal,anyenum,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_enum_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

-- Create the operator class
CREATE OPERATOR CLASS gist_enum_ops
DEFAULT FOR TYPE anyenum USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	FUNCTION	1	gbt_enum_consistent (internal, anyenum, int2, oid, internal),
	FUNCTION	2	gbt_enum_union (internal, internal),
	FUNCTION	3	gbt_enum_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_enum_penalty (internal, internal, internal),
	FUNCTION	6	gbt_enum_picksplit (internal, internal),
	FUNCTION	7	gbt_enum_same (gbtreekey8, gbtreekey8, internal),
	STORAGE		gbtreekey8;

ALTER OPERATOR FAMILY gist_enum_ops USING gist ADD
	OPERATOR	6	<> (anyenum, anyenum) ,
	FUNCTION	9 (anyenum, anyenum) gbt_enum_fetch (internal) ;
"#,
    ),
    (
        "btree_gist--1.5--1.6.sql",
        r#"/* contrib/btree_gist/btree_gist--1.5--1.6.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.6'" to load this file. \quit

-- This upgrade script marks all btree_gist functions as parallel safe.

-- Input/output functions for GiST key types (gbtreekey*)
ALTER FUNCTION gbtreekey4_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gbtreekey4_out(gbtreekey4) PARALLEL SAFE;
ALTER FUNCTION gbtreekey8_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gbtreekey8_out(gbtreekey8) PARALLEL SAFE;
ALTER FUNCTION gbtreekey16_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gbtreekey16_out(gbtreekey16) PARALLEL SAFE;
ALTER FUNCTION gbtreekey32_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gbtreekey32_out(gbtreekey32) PARALLEL SAFE;
ALTER FUNCTION gbtreekey_var_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gbtreekey_var_out(gbtreekey_var) PARALLEL SAFE;

-- Functions, which implement distance operators (<->)
ALTER FUNCTION cash_dist(money, money) PARALLEL SAFE;
ALTER FUNCTION date_dist(date, date) PARALLEL SAFE;
ALTER FUNCTION float4_dist(real, real) PARALLEL SAFE;
ALTER FUNCTION float8_dist(double precision, double precision) PARALLEL SAFE;
ALTER FUNCTION int2_dist(smallint, smallint) PARALLEL SAFE;
ALTER FUNCTION int4_dist(integer, integer) PARALLEL SAFE;
ALTER FUNCTION int8_dist(bigint, bigint) PARALLEL SAFE;
ALTER FUNCTION interval_dist(interval, interval) PARALLEL SAFE;
ALTER FUNCTION oid_dist(oid, oid) PARALLEL SAFE;
ALTER FUNCTION time_dist(time without time zone, time without time zone) PARALLEL SAFE;
ALTER FUNCTION ts_dist(timestamp without time zone, timestamp without time zone) PARALLEL SAFE;
ALTER FUNCTION tstz_dist(timestamp with time zone, timestamp with time zone) PARALLEL SAFE;

-- GiST support methods
ALTER FUNCTION gbt_oid_consistent(internal, oid, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_distance(internal, oid, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_decompress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_var_decompress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_var_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_oid_same(gbtreekey8, gbtreekey8, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_consistent(internal, smallint, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_distance(internal, smallint, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int2_same(gbtreekey4, gbtreekey4, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_consistent(internal, integer, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_distance(internal, integer, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int4_same(gbtreekey8, gbtreekey8, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_consistent(internal, bigint, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_distance(internal, bigint, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_int8_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_consistent(internal, real, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_distance(internal, real, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float4_same(gbtreekey8, gbtreekey8, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_consistent(internal, double precision, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_distance(internal, double precision, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_float8_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_consistent(internal, timestamp without time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_distance(internal, timestamp without time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_tstz_consistent(internal, timestamp with time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_tstz_distance(internal, timestamp with time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_tstz_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_ts_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_consistent(internal, time without time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_distance(internal, time without time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_timetz_consistent(internal, time with time zone, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_timetz_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_time_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_consistent(internal, date, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_distance(internal, date, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_date_same(gbtreekey8, gbtreekey8, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_consistent(internal, interval, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_distance(internal, interval, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_decompress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_intv_same(gbtreekey32, gbtreekey32, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_consistent(internal, money, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_distance(internal, money, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_cash_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_consistent(internal, macaddr, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_consistent(internal, text, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bpchar_consistent(internal, character, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bpchar_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_text_same(gbtreekey_var, gbtreekey_var, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_consistent(internal, bytea, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bytea_same(gbtreekey_var, gbtreekey_var, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_consistent(internal, numeric, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_numeric_same(gbtreekey_var, gbtreekey_var, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_consistent(internal, bit, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bit_same(gbtreekey_var, gbtreekey_var, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_consistent(internal, inet, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_inet_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_consistent(internal, uuid, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_uuid_same(gbtreekey32, gbtreekey32, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_consistent(internal, macaddr8, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_macad8_same(gbtreekey16, gbtreekey16, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_consistent(internal, anyenum, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_enum_same(gbtreekey8, gbtreekey8, internal) PARALLEL SAFE;
"#,
    ),
    (
        "btree_gist--1.6--1.7.sql",
        r#"/* contrib/btree_gist/btree_gist--1.6--1.7.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.7'" to load this file. \quit

-- This upgrade scripts add support for bool.
CREATE FUNCTION gbtreekey2_in(cstring)
RETURNS gbtreekey2
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey2_out(gbtreekey2)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey2 (
	INTERNALLENGTH = 2,
	INPUT  = gbtreekey2_in,
	OUTPUT = gbtreekey2_out
);

-- Define the GiST support methods
CREATE FUNCTION gbt_bool_consistent(internal,bool,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_union(internal, internal)
RETURNS gbtreekey2
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

CREATE FUNCTION gbt_bool_same(gbtreekey2, gbtreekey2, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

-- Create the operator class
CREATE OPERATOR CLASS gist_bool_ops
DEFAULT FOR TYPE bool USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bool_consistent (internal, bool, int2, oid, internal),
	FUNCTION	2	gbt_bool_union (internal, internal),
	FUNCTION	3	gbt_bool_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_bool_penalty (internal, internal, internal),
	FUNCTION	6	gbt_bool_picksplit (internal, internal),
	FUNCTION	7	gbt_bool_same (gbtreekey2, gbtreekey2, internal),
	FUNCTION	9   gbt_bool_fetch (internal),
	STORAGE		gbtreekey2;
"#,
    ),
    (
        "btree_gist--1.7--1.8.sql",
        r#"/* contrib/btree_gist/btree_gist--1.7--1.8.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.8'" to load this file. \quit

-- Add sortsupport functions

CREATE FUNCTION gbt_bit_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_varbit_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_bool_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_bytea_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_cash_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_date_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_enum_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_float4_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_float8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_inet_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_int2_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_int4_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_int8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_intv_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_macaddr_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_macad8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_numeric_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_oid_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_text_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_bpchar_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_time_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_ts_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

CREATE FUNCTION gbt_uuid_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

ALTER OPERATOR FAMILY gist_bit_ops USING gist ADD
	FUNCTION	11  (bit, bit) gbt_bit_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_vbit_ops USING gist ADD
	FUNCTION	11  (varbit, varbit) gbt_varbit_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_bool_ops USING gist ADD
	FUNCTION	11  (bool, bool) gbt_bool_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_bytea_ops USING gist ADD
	FUNCTION	11  (bytea, bytea) gbt_bytea_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_cash_ops USING gist ADD
	FUNCTION	11  (money, money) gbt_cash_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_date_ops USING gist ADD
	FUNCTION	11  (date, date) gbt_date_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_enum_ops USING gist ADD
	FUNCTION	11  (anyenum, anyenum) gbt_enum_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_float4_ops USING gist ADD
	FUNCTION	11  (float4, float4) gbt_float4_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_float8_ops USING gist ADD
	FUNCTION	11  (float8, float8) gbt_float8_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_inet_ops USING gist ADD
	FUNCTION	11  (inet, inet) gbt_inet_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_cidr_ops USING gist ADD
	FUNCTION	11  (cidr, cidr) gbt_inet_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_int2_ops USING gist ADD
	FUNCTION	11  (int2, int2) gbt_int2_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_int4_ops USING gist ADD
	FUNCTION	11  (int4, int4) gbt_int4_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_int8_ops USING gist ADD
	FUNCTION	11  (int8, int8) gbt_int8_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_interval_ops USING gist ADD
	FUNCTION	11  (interval, interval) gbt_intv_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_macaddr_ops USING gist ADD
	FUNCTION	11  (macaddr, macaddr) gbt_macaddr_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_macaddr8_ops USING gist ADD
	FUNCTION	11  (macaddr8, macaddr8) gbt_macad8_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_numeric_ops USING gist ADD
	FUNCTION	11  (numeric, numeric) gbt_numeric_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_oid_ops USING gist ADD
	FUNCTION	11  (oid, oid) gbt_oid_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_text_ops USING gist ADD
	FUNCTION	11  (text, text) gbt_text_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_bpchar_ops USING gist ADD
	FUNCTION	11  (bpchar, bpchar) gbt_bpchar_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_time_ops USING gist ADD
	FUNCTION	11  (time, time) gbt_time_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_timetz_ops USING gist ADD
	FUNCTION	11  (timetz, timetz) gbt_time_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_timestamp_ops USING gist ADD
	FUNCTION	11  (timestamp, timestamp) gbt_ts_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_timestamptz_ops USING gist ADD
	FUNCTION	11  (timestamptz, timestamptz) gbt_ts_sortsupport (internal) ;

ALTER OPERATOR FAMILY gist_uuid_ops USING gist ADD
	FUNCTION	11  (uuid, uuid) gbt_uuid_sortsupport (internal) ;

-- Add translate_cmptype functions

CREATE FUNCTION gist_translate_cmptype_btree(int)
RETURNS smallint
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE PARALLEL SAFE STRICT;

ALTER OPERATOR FAMILY gist_oid_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_int2_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_int4_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_int8_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_float4_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_float8_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_timestamp_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_timestamptz_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_time_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_date_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_interval_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_cash_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_macaddr_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_text_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_bpchar_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_bytea_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_numeric_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_bit_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_vbit_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_inet_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_cidr_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_timetz_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_uuid_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_macaddr8_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_enum_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;

ALTER OPERATOR FAMILY gist_bool_ops USING gist ADD
	FUNCTION 12 ("any", "any") gist_translate_cmptype_btree (int) ;
"#,
    ),
    (
        "btree_gist--1.8--1.9.sql",
        r#"/* contrib/btree_gist/btree_gist--1.8--1.9.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "ALTER EXTENSION btree_gist UPDATE TO '1.9'" to load this file. \quit

--
-- Mark gist_inet_ops and gist_cidr_ops opclasses as non-default.
-- This is the first step on the way to eventually removing them.
--
-- There's no SQL command for this, so fake it with a manual update on
-- pg_opclass.
--
DO LANGUAGE plpgsql
$$
DECLARE
  my_schema pg_catalog.text := pg_catalog.quote_ident(pg_catalog.current_schema());
  old_path pg_catalog.text := pg_catalog.current_setting('search_path');
BEGIN
-- for safety, transiently set search_path to just pg_catalog+pg_temp
PERFORM pg_catalog.set_config('search_path', 'pg_catalog, pg_temp', true);

UPDATE pg_catalog.pg_opclass
SET opcdefault = false
WHERE opcmethod = (SELECT oid FROM pg_catalog.pg_am WHERE amname = 'gist') AND
      opcname IN ('gist_inet_ops', 'gist_cidr_ops') AND
      opcnamespace = my_schema::pg_catalog.regnamespace;

PERFORM pg_catalog.set_config('search_path', old_path, true);
END
$$;


-- Fix parallel-safety markings overlooked in btree_gist--1.6--1.7.sql.
ALTER FUNCTION gbt_bool_consistent(internal, bool, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_fetch(internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gbt_bool_same(gbtreekey2, gbtreekey2, internal) PARALLEL SAFE;
"#,
    ),
    (
        "btree_gist--1.9.sql",
        r#"/* contrib/btree_gist/btree_gist--1.9.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "CREATE EXTENSION btree_gist" to load this file. \quit

CREATE FUNCTION gbtreekey2_in(cstring)
RETURNS gbtreekey2
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey2_out(gbtreekey2)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey2 (
	INTERNALLENGTH = 2,
	INPUT  = gbtreekey2_in,
	OUTPUT = gbtreekey2_out
);

CREATE FUNCTION gbtreekey4_in(cstring)
RETURNS gbtreekey4
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey4_out(gbtreekey4)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey4 (
	INTERNALLENGTH = 4,
	INPUT  = gbtreekey4_in,
	OUTPUT = gbtreekey4_out
);

CREATE FUNCTION gbtreekey8_in(cstring)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey8_out(gbtreekey8)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey8 (
	INTERNALLENGTH = 8,
	INPUT  = gbtreekey8_in,
	OUTPUT = gbtreekey8_out
);

CREATE FUNCTION gbtreekey16_in(cstring)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey16_out(gbtreekey16)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey16 (
	INTERNALLENGTH = 16,
	INPUT  = gbtreekey16_in,
	OUTPUT = gbtreekey16_out
);

CREATE FUNCTION gbtreekey32_in(cstring)
RETURNS gbtreekey32
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey32_out(gbtreekey32)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey32 (
	INTERNALLENGTH = 32,
	INPUT  = gbtreekey32_in,
	OUTPUT = gbtreekey32_out
);

CREATE FUNCTION gbtreekey_var_in(cstring)
RETURNS gbtreekey_var
AS 'MODULE_PATHNAME', 'gbtreekey_in'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbtreekey_var_out(gbtreekey_var)
RETURNS cstring
AS 'MODULE_PATHNAME', 'gbtreekey_out'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE TYPE gbtreekey_var (
	INTERNALLENGTH = VARIABLE,
	INPUT  = gbtreekey_var_in,
	OUTPUT = gbtreekey_var_out,
	STORAGE = EXTENDED
);

--common support functions

CREATE FUNCTION gist_translate_cmptype_btree(int)
RETURNS smallint
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

--distance operators

CREATE FUNCTION cash_dist(money, money)
RETURNS money
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = money,
	RIGHTARG = money,
	PROCEDURE = cash_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION date_dist(date, date)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = date,
	RIGHTARG = date,
	PROCEDURE = date_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION float4_dist(float4, float4)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = float4,
	RIGHTARG = float4,
	PROCEDURE = float4_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION float8_dist(float8, float8)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = float8,
	RIGHTARG = float8,
	PROCEDURE = float8_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION int2_dist(int2, int2)
RETURNS int2
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = int2,
	RIGHTARG = int2,
	PROCEDURE = int2_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION int4_dist(int4, int4)
RETURNS int4
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = int4,
	RIGHTARG = int4,
	PROCEDURE = int4_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION int8_dist(int8, int8)
RETURNS int8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = int8,
	RIGHTARG = int8,
	PROCEDURE = int8_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION interval_dist(interval, interval)
RETURNS interval
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = interval,
	RIGHTARG = interval,
	PROCEDURE = interval_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION oid_dist(oid, oid)
RETURNS oid
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = oid,
	RIGHTARG = oid,
	PROCEDURE = oid_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION time_dist(time, time)
RETURNS interval
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = time,
	RIGHTARG = time,
	PROCEDURE = time_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION ts_dist(timestamp, timestamp)
RETURNS interval
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = timestamp,
	RIGHTARG = timestamp,
	PROCEDURE = ts_dist,
	COMMUTATOR = '<->'
);

CREATE FUNCTION tstz_dist(timestamptz, timestamptz)
RETURNS interval
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR <-> (
	LEFTARG = timestamptz,
	RIGHTARG = timestamptz,
	PROCEDURE = tstz_dist,
	COMMUTATOR = '<->'
);


--
--
--
-- oid ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_oid_consistent(internal,oid,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_distance(internal,oid,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_decompress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_var_decompress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_var_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_oid_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_oid_ops
DEFAULT FOR TYPE oid USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.oid_ops ,
	FUNCTION	1	gbt_oid_consistent (internal, oid, int2, oid, internal),
	FUNCTION	2	gbt_oid_union (internal, internal),
	FUNCTION	3	gbt_oid_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_oid_penalty (internal, internal, internal),
	FUNCTION	6	gbt_oid_picksplit (internal, internal),
	FUNCTION	7	gbt_oid_same (gbtreekey8, gbtreekey8, internal),
	FUNCTION	8	gbt_oid_distance (internal, oid, int2, oid, internal),
	FUNCTION	9	gbt_oid_fetch (internal),
	FUNCTION	11	gbt_oid_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey8;


--
--
--
-- int2 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_int2_consistent(internal,int2,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_distance(internal,int2,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_union(internal, internal)
RETURNS gbtreekey4
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_same(gbtreekey4, gbtreekey4, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int2_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_int2_ops
DEFAULT FOR TYPE int2 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.integer_ops ,
	FUNCTION	1	gbt_int2_consistent (internal, int2, int2, oid, internal),
	FUNCTION	2	gbt_int2_union (internal, internal),
	FUNCTION	3	gbt_int2_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_int2_penalty (internal, internal, internal),
	FUNCTION	6	gbt_int2_picksplit (internal, internal),
	FUNCTION	7	gbt_int2_same (gbtreekey4, gbtreekey4, internal),
	FUNCTION	8	gbt_int2_distance (internal, int2, int2, oid, internal),
	FUNCTION	9	gbt_int2_fetch (internal),
	FUNCTION	11	gbt_int2_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey4;


--
--
--
-- int4 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_int4_consistent(internal,int4,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_distance(internal,int4,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int4_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_int4_ops
DEFAULT FOR TYPE int4 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.integer_ops ,
	FUNCTION	1	gbt_int4_consistent (internal, int4, int2, oid, internal),
	FUNCTION	2	gbt_int4_union (internal, internal),
	FUNCTION	3	gbt_int4_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_int4_penalty (internal, internal, internal),
	FUNCTION	6	gbt_int4_picksplit (internal, internal),
	FUNCTION	7	gbt_int4_same (gbtreekey8, gbtreekey8, internal),
	FUNCTION	8	gbt_int4_distance (internal, int4, int2, oid, internal),
	FUNCTION	9	gbt_int4_fetch (internal),
	FUNCTION	11	gbt_int4_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey8;


--
--
--
-- int8 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_int8_consistent(internal,int8,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_distance(internal,int8,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_int8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_int8_ops
DEFAULT FOR TYPE int8 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.integer_ops ,
	FUNCTION	1	gbt_int8_consistent (internal, int8, int2, oid, internal),
	FUNCTION	2	gbt_int8_union (internal, internal),
	FUNCTION	3	gbt_int8_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_int8_penalty (internal, internal, internal),
	FUNCTION	6	gbt_int8_picksplit (internal, internal),
	FUNCTION	7	gbt_int8_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_int8_distance (internal, int8, int2, oid, internal),
	FUNCTION	9	gbt_int8_fetch (internal),
	FUNCTION	11	gbt_int8_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- float4 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_float4_consistent(internal,float4,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_distance(internal,float4,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float4_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_float4_ops
DEFAULT FOR TYPE float4 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.float_ops ,
	FUNCTION	1	gbt_float4_consistent (internal, float4, int2, oid, internal),
	FUNCTION	2	gbt_float4_union (internal, internal),
	FUNCTION	3	gbt_float4_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_float4_penalty (internal, internal, internal),
	FUNCTION	6	gbt_float4_picksplit (internal, internal),
	FUNCTION	7	gbt_float4_same (gbtreekey8, gbtreekey8, internal),
	FUNCTION	8	gbt_float4_distance (internal, float4, int2, oid, internal),
	FUNCTION	9	gbt_float4_fetch (internal),
	FUNCTION	11	gbt_float4_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey8;


--
--
--
-- float8 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_float8_consistent(internal,float8,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_distance(internal,float8,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_float8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_float8_ops
DEFAULT FOR TYPE float8 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.float_ops ,
	FUNCTION	1	gbt_float8_consistent (internal, float8, int2, oid, internal),
	FUNCTION	2	gbt_float8_union (internal, internal),
	FUNCTION	3	gbt_float8_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_float8_penalty (internal, internal, internal),
	FUNCTION	6	gbt_float8_picksplit (internal, internal),
	FUNCTION	7	gbt_float8_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_float8_distance (internal, float8, int2, oid, internal),
	FUNCTION	9	gbt_float8_fetch (internal),
	FUNCTION	11	gbt_float8_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- timestamp ops
--
--
--

CREATE FUNCTION gbt_ts_consistent(internal,timestamp,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_distance(internal,timestamp,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_tstz_consistent(internal,timestamptz,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_tstz_distance(internal,timestamptz,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_tstz_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_ts_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_timestamp_ops
DEFAULT FOR TYPE timestamp USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.interval_ops ,
	FUNCTION	1	gbt_ts_consistent (internal, timestamp, int2, oid, internal),
	FUNCTION	2	gbt_ts_union (internal, internal),
	FUNCTION	3	gbt_ts_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_ts_penalty (internal, internal, internal),
	FUNCTION	6	gbt_ts_picksplit (internal, internal),
	FUNCTION	7	gbt_ts_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_ts_distance (internal, timestamp, int2, oid, internal),
	FUNCTION	9	gbt_ts_fetch (internal),
	FUNCTION	11	gbt_ts_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;

-- Create the operator class
CREATE OPERATOR CLASS gist_timestamptz_ops
DEFAULT FOR TYPE timestamptz USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.interval_ops ,
	FUNCTION	1	gbt_tstz_consistent (internal, timestamptz, int2, oid, internal),
	FUNCTION	2	gbt_ts_union (internal, internal),
	FUNCTION	3	gbt_tstz_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_ts_penalty (internal, internal, internal),
	FUNCTION	6	gbt_ts_picksplit (internal, internal),
	FUNCTION	7	gbt_ts_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_tstz_distance (internal, timestamptz, int2, oid, internal),
	FUNCTION	9	gbt_ts_fetch (internal),
	FUNCTION	11	gbt_ts_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- time ops
--
--
--

CREATE FUNCTION gbt_time_consistent(internal,time,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_distance(internal,time,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_timetz_consistent(internal,timetz,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_timetz_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_time_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_time_ops
DEFAULT FOR TYPE time USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.interval_ops ,
	FUNCTION	1	gbt_time_consistent (internal, time, int2, oid, internal),
	FUNCTION	2	gbt_time_union (internal, internal),
	FUNCTION	3	gbt_time_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_time_penalty (internal, internal, internal),
	FUNCTION	6	gbt_time_picksplit (internal, internal),
	FUNCTION	7	gbt_time_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_time_distance (internal, time, int2, oid, internal),
	FUNCTION	9	gbt_time_fetch (internal),
	FUNCTION	11	gbt_time_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;

CREATE OPERATOR CLASS gist_timetz_ops
DEFAULT FOR TYPE timetz USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_timetz_consistent (internal, timetz, int2, oid, internal),
	FUNCTION	2	gbt_time_union (internal, internal),
	FUNCTION	3	gbt_timetz_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_time_penalty (internal, internal, internal),
	FUNCTION	6	gbt_time_picksplit (internal, internal),
	FUNCTION	7	gbt_time_same (gbtreekey16, gbtreekey16, internal),
	-- no 'fetch' function, as the compress function is lossy.
	FUNCTION	11	gbt_time_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- date ops
--
--
--

CREATE FUNCTION gbt_date_consistent(internal,date,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_distance(internal,date,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_date_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_date_ops
DEFAULT FOR TYPE date USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.integer_ops ,
	FUNCTION	1	gbt_date_consistent (internal, date, int2, oid, internal),
	FUNCTION	2	gbt_date_union (internal, internal),
	FUNCTION	3	gbt_date_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_date_penalty (internal, internal, internal),
	FUNCTION	6	gbt_date_picksplit (internal, internal),
	FUNCTION	7	gbt_date_same (gbtreekey8, gbtreekey8, internal),
	FUNCTION	8	gbt_date_distance (internal, date, int2, oid, internal),
	FUNCTION	9	gbt_date_fetch (internal),
	FUNCTION	11	gbt_date_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey8;


--
--
--
-- interval ops
--
--
--

CREATE FUNCTION gbt_intv_consistent(internal,interval,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_distance(internal,interval,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_decompress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_union(internal, internal)
RETURNS gbtreekey32
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_same(gbtreekey32, gbtreekey32, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_intv_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_interval_ops
DEFAULT FOR TYPE interval USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.interval_ops ,
	FUNCTION	1	gbt_intv_consistent (internal, interval, int2, oid, internal),
	FUNCTION	2	gbt_intv_union (internal, internal),
	FUNCTION	3	gbt_intv_compress (internal),
	FUNCTION	4	gbt_intv_decompress (internal),
	FUNCTION	5	gbt_intv_penalty (internal, internal, internal),
	FUNCTION	6	gbt_intv_picksplit (internal, internal),
	FUNCTION	7	gbt_intv_same (gbtreekey32, gbtreekey32, internal),
	FUNCTION	8	gbt_intv_distance (internal, interval, int2, oid, internal),
	FUNCTION	9	gbt_intv_fetch (internal),
	FUNCTION	11	gbt_intv_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey32;


--
--
--
-- cash ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_cash_consistent(internal,money,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_distance(internal,money,int2,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_cash_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_cash_ops
DEFAULT FOR TYPE money USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	OPERATOR	15	<-> FOR ORDER BY pg_catalog.money_ops ,
	FUNCTION	1	gbt_cash_consistent (internal, money, int2, oid, internal),
	FUNCTION	2	gbt_cash_union (internal, internal),
	FUNCTION	3	gbt_cash_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_cash_penalty (internal, internal, internal),
	FUNCTION	6	gbt_cash_picksplit (internal, internal),
	FUNCTION	7	gbt_cash_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	8	gbt_cash_distance (internal, money, int2, oid, internal),
	FUNCTION	9	gbt_cash_fetch (internal),
	FUNCTION	11	gbt_cash_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- macaddr ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_macad_consistent(internal,macaddr,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macaddr_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_macaddr_ops
DEFAULT FOR TYPE macaddr USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_macad_consistent (internal, macaddr, int2, oid, internal),
	FUNCTION	2	gbt_macad_union (internal, internal),
	FUNCTION	3	gbt_macad_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_macad_penalty (internal, internal, internal),
	FUNCTION	6	gbt_macad_picksplit (internal, internal),
	FUNCTION	7	gbt_macad_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	9	gbt_macad_fetch (internal),
	FUNCTION	11	gbt_macaddr_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- text/bpchar ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_text_consistent(internal,text,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bpchar_consistent(internal,bpchar,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bpchar_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_union(internal, internal)
RETURNS gbtreekey_var
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_same(gbtreekey_var, gbtreekey_var, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_text_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bpchar_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_text_ops
DEFAULT FOR TYPE text USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_text_consistent (internal, text, int2, oid, internal),
	FUNCTION	2	gbt_text_union (internal, internal),
	FUNCTION	3	gbt_text_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_text_penalty (internal, internal, internal),
	FUNCTION	6	gbt_text_picksplit (internal, internal),
	FUNCTION	7	gbt_text_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_text_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;

---- Create the operator class
CREATE OPERATOR CLASS gist_bpchar_ops
DEFAULT FOR TYPE bpchar USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bpchar_consistent (internal, bpchar , int2, oid, internal),
	FUNCTION	2	gbt_text_union (internal, internal),
	FUNCTION	3	gbt_bpchar_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_text_penalty (internal, internal, internal),
	FUNCTION	6	gbt_text_picksplit (internal, internal),
	FUNCTION	7	gbt_text_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_bpchar_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;


--
--
-- bytea ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_bytea_consistent(internal,bytea,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_union(internal, internal)
RETURNS gbtreekey_var
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_same(gbtreekey_var, gbtreekey_var, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bytea_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_bytea_ops
DEFAULT FOR TYPE bytea USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bytea_consistent (internal, bytea, int2, oid, internal),
	FUNCTION	2	gbt_bytea_union (internal, internal),
	FUNCTION	3	gbt_bytea_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_bytea_penalty (internal, internal, internal),
	FUNCTION	6	gbt_bytea_picksplit (internal, internal),
	FUNCTION	7	gbt_bytea_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_bytea_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;


--
--
--
-- numeric ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_numeric_consistent(internal,numeric,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_union(internal, internal)
RETURNS gbtreekey_var
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_same(gbtreekey_var, gbtreekey_var, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_numeric_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_numeric_ops
DEFAULT FOR TYPE numeric USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_numeric_consistent (internal, numeric, int2, oid, internal),
	FUNCTION	2	gbt_numeric_union (internal, internal),
	FUNCTION	3	gbt_numeric_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_numeric_penalty (internal, internal, internal),
	FUNCTION	6	gbt_numeric_picksplit (internal, internal),
	FUNCTION	7	gbt_numeric_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_numeric_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;


--
--
-- bit ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_bit_consistent(internal,bit,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_union(internal, internal)
RETURNS gbtreekey_var
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_same(gbtreekey_var, gbtreekey_var, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bit_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_varbit_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_bit_ops
DEFAULT FOR TYPE bit USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bit_consistent (internal, bit, int2, oid, internal),
	FUNCTION	2	gbt_bit_union (internal, internal),
	FUNCTION	3	gbt_bit_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_bit_penalty (internal, internal, internal),
	FUNCTION	6	gbt_bit_picksplit (internal, internal),
	FUNCTION	7	gbt_bit_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_bit_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;

-- Create the operator class
CREATE OPERATOR CLASS gist_vbit_ops
DEFAULT FOR TYPE varbit USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bit_consistent (internal, bit, int2, oid, internal),
	FUNCTION	2	gbt_bit_union (internal, internal),
	FUNCTION	3	gbt_bit_compress (internal),
	FUNCTION	4	gbt_var_decompress (internal),
	FUNCTION	5	gbt_bit_penalty (internal, internal, internal),
	FUNCTION	6	gbt_bit_picksplit (internal, internal),
	FUNCTION	7	gbt_bit_same (gbtreekey_var, gbtreekey_var, internal),
	FUNCTION	9	gbt_var_fetch (internal),
	FUNCTION	11	gbt_varbit_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE			gbtreekey_var;


--
--
--
-- inet/cidr ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_inet_consistent(internal,inet,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_inet_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class (intentionally not DEFAULT)
CREATE OPERATOR CLASS gist_inet_ops
FOR TYPE inet USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_inet_consistent (internal, inet, int2, oid, internal),
	FUNCTION	2	gbt_inet_union (internal, internal),
	FUNCTION	3	gbt_inet_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_inet_penalty (internal, internal, internal),
	FUNCTION	6	gbt_inet_picksplit (internal, internal),
	FUNCTION	7	gbt_inet_same (gbtreekey16, gbtreekey16, internal),
	-- no fetch support, the compress function is lossy
	FUNCTION	11	gbt_inet_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;

-- Create the operator class (intentionally not DEFAULT)
CREATE OPERATOR CLASS gist_cidr_ops
FOR TYPE cidr USING gist
AS
	OPERATOR	1	<  (inet, inet) ,
	OPERATOR	2	<= (inet, inet) ,
	OPERATOR	3	=  (inet, inet) ,
	OPERATOR	4	>= (inet, inet) ,
	OPERATOR	5	>  (inet, inet) ,
	OPERATOR	6	<> (inet, inet) ,
	FUNCTION	1	gbt_inet_consistent (internal, inet, int2, oid, internal),
	FUNCTION	2	gbt_inet_union (internal, internal),
	FUNCTION	3	gbt_inet_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_inet_penalty (internal, internal, internal),
	FUNCTION	6	gbt_inet_picksplit (internal, internal),
	FUNCTION	7	gbt_inet_same (gbtreekey16, gbtreekey16, internal),
	-- no fetch support, the compress function is lossy
	FUNCTION	11	gbt_inet_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- uuid ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_uuid_consistent(internal,uuid,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_union(internal, internal)
RETURNS gbtreekey32
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_same(gbtreekey32, gbtreekey32, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_uuid_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_uuid_ops
DEFAULT FOR TYPE uuid USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_uuid_consistent (internal, uuid, int2, oid, internal),
	FUNCTION	2	gbt_uuid_union (internal, internal),
	FUNCTION	3	gbt_uuid_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_uuid_penalty (internal, internal, internal),
	FUNCTION	6	gbt_uuid_picksplit (internal, internal),
	FUNCTION	7	gbt_uuid_same (gbtreekey32, gbtreekey32, internal),
	FUNCTION	9	gbt_uuid_fetch (internal),
	FUNCTION	11	gbt_uuid_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey32;


--
--
--
-- macaddr8 ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_macad8_consistent(internal,macaddr8,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_union(internal, internal)
RETURNS gbtreekey16
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_same(gbtreekey16, gbtreekey16, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_macad8_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_macaddr8_ops
DEFAULT FOR TYPE macaddr8 USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_macad8_consistent (internal, macaddr8, int2, oid, internal),
	FUNCTION	2	gbt_macad8_union (internal, internal),
	FUNCTION	3	gbt_macad8_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_macad8_penalty (internal, internal, internal),
	FUNCTION	6	gbt_macad8_picksplit (internal, internal),
	FUNCTION	7	gbt_macad8_same (gbtreekey16, gbtreekey16, internal),
	FUNCTION	9	gbt_macad8_fetch (internal),
	FUNCTION	11	gbt_macad8_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey16;


--
--
--
-- enum ops
--
--
--
-- define the GiST support methods
CREATE FUNCTION gbt_enum_consistent(internal,anyenum,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_union(internal, internal)
RETURNS gbtreekey8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_same(gbtreekey8, gbtreekey8, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_enum_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_enum_ops
DEFAULT FOR TYPE anyenum USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_enum_consistent (internal, anyenum, int2, oid, internal),
	FUNCTION	2	gbt_enum_union (internal, internal),
	FUNCTION	3	gbt_enum_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_enum_penalty (internal, internal, internal),
	FUNCTION	6	gbt_enum_picksplit (internal, internal),
	FUNCTION	7	gbt_enum_same (gbtreekey8, gbtreekey8, internal),
	FUNCTION	9	gbt_enum_fetch (internal),
	FUNCTION	11	gbt_enum_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey8;


--
--
--
-- bool ops
--
--
--
-- Define the GiST support methods
CREATE FUNCTION gbt_bool_consistent(internal,bool,int2,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_fetch(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_union(internal, internal)
RETURNS gbtreekey2
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_same(gbtreekey2, gbtreekey2, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gbt_bool_sortsupport(internal)
RETURNS void
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- Create the operator class
CREATE OPERATOR CLASS gist_bool_ops
DEFAULT FOR TYPE bool USING gist
AS
	OPERATOR	1	<  ,
	OPERATOR	2	<= ,
	OPERATOR	3	=  ,
	OPERATOR	4	>= ,
	OPERATOR	5	>  ,
	OPERATOR	6	<> ,
	FUNCTION	1	gbt_bool_consistent (internal, bool, int2, oid, internal),
	FUNCTION	2	gbt_bool_union (internal, internal),
	FUNCTION	3	gbt_bool_compress (internal),
	FUNCTION	4	gbt_decompress (internal),
	FUNCTION	5	gbt_bool_penalty (internal, internal, internal),
	FUNCTION	6	gbt_bool_picksplit (internal, internal),
	FUNCTION	7	gbt_bool_same (gbtreekey2, gbtreekey2, internal),
	FUNCTION	9	gbt_bool_fetch (internal),
	FUNCTION	11	gbt_bool_sortsupport (internal),
	FUNCTION	12 ("any", "any") gist_translate_cmptype_btree (int),
	STORAGE		gbtreekey2;
"#,
    ),
    (
        "btree_gist.control",
        r#"# btree_gist extension
comment = 'support for indexing common datatypes in GiST'
default_version = '1.9'
module_pathname = '$libdir/btree_gist'
relocatable = true
trusted = true
"#,
    ),
];

/// The files of `contrib/pg_trgm`.
pub(crate) const PG_TRGM: &[(&str, &str)] = &[
    (
        "pg_trgm--1.0--1.1.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.0--1.1.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.1'" to load this file. \quit

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        5       pg_catalog.~ (text, text),
        OPERATOR        6       pg_catalog.~* (text, text);

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        5       pg_catalog.~ (text, text),
        OPERATOR        6       pg_catalog.~* (text, text);
"#,
    ),
    (
        "pg_trgm--1.1--1.2.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.1--1.2.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.2'" to load this file. \quit

CREATE FUNCTION word_similarity(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION word_similarity_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE FUNCTION word_similarity_commutator_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE FUNCTION word_similarity_dist_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE FUNCTION word_similarity_dist_commutator_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE;

CREATE OPERATOR <% (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_op,
        COMMUTATOR = '%>',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE OPERATOR %> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_commutator_op,
        COMMUTATOR = '<%',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE OPERATOR <<-> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_dist_op,
        COMMUTATOR = '<->>'
);

CREATE OPERATOR <->> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_dist_commutator_op,
        COMMUTATOR = '<<->'
);

CREATE FUNCTION gin_trgm_triconsistent(internal, int2, text, int4, internal, internal, internal)
RETURNS "char"
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT;

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        7       %> (text, text),
        OPERATOR        8       <->> (text, text) FOR ORDER BY pg_catalog.float_ops;

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        7       %> (text, text),
        FUNCTION        6      (text, text)   gin_trgm_triconsistent (internal, int2, text, int4, internal, internal, internal);
"#,
    ),
    (
        "pg_trgm--1.2--1.3.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.2--1.3.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.3'" to load this file. \quit

-- Update procedure signatures the hard way.
-- We use to_regprocedure() so that query doesn't fail if run against 9.6beta1 definitions,
-- wherein the signatures have been updated already.  In that case to_regprocedure() will
-- return NULL and no updates will happen.
DO LANGUAGE plpgsql
$$
DECLARE
  my_schema pg_catalog.text := pg_catalog.quote_ident(pg_catalog.current_schema());
  old_path pg_catalog.text := pg_catalog.current_setting('search_path');
BEGIN
-- for safety, transiently set search_path to just pg_catalog+pg_temp
PERFORM pg_catalog.set_config('search_path', 'pg_catalog, pg_temp', true);

UPDATE pg_catalog.pg_proc SET
  proargtypes = pg_catalog.array_to_string(newtypes::pg_catalog.oid[], ' ')::pg_catalog.oidvector,
  pronargs = pg_catalog.array_length(newtypes, 1)
FROM (VALUES
(NULL::pg_catalog.text, NULL::pg_catalog.text[]), -- establish column types
('gtrgm_consistent(internal,text,int4,oid,internal)', '{internal,text,int2,oid,internal}'),
('gtrgm_distance(internal,text,int4,oid)', '{internal,text,int2,oid,internal}'),
('gtrgm_union(bytea,internal)', '{internal,internal}')
) AS update_data (oldproc, newtypestext),
LATERAL (
  SELECT array_agg(replace(typ, 'SCH', my_schema)::regtype) as newtypes FROM unnest(newtypestext) typ
) ls
WHERE oid = to_regprocedure(my_schema || '.' || replace(oldproc, 'SCH', my_schema));

UPDATE pg_catalog.pg_proc SET
  prorettype = (my_schema || '.gtrgm')::pg_catalog.regtype
WHERE oid = pg_catalog.to_regprocedure(my_schema || '.gtrgm_union(internal,internal)');

PERFORM pg_catalog.set_config('search_path', old_path, true);
END
$$;

ALTER FUNCTION set_limit(float4) PARALLEL UNSAFE;
ALTER FUNCTION show_limit() PARALLEL SAFE;
ALTER FUNCTION show_trgm(text) PARALLEL SAFE;
ALTER FUNCTION similarity(text, text) PARALLEL SAFE;
ALTER FUNCTION similarity_op(text, text) PARALLEL SAFE;
ALTER FUNCTION word_similarity(text, text) PARALLEL SAFE;
ALTER FUNCTION word_similarity_op(text, text) PARALLEL SAFE;
ALTER FUNCTION word_similarity_commutator_op(text, text) PARALLEL SAFE;
ALTER FUNCTION similarity_dist(text, text) PARALLEL SAFE;
ALTER FUNCTION word_similarity_dist_op(text, text) PARALLEL SAFE;
ALTER FUNCTION word_similarity_dist_commutator_op(text, text) PARALLEL SAFE;
ALTER FUNCTION gtrgm_in(cstring) PARALLEL SAFE;
ALTER FUNCTION gtrgm_out(gtrgm) PARALLEL SAFE;
ALTER FUNCTION gtrgm_consistent(internal, text, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_distance(internal, text, smallint, oid, internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_compress(internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_decompress(internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_penalty(internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_picksplit(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_union(internal, internal) PARALLEL SAFE;
ALTER FUNCTION gtrgm_same(gtrgm, gtrgm, internal) PARALLEL SAFE;
ALTER FUNCTION gin_extract_value_trgm(text, internal) PARALLEL SAFE;
ALTER FUNCTION gin_extract_query_trgm(text, internal, int2, internal, internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gin_trgm_consistent(internal, int2, text, int4, internal, internal, internal, internal) PARALLEL SAFE;
ALTER FUNCTION gin_trgm_triconsistent(internal, int2, text, int4, internal, internal, internal) PARALLEL SAFE;
"#,
    ),
    (
        "pg_trgm--1.3--1.4.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.3--1.4.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.4'" to load this file. \quit

CREATE FUNCTION strict_word_similarity(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION strict_word_similarity_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE FUNCTION strict_word_similarity_commutator_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE OPERATOR <<% (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = strict_word_similarity_op,
        COMMUTATOR = '%>>',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE OPERATOR %>> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = strict_word_similarity_commutator_op,
        COMMUTATOR = '<<%',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE FUNCTION strict_word_similarity_dist_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION strict_word_similarity_dist_commutator_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE OPERATOR <<<-> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = strict_word_similarity_dist_op,
        COMMUTATOR = '<->>>'
);

CREATE OPERATOR <->>> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = strict_word_similarity_dist_commutator_op,
        COMMUTATOR = '<<<->'
);

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        9       %>> (text, text),
        OPERATOR        10       <->>> (text, text) FOR ORDER BY pg_catalog.float_ops;

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        9       %>> (text, text);
"#,
    ),
    (
        "pg_trgm--1.3.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.3.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "CREATE EXTENSION pg_trgm" to load this file. \quit

-- Deprecated function
CREATE FUNCTION set_limit(float4)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT VOLATILE PARALLEL UNSAFE;

-- Deprecated function
CREATE FUNCTION show_limit()
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;

CREATE FUNCTION show_trgm(text)
RETURNS _text
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION similarity(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION similarity_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;  -- stable because depends on pg_trgm.similarity_threshold

CREATE OPERATOR % (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = similarity_op,
        COMMUTATOR = '%',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE FUNCTION word_similarity(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION word_similarity_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE FUNCTION word_similarity_commutator_op(text,text)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT STABLE PARALLEL SAFE;  -- stable because depends on pg_trgm.word_similarity_threshold

CREATE OPERATOR <% (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_op,
        COMMUTATOR = '%>',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE OPERATOR %> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_commutator_op,
        COMMUTATOR = '<%',
        RESTRICT = contsel,
        JOIN = contjoinsel
);

CREATE FUNCTION similarity_dist(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE OPERATOR <-> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = similarity_dist,
        COMMUTATOR = '<->'
);

CREATE FUNCTION word_similarity_dist_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION word_similarity_dist_commutator_op(text,text)
RETURNS float4
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE OPERATOR <<-> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_dist_op,
        COMMUTATOR = '<->>'
);

CREATE OPERATOR <->> (
        LEFTARG = text,
        RIGHTARG = text,
        PROCEDURE = word_similarity_dist_commutator_op,
        COMMUTATOR = '<<->'
);

-- gist key
CREATE FUNCTION gtrgm_in(cstring)
RETURNS gtrgm
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE FUNCTION gtrgm_out(gtrgm)
RETURNS cstring
AS 'MODULE_PATHNAME'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

CREATE TYPE gtrgm (
        INTERNALLENGTH = -1,
        INPUT = gtrgm_in,
        OUTPUT = gtrgm_out
);

-- support functions for gist
CREATE FUNCTION gtrgm_consistent(internal,text,smallint,oid,internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_distance(internal,text,smallint,oid,internal)
RETURNS float8
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_compress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_decompress(internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_penalty(internal,internal,internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_picksplit(internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_union(internal, internal)
RETURNS gtrgm
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gtrgm_same(gtrgm, gtrgm, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- create the operator class for gist
CREATE OPERATOR CLASS gist_trgm_ops
FOR TYPE text USING gist
AS
        OPERATOR        1       % (text, text),
        FUNCTION        1       gtrgm_consistent (internal, text, smallint, oid, internal),
        FUNCTION        2       gtrgm_union (internal, internal),
        FUNCTION        3       gtrgm_compress (internal),
        FUNCTION        4       gtrgm_decompress (internal),
        FUNCTION        5       gtrgm_penalty (internal, internal, internal),
        FUNCTION        6       gtrgm_picksplit (internal, internal),
        FUNCTION        7       gtrgm_same (gtrgm, gtrgm, internal),
        STORAGE         gtrgm;

-- Add operators and support functions that are new in 9.1.  We do it like
-- this, leaving them "loose" in the operator family rather than bound into
-- the gist_trgm_ops opclass, because that's the only state that could be
-- reproduced during an upgrade from 9.0.

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        2       <-> (text, text) FOR ORDER BY pg_catalog.float_ops,
        OPERATOR        3       pg_catalog.~~ (text, text),
        OPERATOR        4       pg_catalog.~~* (text, text),
        FUNCTION        8 (text, text)  gtrgm_distance (internal, text, smallint, oid, internal);

-- Add operators that are new in 9.3.

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        5       pg_catalog.~ (text, text),
        OPERATOR        6       pg_catalog.~* (text, text);

-- Add operators that are new in 9.6 (pg_trgm 1.2).

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        7       %> (text, text),
        OPERATOR        8       <->> (text, text) FOR ORDER BY pg_catalog.float_ops;

-- support functions for gin
CREATE FUNCTION gin_extract_value_trgm(text, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gin_extract_query_trgm(text, internal, int2, internal, internal, internal, internal)
RETURNS internal
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE FUNCTION gin_trgm_consistent(internal, int2, text, int4, internal, internal, internal, internal)
RETURNS bool
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- create the operator class for gin
CREATE OPERATOR CLASS gin_trgm_ops
FOR TYPE text USING gin
AS
        OPERATOR        1       % (text, text),
        FUNCTION        1       btint4cmp (int4, int4),
        FUNCTION        2       gin_extract_value_trgm (text, internal),
        FUNCTION        3       gin_extract_query_trgm (text, internal, int2, internal, internal, internal, internal),
        FUNCTION        4       gin_trgm_consistent (internal, int2, text, int4, internal, internal, internal, internal),
        STORAGE         int4;

-- Add operators that are new in 9.1.

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        3       pg_catalog.~~ (text, text),
        OPERATOR        4       pg_catalog.~~* (text, text);

-- Add operators that are new in 9.3.

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        5       pg_catalog.~ (text, text),
        OPERATOR        6       pg_catalog.~* (text, text);

-- Add functions that are new in 9.6 (pg_trgm 1.2).

CREATE FUNCTION gin_trgm_triconsistent(internal, int2, text, int4, internal, internal, internal)
RETURNS "char"
AS 'MODULE_PATHNAME'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        7       %> (text, text),
        FUNCTION        6      (text,text) gin_trgm_triconsistent (internal, int2, text, int4, internal, internal, internal);
"#,
    ),
    (
        "pg_trgm--1.4--1.5.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.4--1.5.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.5'" to load this file. \quit

CREATE FUNCTION gtrgm_options(internal)
RETURNS void
AS 'MODULE_PATHNAME', 'gtrgm_options'
LANGUAGE C IMMUTABLE PARALLEL SAFE;

ALTER OPERATOR FAMILY gist_trgm_ops USING gist
ADD FUNCTION 10 (text) gtrgm_options (internal);

ALTER OPERATOR % (text, text)
  SET (RESTRICT = matchingsel, JOIN = matchingjoinsel);
ALTER OPERATOR <% (text, text)
  SET (RESTRICT = matchingsel, JOIN = matchingjoinsel);
ALTER OPERATOR %> (text, text)
  SET (RESTRICT = matchingsel, JOIN = matchingjoinsel);
ALTER OPERATOR <<% (text, text)
  SET (RESTRICT = matchingsel, JOIN = matchingjoinsel);
ALTER OPERATOR %>> (text, text)
  SET (RESTRICT = matchingsel, JOIN = matchingjoinsel);
"#,
    ),
    (
        "pg_trgm--1.5--1.6.sql",
        r#"/* contrib/pg_trgm/pg_trgm--1.5--1.6.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION
\echo Use "ALTER EXTENSION pg_trgm UPDATE TO '1.6'" to load this file. \quit

ALTER OPERATOR FAMILY gin_trgm_ops USING gin ADD
        OPERATOR        11       pg_catalog.= (text, text);

ALTER OPERATOR FAMILY gist_trgm_ops USING gist ADD
        OPERATOR        11       pg_catalog.= (text, text);
"#,
    ),
    (
        "pg_trgm.control",
        r#"# pg_trgm extension
comment = 'text similarity measurement and index searching based on trigrams'
default_version = '1.6'
module_pathname = '$libdir/pg_trgm'
relocatable = true
trusted = true
"#,
    ),
];

/// The files of the extension `vector`, from `vendor/pgvector`.
pub(crate) const VECTOR: &[(&str, &str)] = &[
    (
        "vector.control",
        r#"comment = 'vector data type and ivfflat and hnsw access methods'
default_version = '0.8.7'
module_pathname = '$libdir/vector'
relocatable = true
"#,
    ),
    (
        "vector--0.8.7.sql",
        r#"-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "CREATE EXTENSION vector" to load this file. \quit

-- vector type

CREATE TYPE vector;

CREATE FUNCTION vector_in(cstring, oid, integer) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_in(cstring, oid, integer) IS 'I/O';

CREATE FUNCTION vector_out(vector) RETURNS cstring
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_out(vector) IS 'I/O';

CREATE FUNCTION vector_typmod_in(cstring[]) RETURNS integer
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_typmod_in(cstring[]) IS 'I/O typmod';

CREATE FUNCTION vector_recv(internal, oid, integer) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_recv(internal, oid, integer) IS 'I/O';

CREATE FUNCTION vector_send(vector) RETURNS bytea
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_send(vector) IS 'I/O';

CREATE TYPE vector (
	INPUT     = vector_in,
	OUTPUT    = vector_out,
	TYPMOD_IN = vector_typmod_in,
	RECEIVE   = vector_recv,
	SEND      = vector_send,
	STORAGE   = external
);

COMMENT ON TYPE vector IS 'single-precision vector';

-- vector functions

CREATE FUNCTION l2_distance(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_distance(vector, vector) IS 'Euclidean distance';

CREATE FUNCTION inner_product(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION inner_product(vector, vector) IS 'inner product';

CREATE FUNCTION cosine_distance(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION cosine_distance(vector, vector) IS 'cosine distance';

CREATE FUNCTION l1_distance(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l1_distance(vector, vector) IS 'taxicab distance';

CREATE FUNCTION vector_dims(vector) RETURNS integer
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_dims(vector) IS 'number of dimensions';

CREATE FUNCTION vector_norm(vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_norm(vector) IS 'Euclidean norm';

CREATE FUNCTION l2_normalize(vector) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_normalize(vector) IS 'normalize with Euclidean norm';

CREATE FUNCTION binary_quantize(vector) RETURNS bit
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION binary_quantize(vector) IS 'binary quantize';

CREATE FUNCTION subvector(vector, int, int) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION subvector(vector, int, int) IS 'subvector';

-- vector private functions

CREATE FUNCTION vector_add(vector, vector) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_add(vector, vector) IS 'implementation of + operator';

CREATE FUNCTION vector_sub(vector, vector) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_sub(vector, vector) IS 'implementation of - operator';

CREATE FUNCTION vector_mul(vector, vector) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_mul(vector, vector) IS 'implementation of * operator';

CREATE FUNCTION vector_concat(vector, vector) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_concat(vector, vector) IS 'implementation of || operator';

CREATE FUNCTION vector_lt(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_lt(vector, vector) IS 'implementation of < operator';

CREATE FUNCTION vector_le(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_le(vector, vector) IS 'implementation of <= operator';

CREATE FUNCTION vector_eq(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_eq(vector, vector) IS 'implementation of = operator';

CREATE FUNCTION vector_ne(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_ne(vector, vector) IS 'implementation of <> operator';

CREATE FUNCTION vector_ge(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_ge(vector, vector) IS 'implementation of >= operator';

CREATE FUNCTION vector_gt(vector, vector) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_gt(vector, vector) IS 'implementation of > operator';

CREATE FUNCTION vector_cmp(vector, vector) RETURNS int4
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_cmp(vector, vector) IS 'less-equal-greater';

CREATE FUNCTION vector_l2_squared_distance(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_l2_squared_distance(vector, vector) IS 'squared Euclidean distance';

CREATE FUNCTION vector_negative_inner_product(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_negative_inner_product(vector, vector) IS 'negative inner product';

CREATE FUNCTION vector_spherical_distance(vector, vector) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_spherical_distance(vector, vector) IS 'spherical distance';

CREATE FUNCTION vector_accum(double precision[], vector) RETURNS double precision[]
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_accum(double precision[], vector) IS 'aggregate transition function';

CREATE FUNCTION vector_avg(double precision[]) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_avg(double precision[]) IS 'aggregate final function';

CREATE FUNCTION vector_combine(double precision[], double precision[]) RETURNS double precision[]
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_combine(double precision[], double precision[]) IS 'aggregate combine function';

-- vector aggregates

CREATE AGGREGATE avg(vector) (
	SFUNC = vector_accum,
	STYPE = double precision[],
	FINALFUNC = vector_avg,
	COMBINEFUNC = vector_combine,
	INITCOND = '{0}',
	PARALLEL = SAFE
);

COMMENT ON AGGREGATE avg(vector) IS 'the average (arithmetic mean) as vector of all vector values';

CREATE AGGREGATE sum(vector) (
	SFUNC = vector_add,
	STYPE = vector,
	COMBINEFUNC = vector_add,
	PARALLEL = SAFE
);

COMMENT ON AGGREGATE sum(vector) IS 'sum as vector across all vector input values';

-- vector cast functions

CREATE FUNCTION vector(vector, integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector(vector, integer, boolean) IS 'adjust vector to typmod';

CREATE FUNCTION array_to_vector(integer[], integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_vector(integer[], integer, boolean) IS 'convert int4 array to vector';

CREATE FUNCTION array_to_vector(real[], integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_vector(real[], integer, boolean) IS 'convert float4 array to vector';

CREATE FUNCTION array_to_vector(double precision[], integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_vector(double precision[], integer, boolean) IS 'convert float8 array to vector';

CREATE FUNCTION array_to_vector(numeric[], integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_vector(numeric[], integer, boolean) IS 'convert numeric array to vector';

CREATE FUNCTION vector_to_float4(vector, integer, boolean) RETURNS real[]
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_to_float4(vector, integer, boolean) IS 'convert vector to float4 array';

-- vector casts

CREATE CAST (vector AS vector)
	WITH FUNCTION vector(vector, integer, boolean) AS IMPLICIT;

CREATE CAST (vector AS real[])
	WITH FUNCTION vector_to_float4(vector, integer, boolean) AS IMPLICIT;

CREATE CAST (integer[] AS vector)
	WITH FUNCTION array_to_vector(integer[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (real[] AS vector)
	WITH FUNCTION array_to_vector(real[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (double precision[] AS vector)
	WITH FUNCTION array_to_vector(double precision[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (numeric[] AS vector)
	WITH FUNCTION array_to_vector(numeric[], integer, boolean) AS ASSIGNMENT;

-- vector operators

CREATE OPERATOR <-> (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = l2_distance,
	COMMUTATOR = '<->'
);

CREATE OPERATOR <#> (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_negative_inner_product,
	COMMUTATOR = '<#>'
);

CREATE OPERATOR <=> (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = cosine_distance,
	COMMUTATOR = '<=>'
);

CREATE OPERATOR <+> (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = l1_distance,
	COMMUTATOR = '<+>'
);

CREATE OPERATOR + (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_add,
	COMMUTATOR = +
);

COMMENT ON OPERATOR + (vector, vector) IS 'add';

CREATE OPERATOR - (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_sub
);

COMMENT ON OPERATOR - (vector, vector) IS 'subtract';

CREATE OPERATOR * (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_mul,
	COMMUTATOR = *
);

COMMENT ON OPERATOR * (vector, vector) IS 'multiply';

CREATE OPERATOR || (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_concat
);

COMMENT ON OPERATOR || (vector, vector) IS 'concatenate';

CREATE OPERATOR < (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_lt,
	COMMUTATOR = > , NEGATOR = >= ,
	RESTRICT = scalarltsel, JOIN = scalarltjoinsel
);

COMMENT ON OPERATOR < (vector, vector) IS 'less than';

CREATE OPERATOR <= (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_le,
	COMMUTATOR = >= , NEGATOR = > ,
	RESTRICT = scalarlesel, JOIN = scalarlejoinsel
);

COMMENT ON OPERATOR <= (vector, vector) IS 'less than or equal';

CREATE OPERATOR = (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_eq,
	COMMUTATOR = = , NEGATOR = <> ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR = (vector, vector) IS 'equal';

CREATE OPERATOR <> (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_ne,
	COMMUTATOR = <> , NEGATOR = = ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR <> (vector, vector) IS 'not equal';

CREATE OPERATOR >= (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_ge,
	COMMUTATOR = <= , NEGATOR = < ,
	RESTRICT = scalargesel, JOIN = scalargejoinsel
);

COMMENT ON OPERATOR >= (vector, vector) IS 'greater than or equal';

CREATE OPERATOR > (
	LEFTARG = vector, RIGHTARG = vector, PROCEDURE = vector_gt,
	COMMUTATOR = < , NEGATOR = <= ,
	RESTRICT = scalargtsel, JOIN = scalargtjoinsel
);

COMMENT ON OPERATOR > (vector, vector) IS 'greater than';

-- access methods

CREATE FUNCTION ivfflathandler(internal) RETURNS index_am_handler
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION ivfflathandler(internal) IS 'ivfflat index access method handler';

CREATE ACCESS METHOD ivfflat TYPE INDEX HANDLER ivfflathandler;

COMMENT ON ACCESS METHOD ivfflat IS 'ivfflat index access method';

CREATE FUNCTION hnswhandler(internal) RETURNS index_am_handler
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION hnswhandler(internal) IS 'hnsw index access method handler';

CREATE ACCESS METHOD hnsw TYPE INDEX HANDLER hnswhandler;

COMMENT ON ACCESS METHOD hnsw IS 'hnsw index access method';

-- access method private functions

CREATE FUNCTION ivfflat_halfvec_support(internal) RETURNS internal
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION ivfflat_halfvec_support(internal) IS 'ivfflat halfvec support';

CREATE FUNCTION ivfflat_bit_support(internal) RETURNS internal
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION ivfflat_bit_support(internal) IS 'ivfflat bit support';

CREATE FUNCTION hnsw_halfvec_support(internal) RETURNS internal
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION hnsw_halfvec_support(internal) IS 'hnsw halfvec support';

CREATE FUNCTION hnsw_bit_support(internal) RETURNS internal
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION hnsw_bit_support(internal) IS 'hnsw bit support';

CREATE FUNCTION hnsw_sparsevec_support(internal) RETURNS internal
	AS 'MODULE_PATHNAME' LANGUAGE C;

COMMENT ON FUNCTION hnsw_sparsevec_support(internal) IS 'hnsw sparsevec support';

-- vector opclasses

CREATE OPERATOR CLASS vector_ops
	DEFAULT FOR TYPE vector USING btree AS
	OPERATOR 1 < ,
	OPERATOR 2 <= ,
	OPERATOR 3 = ,
	OPERATOR 4 >= ,
	OPERATOR 5 > ,
	FUNCTION 1 vector_cmp(vector, vector);

CREATE OPERATOR CLASS vector_l2_ops
	DEFAULT FOR TYPE vector USING ivfflat AS
	OPERATOR 1 <-> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_l2_squared_distance(vector, vector),
	FUNCTION 3 l2_distance(vector, vector);

CREATE OPERATOR CLASS vector_ip_ops
	FOR TYPE vector USING ivfflat AS
	OPERATOR 1 <#> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_negative_inner_product(vector, vector),
	FUNCTION 3 vector_spherical_distance(vector, vector),
	FUNCTION 4 vector_norm(vector);

CREATE OPERATOR CLASS vector_cosine_ops
	FOR TYPE vector USING ivfflat AS
	OPERATOR 1 <=> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_negative_inner_product(vector, vector),
	FUNCTION 2 vector_norm(vector),
	FUNCTION 3 vector_spherical_distance(vector, vector),
	FUNCTION 4 vector_norm(vector);

CREATE OPERATOR CLASS vector_l2_ops
	FOR TYPE vector USING hnsw AS
	OPERATOR 1 <-> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_l2_squared_distance(vector, vector);

CREATE OPERATOR CLASS vector_ip_ops
	FOR TYPE vector USING hnsw AS
	OPERATOR 1 <#> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_negative_inner_product(vector, vector);

CREATE OPERATOR CLASS vector_cosine_ops
	FOR TYPE vector USING hnsw AS
	OPERATOR 1 <=> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 vector_negative_inner_product(vector, vector),
	FUNCTION 2 vector_norm(vector);

CREATE OPERATOR CLASS vector_l1_ops
	FOR TYPE vector USING hnsw AS
	OPERATOR 1 <+> (vector, vector) FOR ORDER BY float_ops,
	FUNCTION 1 l1_distance(vector, vector);

-- halfvec type

CREATE TYPE halfvec;

CREATE FUNCTION halfvec_in(cstring, oid, integer) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_in(cstring, oid, integer) IS 'I/O';

CREATE FUNCTION halfvec_out(halfvec) RETURNS cstring
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_out(halfvec) IS 'I/O';

CREATE FUNCTION halfvec_typmod_in(cstring[]) RETURNS integer
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_typmod_in(cstring[]) IS 'I/O typmod';

CREATE FUNCTION halfvec_recv(internal, oid, integer) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_recv(internal, oid, integer) IS 'I/O';

CREATE FUNCTION halfvec_send(halfvec) RETURNS bytea
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_send(halfvec) IS 'I/O';

CREATE TYPE halfvec (
	INPUT     = halfvec_in,
	OUTPUT    = halfvec_out,
	TYPMOD_IN = halfvec_typmod_in,
	RECEIVE   = halfvec_recv,
	SEND      = halfvec_send,
	STORAGE   = external
);

COMMENT ON TYPE halfvec IS 'half-precision vector';

-- halfvec functions

CREATE FUNCTION l2_distance(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME', 'halfvec_l2_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_distance(halfvec, halfvec) IS 'Euclidean distance';

CREATE FUNCTION inner_product(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME', 'halfvec_inner_product' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION inner_product(halfvec, halfvec) IS 'inner product';

CREATE FUNCTION cosine_distance(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME', 'halfvec_cosine_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION cosine_distance(halfvec, halfvec) IS 'cosine distance';

CREATE FUNCTION l1_distance(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME', 'halfvec_l1_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l1_distance(halfvec, halfvec) IS 'taxicab distance';

CREATE FUNCTION vector_dims(halfvec) RETURNS integer
	AS 'MODULE_PATHNAME', 'halfvec_vector_dims' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_dims(halfvec) IS 'number of dimensions';

CREATE FUNCTION l2_norm(halfvec) RETURNS float8
	AS 'MODULE_PATHNAME', 'halfvec_l2_norm' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_norm(halfvec) IS 'Euclidean norm';

CREATE FUNCTION l2_normalize(halfvec) RETURNS halfvec
	AS 'MODULE_PATHNAME', 'halfvec_l2_normalize' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_normalize(halfvec) IS 'normalize with Euclidean norm';

CREATE FUNCTION binary_quantize(halfvec) RETURNS bit
	AS 'MODULE_PATHNAME', 'halfvec_binary_quantize' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION binary_quantize(halfvec) IS 'binary quantize';

CREATE FUNCTION subvector(halfvec, int, int) RETURNS halfvec
	AS 'MODULE_PATHNAME', 'halfvec_subvector' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION subvector(halfvec, int, int) IS 'subvector';

-- halfvec private functions

CREATE FUNCTION halfvec_add(halfvec, halfvec) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_add(halfvec, halfvec) IS 'implementation of + operator';

CREATE FUNCTION halfvec_sub(halfvec, halfvec) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_sub(halfvec, halfvec) IS 'implementation of - operator';

CREATE FUNCTION halfvec_mul(halfvec, halfvec) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_mul(halfvec, halfvec) IS 'implementation of * operator';

CREATE FUNCTION halfvec_concat(halfvec, halfvec) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_concat(halfvec, halfvec) IS 'implementation of || operator';

CREATE FUNCTION halfvec_lt(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_lt(halfvec, halfvec) IS 'implementation of < operator';

CREATE FUNCTION halfvec_le(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_le(halfvec, halfvec) IS 'implementation of <= operator';

CREATE FUNCTION halfvec_eq(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_eq(halfvec, halfvec) IS 'implementation of = operator';

CREATE FUNCTION halfvec_ne(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_ne(halfvec, halfvec) IS 'implementation of <> operator';

CREATE FUNCTION halfvec_ge(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_ge(halfvec, halfvec) IS 'implementation of >= operator';

CREATE FUNCTION halfvec_gt(halfvec, halfvec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_gt(halfvec, halfvec) IS 'implementation of > operator';

CREATE FUNCTION halfvec_cmp(halfvec, halfvec) RETURNS int4
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_cmp(halfvec, halfvec) IS 'less-equal-greater';

CREATE FUNCTION halfvec_l2_squared_distance(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_l2_squared_distance(halfvec, halfvec) IS 'squared Euclidean distance';

CREATE FUNCTION halfvec_negative_inner_product(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_negative_inner_product(halfvec, halfvec) IS 'negative inner product';

CREATE FUNCTION halfvec_spherical_distance(halfvec, halfvec) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_spherical_distance(halfvec, halfvec) IS 'spherical distance';

CREATE FUNCTION halfvec_accum(double precision[], halfvec) RETURNS double precision[]
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_accum(double precision[], halfvec) IS 'aggregate transition function';

CREATE FUNCTION halfvec_avg(double precision[]) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_avg(double precision[]) IS 'aggregate final function';

CREATE FUNCTION halfvec_combine(double precision[], double precision[]) RETURNS double precision[]
	AS 'MODULE_PATHNAME', 'vector_combine' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_combine(double precision[], double precision[]) IS 'aggregate combine function';

-- halfvec aggregates

CREATE AGGREGATE avg(halfvec) (
	SFUNC = halfvec_accum,
	STYPE = double precision[],
	FINALFUNC = halfvec_avg,
	COMBINEFUNC = halfvec_combine,
	INITCOND = '{0}',
	PARALLEL = SAFE
);

COMMENT ON AGGREGATE avg(halfvec) IS 'the average (arithmetic mean) as halfvec of all halfvec values';

CREATE AGGREGATE sum(halfvec) (
	SFUNC = halfvec_add,
	STYPE = halfvec,
	COMBINEFUNC = halfvec_add,
	PARALLEL = SAFE
);

COMMENT ON AGGREGATE sum(halfvec) IS 'sum as halfvec across all halfvec input values';

-- halfvec cast functions

CREATE FUNCTION halfvec(halfvec, integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec(halfvec, integer, boolean) IS 'adjust halfvec to typmod';

CREATE FUNCTION halfvec_to_vector(halfvec, integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_to_vector(halfvec, integer, boolean) IS 'convert halfvec to vector';

CREATE FUNCTION vector_to_halfvec(vector, integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_to_halfvec(vector, integer, boolean) IS 'convert vector to halfvec';

CREATE FUNCTION array_to_halfvec(integer[], integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_halfvec(integer[], integer, boolean) IS 'convert int4 array to halfvec';

CREATE FUNCTION array_to_halfvec(real[], integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_halfvec(real[], integer, boolean) IS 'convert float4 array to halfvec';

CREATE FUNCTION array_to_halfvec(double precision[], integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_halfvec(double precision[], integer, boolean) IS 'convert float8 array to halfvec';

CREATE FUNCTION array_to_halfvec(numeric[], integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_halfvec(numeric[], integer, boolean) IS 'convert numeric array to halfvec';

CREATE FUNCTION halfvec_to_float4(halfvec, integer, boolean) RETURNS real[]
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_to_float4(halfvec, integer, boolean) IS 'convert halfvec to float4 array';

-- halfvec casts

CREATE CAST (halfvec AS halfvec)
	WITH FUNCTION halfvec(halfvec, integer, boolean) AS IMPLICIT;

CREATE CAST (halfvec AS vector)
	WITH FUNCTION halfvec_to_vector(halfvec, integer, boolean) AS ASSIGNMENT;

CREATE CAST (vector AS halfvec)
	WITH FUNCTION vector_to_halfvec(vector, integer, boolean) AS IMPLICIT;

CREATE CAST (halfvec AS real[])
	WITH FUNCTION halfvec_to_float4(halfvec, integer, boolean) AS ASSIGNMENT;

CREATE CAST (integer[] AS halfvec)
	WITH FUNCTION array_to_halfvec(integer[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (real[] AS halfvec)
	WITH FUNCTION array_to_halfvec(real[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (double precision[] AS halfvec)
	WITH FUNCTION array_to_halfvec(double precision[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (numeric[] AS halfvec)
	WITH FUNCTION array_to_halfvec(numeric[], integer, boolean) AS ASSIGNMENT;

-- halfvec operators

CREATE OPERATOR <-> (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = l2_distance,
	COMMUTATOR = '<->'
);

CREATE OPERATOR <#> (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_negative_inner_product,
	COMMUTATOR = '<#>'
);

CREATE OPERATOR <=> (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = cosine_distance,
	COMMUTATOR = '<=>'
);

CREATE OPERATOR <+> (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = l1_distance,
	COMMUTATOR = '<+>'
);

CREATE OPERATOR + (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_add,
	COMMUTATOR = +
);

COMMENT ON OPERATOR + (halfvec, halfvec) IS 'add';

CREATE OPERATOR - (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_sub
);

COMMENT ON OPERATOR - (halfvec, halfvec) IS 'subtract';

CREATE OPERATOR * (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_mul,
	COMMUTATOR = *
);

COMMENT ON OPERATOR * (halfvec, halfvec) IS 'multiply';

CREATE OPERATOR || (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_concat
);

COMMENT ON OPERATOR || (halfvec, halfvec) IS 'concatenate';

CREATE OPERATOR < (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_lt,
	COMMUTATOR = > , NEGATOR = >= ,
	RESTRICT = scalarltsel, JOIN = scalarltjoinsel
);

COMMENT ON OPERATOR < (halfvec, halfvec) IS 'less than';

CREATE OPERATOR <= (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_le,
	COMMUTATOR = >= , NEGATOR = > ,
	RESTRICT = scalarlesel, JOIN = scalarlejoinsel
);

COMMENT ON OPERATOR <= (halfvec, halfvec) IS 'less than or equal';

CREATE OPERATOR = (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_eq,
	COMMUTATOR = = , NEGATOR = <> ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR = (halfvec, halfvec) IS 'equal';

CREATE OPERATOR <> (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_ne,
	COMMUTATOR = <> , NEGATOR = = ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR <> (halfvec, halfvec) IS 'not equal';

CREATE OPERATOR >= (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_ge,
	COMMUTATOR = <= , NEGATOR = < ,
	RESTRICT = scalargesel, JOIN = scalargejoinsel
);

COMMENT ON OPERATOR >= (halfvec, halfvec) IS 'greater than or equal';

CREATE OPERATOR > (
	LEFTARG = halfvec, RIGHTARG = halfvec, PROCEDURE = halfvec_gt,
	COMMUTATOR = < , NEGATOR = <= ,
	RESTRICT = scalargtsel, JOIN = scalargtjoinsel
);

COMMENT ON OPERATOR > (halfvec, halfvec) IS 'greater than';

-- halfvec opclasses

CREATE OPERATOR CLASS halfvec_ops
	DEFAULT FOR TYPE halfvec USING btree AS
	OPERATOR 1 < ,
	OPERATOR 2 <= ,
	OPERATOR 3 = ,
	OPERATOR 4 >= ,
	OPERATOR 5 > ,
	FUNCTION 1 halfvec_cmp(halfvec, halfvec);

CREATE OPERATOR CLASS halfvec_l2_ops
	FOR TYPE halfvec USING ivfflat AS
	OPERATOR 1 <-> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_l2_squared_distance(halfvec, halfvec),
	FUNCTION 3 l2_distance(halfvec, halfvec),
	FUNCTION 5 ivfflat_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_ip_ops
	FOR TYPE halfvec USING ivfflat AS
	OPERATOR 1 <#> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_negative_inner_product(halfvec, halfvec),
	FUNCTION 3 halfvec_spherical_distance(halfvec, halfvec),
	FUNCTION 4 l2_norm(halfvec),
	FUNCTION 5 ivfflat_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_cosine_ops
	FOR TYPE halfvec USING ivfflat AS
	OPERATOR 1 <=> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_negative_inner_product(halfvec, halfvec),
	FUNCTION 2 l2_norm(halfvec),
	FUNCTION 3 halfvec_spherical_distance(halfvec, halfvec),
	FUNCTION 4 l2_norm(halfvec),
	FUNCTION 5 ivfflat_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_l2_ops
	FOR TYPE halfvec USING hnsw AS
	OPERATOR 1 <-> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_l2_squared_distance(halfvec, halfvec),
	FUNCTION 3 hnsw_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_ip_ops
	FOR TYPE halfvec USING hnsw AS
	OPERATOR 1 <#> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_negative_inner_product(halfvec, halfvec),
	FUNCTION 3 hnsw_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_cosine_ops
	FOR TYPE halfvec USING hnsw AS
	OPERATOR 1 <=> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 halfvec_negative_inner_product(halfvec, halfvec),
	FUNCTION 2 l2_norm(halfvec),
	FUNCTION 3 hnsw_halfvec_support(internal);

CREATE OPERATOR CLASS halfvec_l1_ops
	FOR TYPE halfvec USING hnsw AS
	OPERATOR 1 <+> (halfvec, halfvec) FOR ORDER BY float_ops,
	FUNCTION 1 l1_distance(halfvec, halfvec),
	FUNCTION 3 hnsw_halfvec_support(internal);

-- bit functions

CREATE FUNCTION hamming_distance(bit, bit) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION hamming_distance(bit, bit) IS 'Hamming distance';

CREATE FUNCTION jaccard_distance(bit, bit) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION jaccard_distance(bit, bit) IS 'Jaccard distance';

-- bit operators

CREATE OPERATOR <~> (
	LEFTARG = bit, RIGHTARG = bit, PROCEDURE = hamming_distance,
	COMMUTATOR = '<~>'
);

CREATE OPERATOR <%> (
	LEFTARG = bit, RIGHTARG = bit, PROCEDURE = jaccard_distance,
	COMMUTATOR = '<%>'
);

-- bit opclasses

CREATE OPERATOR CLASS bit_hamming_ops
	FOR TYPE bit USING ivfflat AS
	OPERATOR 1 <~> (bit, bit) FOR ORDER BY float_ops,
	FUNCTION 1 hamming_distance(bit, bit),
	FUNCTION 3 hamming_distance(bit, bit),
	FUNCTION 5 ivfflat_bit_support(internal);

CREATE OPERATOR CLASS bit_hamming_ops
	FOR TYPE bit USING hnsw AS
	OPERATOR 1 <~> (bit, bit) FOR ORDER BY float_ops,
	FUNCTION 1 hamming_distance(bit, bit),
	FUNCTION 3 hnsw_bit_support(internal);

CREATE OPERATOR CLASS bit_jaccard_ops
	FOR TYPE bit USING hnsw AS
	OPERATOR 1 <%> (bit, bit) FOR ORDER BY float_ops,
	FUNCTION 1 jaccard_distance(bit, bit),
	FUNCTION 3 hnsw_bit_support(internal);

-- sparsevec type

CREATE TYPE sparsevec;

CREATE FUNCTION sparsevec_in(cstring, oid, integer) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_in(cstring, oid, integer) IS 'I/O';

CREATE FUNCTION sparsevec_out(sparsevec) RETURNS cstring
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_out(sparsevec) IS 'I/O';

CREATE FUNCTION sparsevec_typmod_in(cstring[]) RETURNS integer
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_typmod_in(cstring[]) IS 'I/O typmod';

CREATE FUNCTION sparsevec_recv(internal, oid, integer) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_recv(internal, oid, integer) IS 'I/O';

CREATE FUNCTION sparsevec_send(sparsevec) RETURNS bytea
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_send(sparsevec) IS 'I/O';

CREATE TYPE sparsevec (
	INPUT     = sparsevec_in,
	OUTPUT    = sparsevec_out,
	TYPMOD_IN = sparsevec_typmod_in,
	RECEIVE   = sparsevec_recv,
	SEND      = sparsevec_send,
	STORAGE   = external
);

COMMENT ON TYPE sparsevec IS 'single-precision sparse vector';

-- sparsevec functions

CREATE FUNCTION l2_distance(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME', 'sparsevec_l2_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_distance(sparsevec, sparsevec) IS 'Euclidean distance';

CREATE FUNCTION inner_product(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME', 'sparsevec_inner_product' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION inner_product(sparsevec, sparsevec) IS 'inner product';

CREATE FUNCTION cosine_distance(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME', 'sparsevec_cosine_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION cosine_distance(sparsevec, sparsevec) IS 'cosine distance';

CREATE FUNCTION l1_distance(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME', 'sparsevec_l1_distance' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l1_distance(sparsevec, sparsevec) IS 'taxicab distance';

CREATE FUNCTION l2_norm(sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME', 'sparsevec_l2_norm' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_norm(sparsevec) IS 'Euclidean norm';

CREATE FUNCTION l2_normalize(sparsevec) RETURNS sparsevec
	AS 'MODULE_PATHNAME', 'sparsevec_l2_normalize' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION l2_normalize(sparsevec) IS 'normalize with Euclidean norm';

-- sparsevec private functions

CREATE FUNCTION sparsevec_lt(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_lt(sparsevec, sparsevec) IS 'implementation of < operator';

CREATE FUNCTION sparsevec_le(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_le(sparsevec, sparsevec) IS 'implementation of <= operator';

CREATE FUNCTION sparsevec_eq(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_eq(sparsevec, sparsevec) IS 'implementation of = operator';

CREATE FUNCTION sparsevec_ne(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_ne(sparsevec, sparsevec) IS 'implementation of <> operator';

CREATE FUNCTION sparsevec_ge(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_ge(sparsevec, sparsevec) IS 'implementation of >= operator';

CREATE FUNCTION sparsevec_gt(sparsevec, sparsevec) RETURNS bool
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_gt(sparsevec, sparsevec) IS 'implementation of > operator';

CREATE FUNCTION sparsevec_cmp(sparsevec, sparsevec) RETURNS int4
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_cmp(sparsevec, sparsevec) IS 'less-equal-greater';

CREATE FUNCTION sparsevec_l2_squared_distance(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_l2_squared_distance(sparsevec, sparsevec) IS 'squared Euclidean distance';

CREATE FUNCTION sparsevec_negative_inner_product(sparsevec, sparsevec) RETURNS float8
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_negative_inner_product(sparsevec, sparsevec) IS 'negative inner product';

-- sparsevec cast functions

CREATE FUNCTION sparsevec(sparsevec, integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec(sparsevec, integer, boolean) IS 'adjust sparsevec to typmod';

CREATE FUNCTION vector_to_sparsevec(vector, integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION vector_to_sparsevec(vector, integer, boolean) IS 'convert vector to sparsevec';

CREATE FUNCTION sparsevec_to_vector(sparsevec, integer, boolean) RETURNS vector
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_to_vector(sparsevec, integer, boolean) IS 'convert sparsevec to vector';

CREATE FUNCTION halfvec_to_sparsevec(halfvec, integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION halfvec_to_sparsevec(halfvec, integer, boolean) IS 'convert halfvec to sparsevec';

CREATE FUNCTION sparsevec_to_halfvec(sparsevec, integer, boolean) RETURNS halfvec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION sparsevec_to_halfvec(sparsevec, integer, boolean) IS 'convert sparsevec to halfvec';

CREATE FUNCTION array_to_sparsevec(integer[], integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_sparsevec(integer[], integer, boolean) IS 'convert int4 array to sparsevec';

CREATE FUNCTION array_to_sparsevec(real[], integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_sparsevec(real[], integer, boolean) IS 'convert float4 array to sparsevec';

CREATE FUNCTION array_to_sparsevec(double precision[], integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_sparsevec(double precision[], integer, boolean) IS 'convert float8 array to sparsevec';

CREATE FUNCTION array_to_sparsevec(numeric[], integer, boolean) RETURNS sparsevec
	AS 'MODULE_PATHNAME' LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

COMMENT ON FUNCTION array_to_sparsevec(numeric[], integer, boolean) IS 'convert numeric array to sparsevec';

-- sparsevec casts

CREATE CAST (sparsevec AS sparsevec)
	WITH FUNCTION sparsevec(sparsevec, integer, boolean) AS IMPLICIT;

CREATE CAST (sparsevec AS vector)
	WITH FUNCTION sparsevec_to_vector(sparsevec, integer, boolean) AS ASSIGNMENT;

CREATE CAST (vector AS sparsevec)
	WITH FUNCTION vector_to_sparsevec(vector, integer, boolean) AS IMPLICIT;

CREATE CAST (sparsevec AS halfvec)
	WITH FUNCTION sparsevec_to_halfvec(sparsevec, integer, boolean) AS ASSIGNMENT;

CREATE CAST (halfvec AS sparsevec)
	WITH FUNCTION halfvec_to_sparsevec(halfvec, integer, boolean) AS IMPLICIT;

CREATE CAST (integer[] AS sparsevec)
	WITH FUNCTION array_to_sparsevec(integer[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (real[] AS sparsevec)
	WITH FUNCTION array_to_sparsevec(real[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (double precision[] AS sparsevec)
	WITH FUNCTION array_to_sparsevec(double precision[], integer, boolean) AS ASSIGNMENT;

CREATE CAST (numeric[] AS sparsevec)
	WITH FUNCTION array_to_sparsevec(numeric[], integer, boolean) AS ASSIGNMENT;

-- sparsevec operators

CREATE OPERATOR <-> (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = l2_distance,
	COMMUTATOR = '<->'
);

CREATE OPERATOR <#> (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_negative_inner_product,
	COMMUTATOR = '<#>'
);

CREATE OPERATOR <=> (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = cosine_distance,
	COMMUTATOR = '<=>'
);

CREATE OPERATOR <+> (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = l1_distance,
	COMMUTATOR = '<+>'
);

CREATE OPERATOR < (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_lt,
	COMMUTATOR = > , NEGATOR = >= ,
	RESTRICT = scalarltsel, JOIN = scalarltjoinsel
);

COMMENT ON OPERATOR < (sparsevec, sparsevec) IS 'less than';

CREATE OPERATOR <= (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_le,
	COMMUTATOR = >= , NEGATOR = > ,
	RESTRICT = scalarlesel, JOIN = scalarlejoinsel
);

COMMENT ON OPERATOR <= (sparsevec, sparsevec) IS 'less than or equal';

CREATE OPERATOR = (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_eq,
	COMMUTATOR = = , NEGATOR = <> ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR = (sparsevec, sparsevec) IS 'equal';

CREATE OPERATOR <> (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_ne,
	COMMUTATOR = <> , NEGATOR = = ,
	RESTRICT = eqsel, JOIN = eqjoinsel
);

COMMENT ON OPERATOR <> (sparsevec, sparsevec) IS 'not equal';

CREATE OPERATOR >= (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_ge,
	COMMUTATOR = <= , NEGATOR = < ,
	RESTRICT = scalargesel, JOIN = scalargejoinsel
);

COMMENT ON OPERATOR >= (sparsevec, sparsevec) IS 'greater than or equal';

CREATE OPERATOR > (
	LEFTARG = sparsevec, RIGHTARG = sparsevec, PROCEDURE = sparsevec_gt,
	COMMUTATOR = < , NEGATOR = <= ,
	RESTRICT = scalargtsel, JOIN = scalargtjoinsel
);

COMMENT ON OPERATOR > (sparsevec, sparsevec) IS 'greater than';

-- sparsevec opclasses

CREATE OPERATOR CLASS sparsevec_ops
	DEFAULT FOR TYPE sparsevec USING btree AS
	OPERATOR 1 < ,
	OPERATOR 2 <= ,
	OPERATOR 3 = ,
	OPERATOR 4 >= ,
	OPERATOR 5 > ,
	FUNCTION 1 sparsevec_cmp(sparsevec, sparsevec);

CREATE OPERATOR CLASS sparsevec_l2_ops
	FOR TYPE sparsevec USING hnsw AS
	OPERATOR 1 <-> (sparsevec, sparsevec) FOR ORDER BY float_ops,
	FUNCTION 1 sparsevec_l2_squared_distance(sparsevec, sparsevec),
	FUNCTION 3 hnsw_sparsevec_support(internal);

CREATE OPERATOR CLASS sparsevec_ip_ops
	FOR TYPE sparsevec USING hnsw AS
	OPERATOR 1 <#> (sparsevec, sparsevec) FOR ORDER BY float_ops,
	FUNCTION 1 sparsevec_negative_inner_product(sparsevec, sparsevec),
	FUNCTION 3 hnsw_sparsevec_support(internal);

CREATE OPERATOR CLASS sparsevec_cosine_ops
	FOR TYPE sparsevec USING hnsw AS
	OPERATOR 1 <=> (sparsevec, sparsevec) FOR ORDER BY float_ops,
	FUNCTION 1 sparsevec_negative_inner_product(sparsevec, sparsevec),
	FUNCTION 2 l2_norm(sparsevec),
	FUNCTION 3 hnsw_sparsevec_support(internal);

CREATE OPERATOR CLASS sparsevec_l1_ops
	FOR TYPE sparsevec USING hnsw AS
	OPERATOR 1 <+> (sparsevec, sparsevec) FOR ORDER BY float_ops,
	FUNCTION 1 l1_distance(sparsevec, sparsevec),
	FUNCTION 3 hnsw_sparsevec_support(internal);
"#,
    ),
];
