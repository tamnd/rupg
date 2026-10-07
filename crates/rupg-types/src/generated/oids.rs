//! The built-in types of PostgreSQL: one for each entry of `pg_type.dat`, and one for each array type that an entry asks for with `array_type_oid`, as `genbki.pl` makes them.
//!
//! `cargo xtask pgtype` makes this file from `vendor/postgres-19/src/include/catalog/pg_type.dat`. Do not edit it.

use crate::types::{Oid, TypeInfo, t};

/// `bool`, boolean, format 't'/'f'.
pub const BOOL: Oid = 16;
/// `bytea`, variable-length string, binary values escaped.
pub const BYTEA: Oid = 17;
/// `char`, single character.
pub const CHAR: Oid = 18;
/// `name`, 63-byte type for storing system identifiers.
pub const NAME: Oid = 19;
/// `int8`, ~18 digit integer, 8-byte storage.
pub const INT8: Oid = 20;
/// `int2`, -32 thousand to 32 thousand, 2-byte storage.
pub const INT2: Oid = 21;
/// `int2vector`, array of int2, used in system tables.
pub const INT2VECTOR: Oid = 22;
/// `int4`, -2 billion to 2 billion integer, 4-byte storage.
pub const INT4: Oid = 23;
/// `regproc`, registered procedure.
pub const REGPROC: Oid = 24;
/// `text`, variable-length string, no limit specified.
pub const TEXT: Oid = 25;
/// `oid`, object identifier(oid), maximum 4 billion.
pub const OID: Oid = 26;
/// `tid`, tuple physical location, format '(block,offset)'.
pub const TID: Oid = 27;
/// `xid`, transaction id.
pub const XID: Oid = 28;
/// `cid`, command identifier type, sequence in transaction id.
pub const CID: Oid = 29;
/// `oidvector`, array of oids, used in system tables.
pub const OIDVECTOR: Oid = 30;
/// `pg_ddl_command`, internal type for passing CollectedCommand.
pub const PG_DDL_COMMAND: Oid = 32;
/// `pg_type`.
pub const PG_TYPE: Oid = 71;
/// `pg_attribute`.
pub const PG_ATTRIBUTE: Oid = 75;
/// `pg_proc`.
pub const PG_PROC: Oid = 81;
/// `pg_class`.
pub const PG_CLASS: Oid = 83;
/// `json`, JSON stored as text.
pub const JSON: Oid = 114;
/// `xml`, XML content.
pub const XML: Oid = 142;
/// `_xml`, the array of `xml`.
pub const XML_ARRAY: Oid = 143;
/// `pg_node_tree`, string representing an internal node tree.
pub const PG_NODE_TREE: Oid = 194;
/// `_json`, the array of `json`.
pub const JSON_ARRAY: Oid = 199;
/// `_pg_type`, the array of `pg_type`.
pub const PG_TYPE_ARRAY: Oid = 210;
/// `table_am_handler`, pseudo-type for the result of a table AM handler function.
pub const TABLE_AM_HANDLER: Oid = 269;
/// `_pg_attribute`, the array of `pg_attribute`.
pub const PG_ATTRIBUTE_ARRAY: Oid = 270;
/// `_xid8`, the array of `xid8`.
pub const XID8_ARRAY: Oid = 271;
/// `_pg_proc`, the array of `pg_proc`.
pub const PG_PROC_ARRAY: Oid = 272;
/// `_pg_class`, the array of `pg_class`.
pub const PG_CLASS_ARRAY: Oid = 273;
/// `index_am_handler`, pseudo-type for the result of an index AM handler function.
pub const INDEX_AM_HANDLER: Oid = 325;
/// `point`, geometric point, format '(x,y)'.
pub const POINT: Oid = 600;
/// `lseg`, geometric line segment, format '\[point1,point2\]'.
pub const LSEG: Oid = 601;
/// `path`, geometric path, format '(point1,...)'.
pub const PATH: Oid = 602;
/// `box`, geometric box, format 'lower left point,upper right point'.
pub const BOX: Oid = 603;
/// `polygon`, geometric polygon, format '(point1,...)'.
pub const POLYGON: Oid = 604;
/// `line`, geometric line, formats '{A,B,C}'/'\[point1,point2\]'.
pub const LINE: Oid = 628;
/// `_line`, the array of `line`.
pub const LINE_ARRAY: Oid = 629;
/// `cidr`, network IP address/netmask, network address.
pub const CIDR: Oid = 650;
/// `_cidr`, the array of `cidr`.
pub const CIDR_ARRAY: Oid = 651;
/// `float4`, single-precision floating point number, 4-byte storage.
pub const FLOAT4: Oid = 700;
/// `float8`, double-precision floating point number, 8-byte storage.
pub const FLOAT8: Oid = 701;
/// `unknown`, pseudo-type representing an undetermined type.
pub const UNKNOWN: Oid = 705;
/// `circle`, geometric circle, format '\<center point,radius\>'.
pub const CIRCLE: Oid = 718;
/// `_circle`, the array of `circle`.
pub const CIRCLE_ARRAY: Oid = 719;
/// `macaddr8`, XX:XX:XX:XX:XX:XX:XX:XX, MAC address.
pub const MACADDR8: Oid = 774;
/// `_macaddr8`, the array of `macaddr8`.
pub const MACADDR8_ARRAY: Oid = 775;
/// `money`, monetary amounts, $d,ddd.cc.
pub const MONEY: Oid = 790;
/// `_money`, the array of `money`.
pub const MONEY_ARRAY: Oid = 791;
/// `macaddr`, XX:XX:XX:XX:XX:XX, MAC address.
pub const MACADDR: Oid = 829;
/// `inet`, IP address/netmask, host address, netmask optional.
pub const INET: Oid = 869;
/// `_bool`, the array of `bool`.
pub const BOOL_ARRAY: Oid = 1000;
/// `_bytea`, the array of `bytea`.
pub const BYTEA_ARRAY: Oid = 1001;
/// `_char`, the array of `char`.
pub const CHAR_ARRAY: Oid = 1002;
/// `_name`, the array of `name`.
pub const NAME_ARRAY: Oid = 1003;
/// `_int2`, the array of `int2`.
pub const INT2_ARRAY: Oid = 1005;
/// `_int2vector`, the array of `int2vector`.
pub const INT2VECTOR_ARRAY: Oid = 1006;
/// `_int4`, the array of `int4`.
pub const INT4_ARRAY: Oid = 1007;
/// `_regproc`, the array of `regproc`.
pub const REGPROC_ARRAY: Oid = 1008;
/// `_text`, the array of `text`.
pub const TEXT_ARRAY: Oid = 1009;
/// `_tid`, the array of `tid`.
pub const TID_ARRAY: Oid = 1010;
/// `_xid`, the array of `xid`.
pub const XID_ARRAY: Oid = 1011;
/// `_cid`, the array of `cid`.
pub const CID_ARRAY: Oid = 1012;
/// `_oidvector`, the array of `oidvector`.
pub const OIDVECTOR_ARRAY: Oid = 1013;
/// `_bpchar`, the array of `bpchar`.
pub const BPCHAR_ARRAY: Oid = 1014;
/// `_varchar`, the array of `varchar`.
pub const VARCHAR_ARRAY: Oid = 1015;
/// `_int8`, the array of `int8`.
pub const INT8_ARRAY: Oid = 1016;
/// `_point`, the array of `point`.
pub const POINT_ARRAY: Oid = 1017;
/// `_lseg`, the array of `lseg`.
pub const LSEG_ARRAY: Oid = 1018;
/// `_path`, the array of `path`.
pub const PATH_ARRAY: Oid = 1019;
/// `_box`, the array of `box`.
pub const BOX_ARRAY: Oid = 1020;
/// `_float4`, the array of `float4`.
pub const FLOAT4_ARRAY: Oid = 1021;
/// `_float8`, the array of `float8`.
pub const FLOAT8_ARRAY: Oid = 1022;
/// `_polygon`, the array of `polygon`.
pub const POLYGON_ARRAY: Oid = 1027;
/// `_oid`, the array of `oid`.
pub const OID_ARRAY: Oid = 1028;
/// `aclitem`, access control list.
pub const ACLITEM: Oid = 1033;
/// `_aclitem`, the array of `aclitem`.
pub const ACLITEM_ARRAY: Oid = 1034;
/// `_macaddr`, the array of `macaddr`.
pub const MACADDR_ARRAY: Oid = 1040;
/// `_inet`, the array of `inet`.
pub const INET_ARRAY: Oid = 1041;
/// `bpchar`, 'char(length)' blank-padded string, fixed storage length.
pub const BPCHAR: Oid = 1042;
/// `varchar`, 'varchar(length)' non-blank-padded string, variable storage length.
pub const VARCHAR: Oid = 1043;
/// `date`, date.
pub const DATE: Oid = 1082;
/// `time`, time of day.
pub const TIME: Oid = 1083;
/// `timestamp`, date and time.
pub const TIMESTAMP: Oid = 1114;
/// `_timestamp`, the array of `timestamp`.
pub const TIMESTAMP_ARRAY: Oid = 1115;
/// `_date`, the array of `date`.
pub const DATE_ARRAY: Oid = 1182;
/// `_time`, the array of `time`.
pub const TIME_ARRAY: Oid = 1183;
/// `timestamptz`, date and time with time zone.
pub const TIMESTAMPTZ: Oid = 1184;
/// `_timestamptz`, the array of `timestamptz`.
pub const TIMESTAMPTZ_ARRAY: Oid = 1185;
/// `interval`, time interval, format 'number units ...'.
pub const INTERVAL: Oid = 1186;
/// `_interval`, the array of `interval`.
pub const INTERVAL_ARRAY: Oid = 1187;
/// `_numeric`, the array of `numeric`.
pub const NUMERIC_ARRAY: Oid = 1231;
/// `_cstring`, the array of `cstring`.
pub const CSTRING_ARRAY: Oid = 1263;
/// `timetz`, time of day with time zone.
pub const TIMETZ: Oid = 1266;
/// `_timetz`, the array of `timetz`.
pub const TIMETZ_ARRAY: Oid = 1270;
/// `bit`, fixed-length bit string.
pub const BIT: Oid = 1560;
/// `_bit`, the array of `bit`.
pub const BIT_ARRAY: Oid = 1561;
/// `varbit`, variable-length bit string.
pub const VARBIT: Oid = 1562;
/// `_varbit`, the array of `varbit`.
pub const VARBIT_ARRAY: Oid = 1563;
/// `numeric`, 'numeric(precision, scale)' arbitrary precision number.
pub const NUMERIC: Oid = 1700;
/// `refcursor`, reference to cursor (portal name).
pub const REFCURSOR: Oid = 1790;
/// `_refcursor`, the array of `refcursor`.
pub const REFCURSOR_ARRAY: Oid = 2201;
/// `regprocedure`, registered procedure (with args).
pub const REGPROCEDURE: Oid = 2202;
/// `regoper`, registered operator.
pub const REGOPER: Oid = 2203;
/// `regoperator`, registered operator (with args).
pub const REGOPERATOR: Oid = 2204;
/// `regclass`, registered class.
pub const REGCLASS: Oid = 2205;
/// `regtype`, registered type.
pub const REGTYPE: Oid = 2206;
/// `_regprocedure`, the array of `regprocedure`.
pub const REGPROCEDURE_ARRAY: Oid = 2207;
/// `_regoper`, the array of `regoper`.
pub const REGOPER_ARRAY: Oid = 2208;
/// `_regoperator`, the array of `regoperator`.
pub const REGOPERATOR_ARRAY: Oid = 2209;
/// `_regclass`, the array of `regclass`.
pub const REGCLASS_ARRAY: Oid = 2210;
/// `_regtype`, the array of `regtype`.
pub const REGTYPE_ARRAY: Oid = 2211;
/// `record`, pseudo-type representing any composite type.
pub const RECORD: Oid = 2249;
/// `cstring`, C-style string.
pub const CSTRING: Oid = 2275;
/// `any`, pseudo-type representing any type.
pub const ANY: Oid = 2276;
/// `anyarray`, pseudo-type representing a polymorphic array type.
pub const ANYARRAY: Oid = 2277;
/// `void`, pseudo-type for the result of a function with no real result.
pub const VOID: Oid = 2278;
/// `trigger`, pseudo-type for the result of a trigger function.
pub const TRIGGER: Oid = 2279;
/// `language_handler`, pseudo-type for the result of a language handler function.
pub const LANGUAGE_HANDLER: Oid = 2280;
/// `internal`, pseudo-type representing an internal data structure.
pub const INTERNAL: Oid = 2281;
/// `anyelement`, pseudo-type representing a polymorphic base type.
pub const ANYELEMENT: Oid = 2283;
/// `_record`.
pub const RECORD_ARRAY: Oid = 2287;
/// `anynonarray`, pseudo-type representing a polymorphic base type that is not an array.
pub const ANYNONARRAY: Oid = 2776;
/// `_txid_snapshot`, the array of `txid_snapshot`.
pub const TXID_SNAPSHOT_ARRAY: Oid = 2949;
/// `uuid`, UUID.
pub const UUID: Oid = 2950;
/// `_uuid`, the array of `uuid`.
pub const UUID_ARRAY: Oid = 2951;
/// `txid_snapshot`, transaction snapshot.
pub const TXID_SNAPSHOT: Oid = 2970;
/// `fdw_handler`, pseudo-type for the result of an FDW handler function.
pub const FDW_HANDLER: Oid = 3115;
/// `pg_lsn`, PostgreSQL LSN.
pub const PG_LSN: Oid = 3220;
/// `_pg_lsn`, the array of `pg_lsn`.
pub const PG_LSN_ARRAY: Oid = 3221;
/// `tsm_handler`, pseudo-type for the result of a tablesample method function.
pub const TSM_HANDLER: Oid = 3310;
/// `pg_ndistinct`, multivariate ndistinct coefficients.
pub const PG_NDISTINCT: Oid = 3361;
/// `pg_dependencies`, multivariate dependencies.
pub const PG_DEPENDENCIES: Oid = 3402;
/// `anyenum`, pseudo-type representing a polymorphic base type that is an enum.
pub const ANYENUM: Oid = 3500;
/// `tsvector`, text representation for text search.
pub const TSVECTOR: Oid = 3614;
/// `tsquery`, query representation for text search.
pub const TSQUERY: Oid = 3615;
/// `gtsvector`, GiST index internal text representation for text search.
pub const GTSVECTOR: Oid = 3642;
/// `_tsvector`, the array of `tsvector`.
pub const TSVECTOR_ARRAY: Oid = 3643;
/// `_gtsvector`, the array of `gtsvector`.
pub const GTSVECTOR_ARRAY: Oid = 3644;
/// `_tsquery`, the array of `tsquery`.
pub const TSQUERY_ARRAY: Oid = 3645;
/// `regconfig`, registered text search configuration.
pub const REGCONFIG: Oid = 3734;
/// `_regconfig`, the array of `regconfig`.
pub const REGCONFIG_ARRAY: Oid = 3735;
/// `regdictionary`, registered text search dictionary.
pub const REGDICTIONARY: Oid = 3769;
/// `_regdictionary`, the array of `regdictionary`.
pub const REGDICTIONARY_ARRAY: Oid = 3770;
/// `jsonb`, Binary JSON.
pub const JSONB: Oid = 3802;
/// `_jsonb`, the array of `jsonb`.
pub const JSONB_ARRAY: Oid = 3807;
/// `anyrange`, pseudo-type representing a range over a polymorphic base type.
pub const ANYRANGE: Oid = 3831;
/// `event_trigger`, pseudo-type for the result of an event trigger function.
pub const EVENT_TRIGGER: Oid = 3838;
/// `int4range`, range of integers.
pub const INT4RANGE: Oid = 3904;
/// `_int4range`, the array of `int4range`.
pub const INT4RANGE_ARRAY: Oid = 3905;
/// `numrange`, range of numerics.
pub const NUMRANGE: Oid = 3906;
/// `_numrange`, the array of `numrange`.
pub const NUMRANGE_ARRAY: Oid = 3907;
/// `tsrange`, range of timestamps without time zone.
pub const TSRANGE: Oid = 3908;
/// `_tsrange`, the array of `tsrange`.
pub const TSRANGE_ARRAY: Oid = 3909;
/// `tstzrange`, range of timestamps with time zone.
pub const TSTZRANGE: Oid = 3910;
/// `_tstzrange`, the array of `tstzrange`.
pub const TSTZRANGE_ARRAY: Oid = 3911;
/// `daterange`, range of dates.
pub const DATERANGE: Oid = 3912;
/// `_daterange`, the array of `daterange`.
pub const DATERANGE_ARRAY: Oid = 3913;
/// `int8range`, range of bigints.
pub const INT8RANGE: Oid = 3926;
/// `_int8range`, the array of `int8range`.
pub const INT8RANGE_ARRAY: Oid = 3927;
/// `jsonpath`, JSON path.
pub const JSONPATH: Oid = 4072;
/// `_jsonpath`, the array of `jsonpath`.
pub const JSONPATH_ARRAY: Oid = 4073;
/// `regnamespace`, registered namespace.
pub const REGNAMESPACE: Oid = 4089;
/// `_regnamespace`, the array of `regnamespace`.
pub const REGNAMESPACE_ARRAY: Oid = 4090;
/// `regrole`, registered role.
pub const REGROLE: Oid = 4096;
/// `_regrole`, the array of `regrole`.
pub const REGROLE_ARRAY: Oid = 4097;
/// `regcollation`, registered collation.
pub const REGCOLLATION: Oid = 4191;
/// `_regcollation`, the array of `regcollation`.
pub const REGCOLLATION_ARRAY: Oid = 4192;
/// `int4multirange`, multirange of integers.
pub const INT4MULTIRANGE: Oid = 4451;
/// `nummultirange`, multirange of numerics.
pub const NUMMULTIRANGE: Oid = 4532;
/// `tsmultirange`, multirange of timestamps without time zone.
pub const TSMULTIRANGE: Oid = 4533;
/// `tstzmultirange`, multirange of timestamps with time zone.
pub const TSTZMULTIRANGE: Oid = 4534;
/// `datemultirange`, multirange of dates.
pub const DATEMULTIRANGE: Oid = 4535;
/// `int8multirange`, multirange of bigints.
pub const INT8MULTIRANGE: Oid = 4536;
/// `anymultirange`, pseudo-type representing a polymorphic base type that is a multirange.
pub const ANYMULTIRANGE: Oid = 4537;
/// `anycompatiblemultirange`, pseudo-type representing a multirange over a polymorphic common type.
pub const ANYCOMPATIBLEMULTIRANGE: Oid = 4538;
/// `pg_brin_bloom_summary`, pseudo-type representing BRIN bloom summary.
pub const PG_BRIN_BLOOM_SUMMARY: Oid = 4600;
/// `pg_brin_minmax_multi_summary`, pseudo-type representing BRIN minmax-multi summary.
pub const PG_BRIN_MINMAX_MULTI_SUMMARY: Oid = 4601;
/// `pg_mcv_list`, multivariate MCV list.
pub const PG_MCV_LIST: Oid = 5017;
/// `pg_snapshot`, transaction snapshot.
pub const PG_SNAPSHOT: Oid = 5038;
/// `_pg_snapshot`, the array of `pg_snapshot`.
pub const PG_SNAPSHOT_ARRAY: Oid = 5039;
/// `xid8`, full transaction id.
pub const XID8: Oid = 5069;
/// `anycompatible`, pseudo-type representing a polymorphic common type.
pub const ANYCOMPATIBLE: Oid = 5077;
/// `anycompatiblearray`, pseudo-type representing an array of polymorphic common type elements.
pub const ANYCOMPATIBLEARRAY: Oid = 5078;
/// `anycompatiblenonarray`, pseudo-type representing a polymorphic common type that is not an array.
pub const ANYCOMPATIBLENONARRAY: Oid = 5079;
/// `anycompatiblerange`, pseudo-type representing a range over a polymorphic common type.
pub const ANYCOMPATIBLERANGE: Oid = 5080;
/// `_int4multirange`, the array of `int4multirange`.
pub const INT4MULTIRANGE_ARRAY: Oid = 6150;
/// `_nummultirange`, the array of `nummultirange`.
pub const NUMMULTIRANGE_ARRAY: Oid = 6151;
/// `_tsmultirange`, the array of `tsmultirange`.
pub const TSMULTIRANGE_ARRAY: Oid = 6152;
/// `_tstzmultirange`, the array of `tstzmultirange`.
pub const TSTZMULTIRANGE_ARRAY: Oid = 6153;
/// `_datemultirange`, the array of `datemultirange`.
pub const DATEMULTIRANGE_ARRAY: Oid = 6155;
/// `_int8multirange`, the array of `int8multirange`.
pub const INT8MULTIRANGE_ARRAY: Oid = 6157;
/// `oid8`, object identifier(oid8), 8 bytes.
pub const OID8: Oid = 6437;
/// `_oid8`, the array of `oid8`.
pub const OID8_ARRAY: Oid = 6442;
/// `regdatabase`, registered database.
pub const REGDATABASE: Oid = 6490;
/// `_regdatabase`, the array of `regdatabase`.
pub const REGDATABASE_ARRAY: Oid = 6491;

/// Every type in the order of the OIDs: the OID, `typname`, `typtype`, `typcategory`, `typlen`, `typelem`, `typarray` and `typdelim`.
pub(crate) static TYPES: [TypeInfo; 197] = [
    t(16, "bool", b'b', b'B', 1, 0, 1000, b','),
    t(17, "bytea", b'b', b'U', -1, 0, 1001, b','),
    t(18, "char", b'b', b'Z', 1, 0, 1002, b','),
    t(19, "name", b'b', b'S', 64, 18, 1003, b','),
    t(20, "int8", b'b', b'N', 8, 0, 1016, b','),
    t(21, "int2", b'b', b'N', 2, 0, 1005, b','),
    t(22, "int2vector", b'b', b'A', -1, 21, 1006, b','),
    t(23, "int4", b'b', b'N', 4, 0, 1007, b','),
    t(24, "regproc", b'b', b'N', 4, 0, 1008, b','),
    t(25, "text", b'b', b'S', -1, 0, 1009, b','),
    t(26, "oid", b'b', b'N', 4, 0, 1028, b','),
    t(27, "tid", b'b', b'U', 6, 0, 1010, b','),
    t(28, "xid", b'b', b'U', 4, 0, 1011, b','),
    t(29, "cid", b'b', b'U', 4, 0, 1012, b','),
    t(30, "oidvector", b'b', b'A', -1, 26, 1013, b','),
    t(32, "pg_ddl_command", b'p', b'P', 8, 0, 0, b','),
    t(71, "pg_type", b'c', b'C', -1, 0, 210, b','),
    t(75, "pg_attribute", b'c', b'C', -1, 0, 270, b','),
    t(81, "pg_proc", b'c', b'C', -1, 0, 272, b','),
    t(83, "pg_class", b'c', b'C', -1, 0, 273, b','),
    t(114, "json", b'b', b'U', -1, 0, 199, b','),
    t(142, "xml", b'b', b'U', -1, 0, 143, b','),
    t(143, "_xml", b'b', b'A', -1, 142, 0, b','),
    t(194, "pg_node_tree", b'b', b'Z', -1, 0, 0, b','),
    t(199, "_json", b'b', b'A', -1, 114, 0, b','),
    t(210, "_pg_type", b'b', b'A', -1, 71, 0, b','),
    t(269, "table_am_handler", b'p', b'P', 4, 0, 0, b','),
    t(270, "_pg_attribute", b'b', b'A', -1, 75, 0, b','),
    t(271, "_xid8", b'b', b'A', -1, 5069, 0, b','),
    t(272, "_pg_proc", b'b', b'A', -1, 81, 0, b','),
    t(273, "_pg_class", b'b', b'A', -1, 83, 0, b','),
    t(325, "index_am_handler", b'p', b'P', 4, 0, 0, b','),
    t(600, "point", b'b', b'G', 16, 701, 1017, b','),
    t(601, "lseg", b'b', b'G', 32, 600, 1018, b','),
    t(602, "path", b'b', b'G', -1, 0, 1019, b','),
    t(603, "box", b'b', b'G', 32, 600, 1020, b';'),
    t(604, "polygon", b'b', b'G', -1, 0, 1027, b','),
    t(628, "line", b'b', b'G', 24, 701, 629, b','),
    t(629, "_line", b'b', b'A', -1, 628, 0, b','),
    t(650, "cidr", b'b', b'I', -1, 0, 651, b','),
    t(651, "_cidr", b'b', b'A', -1, 650, 0, b','),
    t(700, "float4", b'b', b'N', 4, 0, 1021, b','),
    t(701, "float8", b'b', b'N', 8, 0, 1022, b','),
    t(705, "unknown", b'p', b'X', -2, 0, 0, b','),
    t(718, "circle", b'b', b'G', 24, 0, 719, b','),
    t(719, "_circle", b'b', b'A', -1, 718, 0, b','),
    t(774, "macaddr8", b'b', b'U', 8, 0, 775, b','),
    t(775, "_macaddr8", b'b', b'A', -1, 774, 0, b','),
    t(790, "money", b'b', b'N', 8, 0, 791, b','),
    t(791, "_money", b'b', b'A', -1, 790, 0, b','),
    t(829, "macaddr", b'b', b'U', 6, 0, 1040, b','),
    t(869, "inet", b'b', b'I', -1, 0, 1041, b','),
    t(1000, "_bool", b'b', b'A', -1, 16, 0, b','),
    t(1001, "_bytea", b'b', b'A', -1, 17, 0, b','),
    t(1002, "_char", b'b', b'A', -1, 18, 0, b','),
    t(1003, "_name", b'b', b'A', -1, 19, 0, b','),
    t(1005, "_int2", b'b', b'A', -1, 21, 0, b','),
    t(1006, "_int2vector", b'b', b'A', -1, 22, 0, b','),
    t(1007, "_int4", b'b', b'A', -1, 23, 0, b','),
    t(1008, "_regproc", b'b', b'A', -1, 24, 0, b','),
    t(1009, "_text", b'b', b'A', -1, 25, 0, b','),
    t(1010, "_tid", b'b', b'A', -1, 27, 0, b','),
    t(1011, "_xid", b'b', b'A', -1, 28, 0, b','),
    t(1012, "_cid", b'b', b'A', -1, 29, 0, b','),
    t(1013, "_oidvector", b'b', b'A', -1, 30, 0, b','),
    t(1014, "_bpchar", b'b', b'A', -1, 1042, 0, b','),
    t(1015, "_varchar", b'b', b'A', -1, 1043, 0, b','),
    t(1016, "_int8", b'b', b'A', -1, 20, 0, b','),
    t(1017, "_point", b'b', b'A', -1, 600, 0, b','),
    t(1018, "_lseg", b'b', b'A', -1, 601, 0, b','),
    t(1019, "_path", b'b', b'A', -1, 602, 0, b','),
    t(1020, "_box", b'b', b'A', -1, 603, 0, b';'),
    t(1021, "_float4", b'b', b'A', -1, 700, 0, b','),
    t(1022, "_float8", b'b', b'A', -1, 701, 0, b','),
    t(1027, "_polygon", b'b', b'A', -1, 604, 0, b','),
    t(1028, "_oid", b'b', b'A', -1, 26, 0, b','),
    t(1033, "aclitem", b'b', b'U', 16, 0, 1034, b','),
    t(1034, "_aclitem", b'b', b'A', -1, 1033, 0, b','),
    t(1040, "_macaddr", b'b', b'A', -1, 829, 0, b','),
    t(1041, "_inet", b'b', b'A', -1, 869, 0, b','),
    t(1042, "bpchar", b'b', b'S', -1, 0, 1014, b','),
    t(1043, "varchar", b'b', b'S', -1, 0, 1015, b','),
    t(1082, "date", b'b', b'D', 4, 0, 1182, b','),
    t(1083, "time", b'b', b'D', 8, 0, 1183, b','),
    t(1114, "timestamp", b'b', b'D', 8, 0, 1115, b','),
    t(1115, "_timestamp", b'b', b'A', -1, 1114, 0, b','),
    t(1182, "_date", b'b', b'A', -1, 1082, 0, b','),
    t(1183, "_time", b'b', b'A', -1, 1083, 0, b','),
    t(1184, "timestamptz", b'b', b'D', 8, 0, 1185, b','),
    t(1185, "_timestamptz", b'b', b'A', -1, 1184, 0, b','),
    t(1186, "interval", b'b', b'T', 16, 0, 1187, b','),
    t(1187, "_interval", b'b', b'A', -1, 1186, 0, b','),
    t(1231, "_numeric", b'b', b'A', -1, 1700, 0, b','),
    t(1263, "_cstring", b'b', b'A', -1, 2275, 0, b','),
    t(1266, "timetz", b'b', b'D', 12, 0, 1270, b','),
    t(1270, "_timetz", b'b', b'A', -1, 1266, 0, b','),
    t(1560, "bit", b'b', b'V', -1, 0, 1561, b','),
    t(1561, "_bit", b'b', b'A', -1, 1560, 0, b','),
    t(1562, "varbit", b'b', b'V', -1, 0, 1563, b','),
    t(1563, "_varbit", b'b', b'A', -1, 1562, 0, b','),
    t(1700, "numeric", b'b', b'N', -1, 0, 1231, b','),
    t(1790, "refcursor", b'b', b'U', -1, 0, 2201, b','),
    t(2201, "_refcursor", b'b', b'A', -1, 1790, 0, b','),
    t(2202, "regprocedure", b'b', b'N', 4, 0, 2207, b','),
    t(2203, "regoper", b'b', b'N', 4, 0, 2208, b','),
    t(2204, "regoperator", b'b', b'N', 4, 0, 2209, b','),
    t(2205, "regclass", b'b', b'N', 4, 0, 2210, b','),
    t(2206, "regtype", b'b', b'N', 4, 0, 2211, b','),
    t(2207, "_regprocedure", b'b', b'A', -1, 2202, 0, b','),
    t(2208, "_regoper", b'b', b'A', -1, 2203, 0, b','),
    t(2209, "_regoperator", b'b', b'A', -1, 2204, 0, b','),
    t(2210, "_regclass", b'b', b'A', -1, 2205, 0, b','),
    t(2211, "_regtype", b'b', b'A', -1, 2206, 0, b','),
    t(2249, "record", b'p', b'P', -1, 0, 2287, b','),
    t(2275, "cstring", b'p', b'P', -2, 0, 1263, b','),
    t(2276, "any", b'p', b'P', 4, 0, 0, b','),
    t(2277, "anyarray", b'p', b'P', -1, 0, 0, b','),
    t(2278, "void", b'p', b'P', 4, 0, 0, b','),
    t(2279, "trigger", b'p', b'P', 4, 0, 0, b','),
    t(2280, "language_handler", b'p', b'P', 4, 0, 0, b','),
    t(2281, "internal", b'p', b'P', 8, 0, 0, b','),
    t(2283, "anyelement", b'p', b'P', 4, 0, 0, b','),
    t(2287, "_record", b'p', b'P', -1, 2249, 0, b','),
    t(2776, "anynonarray", b'p', b'P', 4, 0, 0, b','),
    t(2949, "_txid_snapshot", b'b', b'A', -1, 2970, 0, b','),
    t(2950, "uuid", b'b', b'U', 16, 0, 2951, b','),
    t(2951, "_uuid", b'b', b'A', -1, 2950, 0, b','),
    t(2970, "txid_snapshot", b'b', b'U', -1, 0, 2949, b','),
    t(3115, "fdw_handler", b'p', b'P', 4, 0, 0, b','),
    t(3220, "pg_lsn", b'b', b'U', 8, 0, 3221, b','),
    t(3221, "_pg_lsn", b'b', b'A', -1, 3220, 0, b','),
    t(3310, "tsm_handler", b'p', b'P', 4, 0, 0, b','),
    t(3361, "pg_ndistinct", b'b', b'Z', -1, 0, 0, b','),
    t(3402, "pg_dependencies", b'b', b'Z', -1, 0, 0, b','),
    t(3500, "anyenum", b'p', b'P', 4, 0, 0, b','),
    t(3614, "tsvector", b'b', b'U', -1, 0, 3643, b','),
    t(3615, "tsquery", b'b', b'U', -1, 0, 3645, b','),
    t(3642, "gtsvector", b'b', b'U', -1, 0, 3644, b','),
    t(3643, "_tsvector", b'b', b'A', -1, 3614, 0, b','),
    t(3644, "_gtsvector", b'b', b'A', -1, 3642, 0, b','),
    t(3645, "_tsquery", b'b', b'A', -1, 3615, 0, b','),
    t(3734, "regconfig", b'b', b'N', 4, 0, 3735, b','),
    t(3735, "_regconfig", b'b', b'A', -1, 3734, 0, b','),
    t(3769, "regdictionary", b'b', b'N', 4, 0, 3770, b','),
    t(3770, "_regdictionary", b'b', b'A', -1, 3769, 0, b','),
    t(3802, "jsonb", b'b', b'U', -1, 0, 3807, b','),
    t(3807, "_jsonb", b'b', b'A', -1, 3802, 0, b','),
    t(3831, "anyrange", b'p', b'P', -1, 0, 0, b','),
    t(3838, "event_trigger", b'p', b'P', 4, 0, 0, b','),
    t(3904, "int4range", b'r', b'R', -1, 0, 3905, b','),
    t(3905, "_int4range", b'b', b'A', -1, 3904, 0, b','),
    t(3906, "numrange", b'r', b'R', -1, 0, 3907, b','),
    t(3907, "_numrange", b'b', b'A', -1, 3906, 0, b','),
    t(3908, "tsrange", b'r', b'R', -1, 0, 3909, b','),
    t(3909, "_tsrange", b'b', b'A', -1, 3908, 0, b','),
    t(3910, "tstzrange", b'r', b'R', -1, 0, 3911, b','),
    t(3911, "_tstzrange", b'b', b'A', -1, 3910, 0, b','),
    t(3912, "daterange", b'r', b'R', -1, 0, 3913, b','),
    t(3913, "_daterange", b'b', b'A', -1, 3912, 0, b','),
    t(3926, "int8range", b'r', b'R', -1, 0, 3927, b','),
    t(3927, "_int8range", b'b', b'A', -1, 3926, 0, b','),
    t(4072, "jsonpath", b'b', b'U', -1, 0, 4073, b','),
    t(4073, "_jsonpath", b'b', b'A', -1, 4072, 0, b','),
    t(4089, "regnamespace", b'b', b'N', 4, 0, 4090, b','),
    t(4090, "_regnamespace", b'b', b'A', -1, 4089, 0, b','),
    t(4096, "regrole", b'b', b'N', 4, 0, 4097, b','),
    t(4097, "_regrole", b'b', b'A', -1, 4096, 0, b','),
    t(4191, "regcollation", b'b', b'N', 4, 0, 4192, b','),
    t(4192, "_regcollation", b'b', b'A', -1, 4191, 0, b','),
    t(4451, "int4multirange", b'm', b'R', -1, 0, 6150, b','),
    t(4532, "nummultirange", b'm', b'R', -1, 0, 6151, b','),
    t(4533, "tsmultirange", b'm', b'R', -1, 0, 6152, b','),
    t(4534, "tstzmultirange", b'm', b'R', -1, 0, 6153, b','),
    t(4535, "datemultirange", b'm', b'R', -1, 0, 6155, b','),
    t(4536, "int8multirange", b'm', b'R', -1, 0, 6157, b','),
    t(4537, "anymultirange", b'p', b'P', -1, 0, 0, b','),
    t(4538, "anycompatiblemultirange", b'p', b'P', -1, 0, 0, b','),
    t(4600, "pg_brin_bloom_summary", b'b', b'Z', -1, 0, 0, b','),
    t(4601, "pg_brin_minmax_multi_summary", b'b', b'Z', -1, 0, 0, b','),
    t(5017, "pg_mcv_list", b'b', b'Z', -1, 0, 0, b','),
    t(5038, "pg_snapshot", b'b', b'U', -1, 0, 5039, b','),
    t(5039, "_pg_snapshot", b'b', b'A', -1, 5038, 0, b','),
    t(5069, "xid8", b'b', b'U', 8, 0, 271, b','),
    t(5077, "anycompatible", b'p', b'P', 4, 0, 0, b','),
    t(5078, "anycompatiblearray", b'p', b'P', -1, 0, 0, b','),
    t(5079, "anycompatiblenonarray", b'p', b'P', 4, 0, 0, b','),
    t(5080, "anycompatiblerange", b'p', b'P', -1, 0, 0, b','),
    t(6150, "_int4multirange", b'b', b'A', -1, 4451, 0, b','),
    t(6151, "_nummultirange", b'b', b'A', -1, 4532, 0, b','),
    t(6152, "_tsmultirange", b'b', b'A', -1, 4533, 0, b','),
    t(6153, "_tstzmultirange", b'b', b'A', -1, 4534, 0, b','),
    t(6155, "_datemultirange", b'b', b'A', -1, 4535, 0, b','),
    t(6157, "_int8multirange", b'b', b'A', -1, 4536, 0, b','),
    t(6437, "oid8", b'b', b'N', 8, 0, 6442, b','),
    t(6442, "_oid8", b'b', b'A', -1, 6437, 0, b','),
    t(6490, "regdatabase", b'b', b'N', 4, 0, 6491, b','),
    t(6491, "_regdatabase", b'b', b'A', -1, 6490, 0, b','),
];
