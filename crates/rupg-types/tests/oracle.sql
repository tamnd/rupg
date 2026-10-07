-- Makes oracle.tsv from the server at the pin:
--   psql -X -At -F "$(printf '\t')" -f oracle.sql > oracle.tsv
-- Each line is the type, extra_float_digits, the input, and the output text or the error. A type
-- that starts with "send" has the hex of the binary output.
\set QUIET on
create temp table inputs (n serial, t text, i text);
insert into inputs (t, i) values
  ('int2', '32767'), ('int2', '-32768'), ('int2', '32768'), ('int2', '0x7FFF'), ('int2', ' 1_2 '),
  ('int4', '0'), ('int4', '-0'), ('int4', ' 12 '), ('int4', '+7'), ('int4', '0x1F'), ('int4', '-0X1f'),
  ('int4', '0o17'), ('int4', '0b101'), ('int4', '1_000'), ('int4', '0x_1F'), ('int4', '2147483647'),
  ('int4', '-2147483648'), ('int4', '-0x80000000'), ('int4', ''), ('int4', ' '), ('int4', '1e3'),
  ('int4', '1.5'), ('int4', '_1'), ('int4', '1_'), ('int4', '1__0'), ('int4', '0x'), ('int4', '0b2'),
  ('int4', '- 1'), ('int4', '1 2'), ('int4', '0o8'), ('int4', '2147483648'), ('int4', '-2147483649'),
  ('int4', '0x80000000'), ('int4', ' 99999999999 '), ('int4', '99999999999x'), ('int4', '00012'),
  ('int8', '-9223372036854775808'), ('int8', '9223372036854775808'), ('int8', '0x7FFF_FFFF_FFFF_FFFF'),
  ('int8', 'x'), ('int8', '0b1111111111111111111111111111111111111111111111111111111111111111'),
  ('oid', '0'), ('oid', '16384'), ('oid', ' 010 '), ('oid', '0x10'), ('oid', '-1'), ('oid', '-2147483648'),
  ('oid', '4294967295'), ('oid', '+5'), ('oid', ''), ('oid', 'x'), ('oid', '0x'), ('oid', '08'),
  ('oid', '1_0'), ('oid', '1.0'), ('oid', '-'), ('oid', '4294967296'), ('oid', '-2147483649'),
  ('oid', '99999999999999999999999'), ('oid', '0X'), ('oid', '0xg'),
  ('bool', 't'), ('bool', 'true'), ('bool', 'TRUE'), ('bool', ' yes '), ('bool', 'y'), ('bool', 'on'),
  ('bool', 'o'), ('bool', 'of'), ('bool', 'off'), ('bool', '1'), ('bool', '0'), ('bool', 'f'),
  ('bool', 'fa'), ('bool', 'no'), ('bool', 'n'), ('bool', ''), ('bool', '2'), ('bool', 'tru e'),
  ('bool', 'offx'), ('bool', 'truex'),
  ('"char"', 'a'), ('"char"', 'abc'), ('"char"', ''), ('"char"', '\101'), ('"char"', '\377'), ('"char"', '\'),
  ('"char"', '\12'), ('"char"', 'é'),
  ('name', 'abc'), ('name', repeat('n', 63)), ('name', repeat('n', 64)), ('name', repeat('é', 40)),
  ('bytea', '\x'), ('bytea', '\x0a0B'), ('bytea', '\x 0a 0b '), ('bytea', '\x0g'), ('bytea', '\x123'),
  ('bytea', '\X0a'), ('bytea', 'abc'), ('bytea', 'a\\b'), ('bytea', '\141\142'), ('bytea', '\q'),
  ('bytea', '\1'), ('bytea', '\400'), ('bytea', 'é'), ('bytea', '\x0 a'),
  ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'), ('uuid', 'A0EEBC999C0B4EF8BB6D6BB9BD380A11'),
  ('uuid', '{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}'), ('uuid', 'a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11'),
  ('uuid', '{a0eebc999c0b4ef8bb6d6bb9bd380a11'), ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1'),
  ('uuid', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 '), ('uuid', '-a0eebc999c0b4ef8bb6d6bb9bd380a11');
create temp table floats (n serial, t text, i text);
insert into floats (t, i) select t, i from
  (values ('float8'), ('float4')) as types (t),
  (values ('0'), ('-0'), ('1'), ('-1.5'), ('0.1'), ('2.5'), ('100'), ('123456'), ('1234567'),
    ('1e6'), ('1e7'), ('1e14'), ('1e15'), ('1e16'), ('1e22'), ('123456789012345678'), ('0.0001'),
    ('0.00001'), ('1.5e-5'), ('0.000123'), ('3.141592653589793'), ('2.718281828459045'),
    ('1.7976931348623157e308'), ('2.2250738585072014e-308'), ('4.9e-324'), ('5e-324'), ('1e-320'),
    ('3.4028235e38'), ('3.5e38'), ('1.17549435e-38'), ('1.4e-45'), ('1e-46'), ('1e-50'), ('1e39'),
    ('1e309'), ('-1e309'), ('1e-400'), ('0e-400'), ('0x1p-1074'), ('0x1.8p1'), ('0X10'), ('0x'),
    ('NaN'), ('nan'), ('Infinity'), ('-infinity'), ('inf'), ('+Inf'), ('-INF'), ('infinit'),
    (' 1.5 '), ('1.5x'), (''), (' '), ('.5'), ('5.'), ('.'), ('1e'), ('1e+'), ('e5'), ('+-1'),
    ('1_000'), ('9007199254740993'), ('0.30000000000000004'), ('1e-7'), ('123456.7'),
    ('16777217'), ('0.333333333333333333')) as inputs (i);
create temp table numerics (n serial, t text, i text);
insert into numerics (t, i) select 'numeric', i from unnest(array[
  '0', '-0', '0.000', '-0.000', '1', '-1', '+1', ' 12.5 ', '12.50', '.5', '5.', '.', '', ' ', '1e3',
  '1E-3', '1.5e+2', '1e', '1e+', 'e5', '1.2.3', '1_000.000_1', '1_', '_1', '1._5', '1_.5', '1__0',
  '1e_1', '1e1_0', '1.e5', '.e5', '0x1F', '-0x1f', '+0o17', '0b101', '0x_1F', '0x', '0b2', '0x1.5',
  '0x1_', '0xFFFFFFFFFFFFFFFF', '0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF', '-0o777777777777777777777777',
  '0b' || repeat('1', 130), 'NaN', 'nan', '-NaN', '+NaN', ' NaN ', 'Infinity', '-Infinity', 'inf',
  '+INF', '-inf', 'infinit', 'infinityx', 'NaNx', 'abc', '1 2', '- 1', '+-1', '1.5x',
  '123456789012345678901234567890.123456789012345678901234567890', '0.00000000000000000001',
  '1e-20', '1e-16384', '1e131072', '1e1073741823', '1e1073741824', '9999.99995', '0.0001',
  '0.00012345', '100000000', '99999999.99999999', '1e100', '-1.5e-10', '00012.3400',
  '12345678901234567890', '170141183460469231731687303715884105727',
  '-170141183460469231731687303715884105727', '1.70141183460469231731687303715884105727',
  '0.12345678901234567890123456789012345678', '123456789.123456789', '1e38', '1e39', '5e-38',
  '0.000000000000000000000000000000000000001'
]) as i;
insert into numerics (t, i) values
  ('numeric(5,2)', '123.456'), ('numeric(5,2)', '123.455'), ('numeric(5,2)', '-123.455'),
  ('numeric(5,2)', '999.994'), ('numeric(5,2)', '999.995'), ('numeric(5,2)', '0.001'),
  ('numeric(5,2)', '0.005'), ('numeric(5,2)', '-0.005'), ('numeric(5,2)', '-0.004'),
  ('numeric(5,2)', 'NaN'), ('numeric(5,2)', 'Infinity'), ('numeric(5,2)', '-inf'),
  ('numeric(5,2)', '1e2'), ('numeric(5,2)', '12345'), ('numeric(5,2)', 'abc'),
  ('numeric(5,2)', '1e1073741824'), ('numeric(5,2)', '1e-16384'), ('numeric(5,2)', '1e131072'),
  ('numeric(5,2)', '0x10'), ('numeric(5,2)', '0xFFFFFFFF'), ('numeric(5,2)', 'infx'),
  ('numeric(3,-1)', '15'), ('numeric(3,-1)', '14'), ('numeric(3,-1)', '-15'),
  ('numeric(3,-1)', '9994'), ('numeric(3,-1)', '9995'), ('numeric(3,-1)', '0'),
  ('numeric(3,-1)', '4.9'), ('numeric(2,5)', '0.000123'), ('numeric(2,5)', '0.0001234'),
  ('numeric(2,5)', '0.001'), ('numeric(2,5)', '0.000995'), ('numeric(2,5)', '0.000994'),
  ('numeric(1,0)', '0.5'), ('numeric(1,0)', '9.4'), ('numeric(1,0)', '9.5'), ('numeric(1,0)', '-9.5'),
  ('numeric(1,1)', '0.95'), ('numeric(1,1)', '0.94'), ('numeric(1,1)', '-0.04'),
  ('numeric(1,1)', '-0.05'), ('numeric(1000,1000)', '0.5'), ('numeric(10,4)', '9999.99995'),
  ('numeric(8,4)', '9999.99995'), ('numeric(8,4)', '0.00005'), ('numeric(4,-3)', '1234567'),
  ('numeric(4,-3)', '12345678'), ('numeric(4,-3)', '9999499.9'), ('numeric(4,-3)', '9999500'),
  ('numeric(38,0)', '99999999999999999999999999999999999999'), ('numeric(38,0)', '1e38'),
  ('numeric(38,38)', '0.99999999999999999999999999999999999999'), ('numeric(38,38)', '1');
-- The check is in EXECUTE because pg_input_is_valid caches the type when its argument looks
-- stable, and a parameter of a generic plan does.
create function pg_temp.out(t text, i text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
begin
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message || coalesce('' DETAIL '' || detail, '''') || coalesce('' HINT '' || hint, '''') from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  -- A cast of a literal to numeric(p,s) reads the literal with no typmod and then calls the
  -- numeric function, which can fail where the input function with the typmod does not. So the
  -- numeric types call the input function.
  if t like 'numeric%' then
    return numeric_out(numeric_in(i::cstring, 0, to_regtypemod(t)));
  end if;
  -- format calls the output function of the type. A cast to text does not for every type: bool
  -- gives true and not t.
  execute format('select format(''%%s'', %L::%s)', i, t) into r;
  return r;
end
$$;
create function pg_temp.send(t text, i text) returns text language sql
  return encode(numeric_send(numeric_in(i::cstring, 0, to_regtypemod(t))), 'hex');
select t, 1, i, pg_temp.out(t, i) from inputs order by n;
select t, 1, i, pg_temp.out(t, i) from numerics order by n;
select 'send ' || t, 1, i, pg_temp.send(t, i) from numerics where pg_temp.out(t, i) not like 'ERROR %' order by n;
set extra_float_digits = 1;
select t, 1, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 3;
select t, 3, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 0;
select t, 0, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = 2;
select t, 2, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = -3;
select t, -3, i, pg_temp.out(t, i) from floats order by n;
set extra_float_digits = -15;
select t, -15, i, pg_temp.out(t, i) from floats order by n;
-- The date and time types. Each value is read once in ISO and UTC, and the line has the hex of
-- the binary output as the input, so that the test checks the receive function and the output
-- function together. The second field is DateStyle, DateStyle and TimeZone, or IntervalStyle.
reset datestyle;
set timezone = 'UTC';
create temp table dates (n serial, v date);
insert into dates (v) values ('2026-10-05'), ('2000-01-01'), ('1999-12-31'), ('1970-01-01'),
  ('2000-02-29'), ('1900-03-01'), ('0001-01-01'), ('0001-12-31 BC'), ('0044-03-15 BC'),
  ('4714-11-24 BC'), ('9999-12-31'), ('10000-01-01'), ('5874897-12-31'), ('infinity'),
  ('-infinity');
create temp table times (n serial, v time);
insert into times (v) values ('00:00'), ('24:00'), ('12:34:56.789'), ('23:59:59.999999'),
  ('00:00:00.000001'), ('01:02:03.1'), ('10:00:00.120');
create temp table timetzs (n serial, v timetz);
insert into timetzs (v) values ('12:34:56+05:30'), ('00:00-15:59'), ('24:00+15:59:59'), ('12:00+00'),
  ('12:00-05:21:10'), ('23:59:59.5+01'), ('06:00:00.000001-00:00:01');
create temp table stamps (n serial, v timestamp);
insert into stamps (v) values ('2026-10-04 12:34:56.789'), ('2026-10-05 00:00'), ('2026-10-06 01:00'),
  ('2026-10-07 23:59:59.999999'), ('2026-10-08 00:00:00.000001'), ('2026-10-09 10:00:00.5'),
  ('2026-10-10 12:00'), ('2000-01-01 00:00'), ('1999-12-31 23:59:59.5'), ('4714-11-24 00:00 BC'),
  ('0044-03-15 12:00 BC'), ('0001-01-01 00:00'), ('1900-01-01 00:00'), ('10000-01-01 00:00'),
  ('294276-12-31 23:59:59.999999'), ('infinity'), ('-infinity');
create temp table stamptzs as select n, v::timestamptz as v from stamps;
create temp table intervals (n serial, v interval);
insert into intervals (v) values ('0'), ('1 year 2 months 3 days 4:05:06'),
  ('-1 year -2 months +3 days -4:05:06'), ('1 day'), ('-1 day'), ('1 sec'), ('-1 sec'), ('1.5 sec'),
  ('-0.5 sec'), ('0.000001 sec'), ('1 mon'), ('-1 mon'), ('1 year'), ('2 years'), ('-1 year'),
  ('25 hours'), ('-25:00:00.000001'), ('1 min'), ('-1 min'), ('1 hour 1 min'), ('3 days 0:00:01'),
  ('1 day -1 sec'), ('-1 day +1 sec'), ('1 year -1 day'), ('-1 year 1 day'), ('1 mon 1 day 00:00:00.5'),
  ('-1 mon -1 day -00:00:00.5'), ('178956970 years 7 months'), ('-178956970 years -8 months'),
  ('2147483647 days'), ('-2147483648 days'), ('2562047788 hours'), ('-2562047788 hours'),
  ('1 year 1 sec'), ('-1 sec 1 year'), ('1 day 1 sec'), ('-1 day -1 sec'), ('10 days -10 sec'),
  ('infinity'), ('-infinity');
create function pg_temp.datetimes() returns table (t text, s text, i text, o text)
language plpgsql as $$
declare
  ds text;
  tz text;
  st text;
begin
  foreach ds in array array['ISO, MDY', 'ISO, DMY', 'SQL, MDY', 'SQL, DMY', 'SQL, YMD',
    'Postgres, MDY', 'Postgres, DMY', 'Postgres, YMD', 'German, DMY'] loop
    perform set_config('datestyle', ds, false);
    return query select 'date', ds, encode(date_send(v), 'hex'), v::text from dates order by n;
    return query select 'timestamp', ds, encode(timestamp_send(v), 'hex'), v::text from stamps order by n;
    foreach tz in array array['UTC', '<+05:30>-05:30', '+05:30'] loop
      perform set_config('timezone', tz, false);
      return query select 'timestamptz', ds || '|' || tz, encode(timestamptz_send(v), 'hex'), v::text
        from stamptzs order by n;
    end loop;
    perform set_config('timezone', 'UTC', false);
  end loop;
  perform set_config('datestyle', 'ISO, MDY', false);
  return query select 'time', 'ISO, MDY', encode(time_send(v), 'hex'), v::text from times order by n;
  return query select 'timetz', 'ISO, MDY', encode(timetz_send(v), 'hex'), v::text from timetzs order by n;
  foreach st in array array['postgres', 'postgres_verbose', 'sql_standard', 'iso_8601'] loop
    perform set_config('intervalstyle', st, false);
    return query select 'interval', st, encode(interval_send(v), 'hex'), v::text from intervals order by n;
  end loop;
  perform set_config('intervalstyle', 'postgres', false);
end
$$;
select * from pg_temp.datetimes();
-- The text input of the date and time types. The type field is "in", the type and the typmod.
-- The output is the hex of the binary form or the error. The second field is DateStyle and
-- TimeZone, or IntervalStyle. The inputs leave out now, today, zone names and the abbreviations
-- whose offset changed over time, because they need the clock or the tz database.
create temp table stampins (n serial, i text);
insert into stampins (i) select unnest(array[
  '2001-02-03', '2001-02-03 04:05:06', '2001-02-03 04:05:06.789', '2001-02-03T04:05:06Z',
  '2001-02-03 04:05:06+05', '2001-02-03 04:05:06-08:00', '2001-02-03 04:05:06 PST',
  '2001-02-03 04:05:06 PDT', '2001-02-03 04:05:06 PST DST', '2001-02-03 04:05:06 dst',
  '2001-02-03 04:05:06 +0530', '2001-02-03 04:05:06 +05:30:15', '2001-02-03 04:05:06+16',
  '2001-02-03 04:05:06 -1500', '2001-02-03 04:05:06 -15:59:59', '2001-02-03 04:05:06 xyz',
  '2001-02-03 foo/bar', '2001-02-03 04:05:06 +05 PST', '2001-02-03 04:05:06 z',
  '2001-02-03 04:05:06Z', '2001-02-03 04:05:06 zulu', '2001-02-03 allballs', 'Feb 3 2001',
  'February 3, 2001', '3 Feb 2001', '2001 Feb 3', 'Feb 3', '01/02/03', '1/2/2003', '02/01/2003',
  '2003/01/02', '1/2/3', '1-2-3', '12/31/99', '31/12/99', '99/12/31', '70-01-01', '69-01-01',
  '2001-2-3', 'Feb-03-2001', '03-Feb-2001', '2001-Feb-03', '2001.02.03', '2001.034', '2001 034',
  '20010203', '010203', '010203 040506', '20010203 0405', '20010203T040506', '2001-02-03T04:05:06',
  '2001-02-03t040506', '2001-02-03 t 04:05', '2001-02-03 040506-08', '2001-02-03 04',
  '2001-02-03 0405', 'J2451187', 'j2451187.25', 'j 2451187-08', 'julian 2451187', 'j -1',
  'J2451187 04:05', '1999-01-08 04:05:06 BC', '0001-01-01 BC', '0001-01-01 AD', '0000-01-01',
  '-2001-01-01', 'epoch', 'infinity', '-infinity', '+infinity', ' Infinity ', 'allballs',
  '2001-02-30', '2001-02-29', '2000-02-29', '2001-13-01', '2001-00-10', '2001-02-00', '2001-02-32',
  '13/13/2001', '2001-02-03 24:00', '2001-02-03 24:00:01', '2001-02-03 25:00', '2001-02-03 12:60',
  '2001-02-03 23:59:60', '2001-02-03 23:59:60.5', '2001-02-03 4pm', '2001-02-03 4:05 PM',
  '2001-02-03 12:00 AM', '2001-02-03 12:30 pm', '2001-02-03 13:00 PM', '2001-02-03 04:05:06.123456789',
  '2001-02-03 04:05:06.9999995', '2001-02-03 4:5:6', '2001-02-03 04:05', '2001-02-03 at 04:05',
  '2001-02-03 on 04:05', 'Saturday 2001-02-03', 'Sat, 03 Feb 2001 04:05:06 GMT',
  'Sat Feb 03 04:05:06 2001 UTC', 'Sat Feb 03 04:05:06.7 2001 PST', 'Feb 03 04:05:06 2001',
  '2001-02-03 04:05:06.5 +05:30 bc', '99999-01-01', '294276-12-31 23:59:59', '294277-01-01',
  '5874897-12-31', '5874898-01-01', '4714-11-24 BC', '4714-11-23 BC', '2001-02-03 04:05:06 ago',
  '', ' ', '3', 'garbage', '2001-02-03 garbage', '2001-02-03 é', '1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26',
  '2001-02-03 04:05:06.' || repeat('1', 110), '2001-02-03 04:05:06.' || repeat('1', 140),
  '2001-02-03 99999999999:00', '2001-02-03 04:05:99999999999', '2001-02-03 04:05:06 +99999999999',
  '99999999999-01-01', '2001-02-03 04:05:06 dow', '2001-02-03 04:05:06 sun', 'monday feb 3 2001',
  'jan 1 feb 2001', '1 jan feb', 'y2001m02d03', '2001-02-03 h 04', '2001-02-03 4:05:06.', '2001-02-03 4:05.5',
  '2001 02 03', '02 03 2001', '03 02 2001', '1.2.3', '2001.02.03.04', '2001-02-03-04', '2001/02/03/04'
]);
create temp table timeins (n serial, i text);
insert into timeins (i) select unnest(array[
  '04:05', '04:05:06', '04:05:06.789', '04:05:06.7891234', '040506', '0405', '040506.5', '04:05 PM',
  '12:00 AM', '12:00 PM', '13:00 PM', '24:00', '24:00:01', '23:59:60', '23:59:60.5', '04:05:06 PST',
  '04:05:06-08', '04:05:06+05:30', '04:05:06 +0530', '04:05-0800', '04:05:06 PST DST', '04:05:06 dst',
  '2001-02-03 04:05:06', '2001-02-03 04:05:06 PST', '04:05:06 2001-02-03', '04:05:06 PDT',
  'allballs', 'z', '04:05 z', '04:05 zulu', 'epoch', '', 'abc', '25:00', '-01:00', '1:2:3',
  '04:05:06 foo/bar', 'T04:05:06', 't040506', 'j2451187 04:05', '04:05:06.999999999',
  '23:59:59.9999999', '04:05:06 BC', '4', '04', '1.5', '04:05:06 xyz', '040506-08', '2001.034 04:05',
  '04:05:06 +16', '04:05:06 -15:59:59', '4 pm', '12 am', '99999999999:00', '04:05:06 jan',
  '04:05:06 mon', 'yesterday', 'infinity', '04:05:06.', '0405.5', '04:05:06 2001-02-03 PST'
]);
create temp table intervalins (n serial, i text);
insert into intervalins (i) select unnest(array[
  '1 day', '1 day 2 hours', '@ 1 day 2 hours ago', '1 year 2 months 3 days 04:05:06', '1-2',
  '1-2 3 4:05:06', '-1-2 3 4:05:06', '-1-2 +3 -4:05:06', '1-12', '-1 2:03:04', '+1 -2:03:04',
  '1 day -2:03:04', '-1 day 2:03:04', '- 1 day', '1.5 years', '1.5 months', '1.5 weeks', '1.5 days',
  '1.5 hours', '1.5 min', '0.5 sec', '1.0000005 sec', '1 millisecond', '1 microsecond', '1.5 ms',
  '1.5 us', '1 decade', '1 century', '1 millennium', '1.5 centuries', '1 quarter', '2 qtr',
  '1 year ago', 'ago', '1 day ago 2 hours', 'infinity', '-infinity', '1 day infinity',
  'infinity 1 day', 'epoch', '', ' ', '1', '1.5', '-1', '1:2', '1:02:03.5', '1:2.5', '100:00:00',
  '-100:00:00', '2147483647 days', '2147483648 days', '-2147483648 days', '178956970 years',
  '178956971 years', '-178956970 years -8 months', '178956970 years 7 months 1 mon',
  '9223372036854775807 microseconds', '9223372036854775808 us', '-9223372036854775808 us',
  '2562047788 hours', '2562047789 hours', '153722867280 min', '1 day 1 day', '1 hour 1 hour',
  '1 sec 1 ms', '1.5 sec 1 ms', '1 sec 1.5 ms', 'P1Y2M3DT4H5M6S', 'P1Y', 'PT1H', 'P1W', 'P0.5Y',
  'P1.5M', 'P1.5W', 'P1.5D', 'PT1.5S', 'PT1.5H', 'P-1Y-2M', 'PT-1.5H', 'P1Y-2M',
  'P0001-02-03T04:05:06', 'P0001-02-03', 'P0001-02', 'P0001', 'P0001-02-03T04', 'P0001-02-03T04:05',
  'P0001-02T04:05:06', 'P0001T04:05:06', 'P00010203T040506', 'P00010203', 'PT040506', 'PT04:05:06',
  'PT0405', 'PT04:05:06.5', 'P00010203T04:05:06', 'P0001-02-03T040506', 'P1Y2M3DT4H5M6.5S', 'P',
  'PT', 'P1', 'P1X', 'p1y', 'P1e2Y', 'PT1e400S', 'P1.5e15Y', 'P1e15D', 'PT.5S', 'P1Y2Y', 'P1YT',
  'P1DT1H1M1S1', 'P1Y1Y', 'PT1H1H', 'P0001-02-03-04', 'P1Y-', 'P--1Y', 'P1Y 2M', ' P1Y',
  '1 week 2 days', '1 mon 2 mon', '3 4:05:06', '3 4:05', '4:05:06.789',
  '1 year 2 mons -3 days +04:05:06.789', '1 day 25:00:00', '1 2', '1 hour 30', '1 2 3', '5 dow',
  '1 day 2 h 3 m 4 s', '1 d', '1 y 2 m', '1 days days', 'day', '1 day 2:03:04 5', '1-2-3',
  '1 timezone', '1:2:3:4', '1.5:00', '1 hour 2:03', '2:03 1 hour', '-2:03:04.5', '+2:03:04.5',
  '1.' || repeat('9', 300), '0.1 microsecond', '0.5 microsecond', '-0.5 microsecond',
  '1 century 1 decade 1 millennium', '1 years 1 decade', '7 days 1 week', '1 week 7 days',
  '12 months 1 year', '1 @ day', '@ 1 @ day', '1 day @', '1 day ago ago', '1 day 2 hours ago'
]);
create function pg_temp.dtin(t text, i text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
  name text := (select typname from pg_type where oid = to_regtype(t));
begin
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message || coalesce('' DETAIL '' || detail, '''') || coalesce('' HINT '' || hint, '''') from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  if name = 'date' then
    execute format('select encode(date_send(date_in(%L::cstring)), ''hex'')', i) into r;
  else
    execute format('select encode(%s_send(%s_in(%L::cstring, 0, %s)), ''hex'')', name, name, i,
      to_regtypemod(t)) into r;
  end if;
  return r;
end
$$;
create function pg_temp.dtins() returns table (t text, s text, i text, o text)
language plpgsql as $$
declare
  ds text;
  tz text;
  ty text;
  st text;
begin
  foreach ds in array array['ISO, MDY', 'ISO, DMY', 'ISO, YMD'] loop
    perform set_config('datestyle', ds, false);
    foreach tz in array array['UTC', '<+05:30>-05:30', '+05:30'] loop
      perform set_config('timezone', tz, false);
      foreach ty in array array['date', 'timestamp', 'timestamptz'] loop
        return query select 'in ' || ty || ' -1', ds || '|' || tz, x.i, pg_temp.dtin(ty, x.i)
          from stampins x order by n;
      end loop;
      foreach ty in array array['time', 'timetz'] loop
        return query select 'in ' || ty || ' -1', ds || '|' || tz, x.i, pg_temp.dtin(ty, x.i)
          from timeins x order by n;
      end loop;
    end loop;
  end loop;
  perform set_config('datestyle', 'ISO, MDY', false);
  perform set_config('timezone', '<+05:30>-05:30', false);
  foreach ty in array array['timestamp(0)', 'timestamp(2)', 'timestamptz(0)', 'timestamptz(5)',
    'time(0)', 'time(3)', 'timetz(0)', 'timetz(1)'] loop
    return query select 'in ' || (select typname from pg_type where oid = to_regtype(ty)) || ' '
      || to_regtypemod(ty), 'ISO, MDY|<+05:30>-05:30', x.i, pg_temp.dtin(ty, x.i)
      from unnest(array['2001-02-03 04:05:06.5', '2001-02-03 04:05:06.49', '2001-02-03 23:59:59.999999',
        '2001-02-03 04:05:06.4999995', '04:05:06.123456', 'infinity', '294276-12-31 23:59:59.9']) x (i);
  end loop;
  perform set_config('timezone', 'UTC', false);
  foreach st in array array['postgres', 'sql_standard'] loop
    perform set_config('intervalstyle', st, false);
    return query select 'in interval -1', st, x.i, pg_temp.dtin('interval', x.i)
      from intervalins x order by n;
  end loop;
  perform set_config('intervalstyle', 'postgres', false);
  foreach ty in array array['interval year', 'interval month', 'interval day', 'interval hour',
    'interval minute', 'interval second', 'interval year to month', 'interval day to hour',
    'interval day to minute', 'interval day to second', 'interval hour to minute',
    'interval hour to second', 'interval minute to second', 'interval(2)', 'interval second(1)',
    'interval day to second(0)'] loop
    return query select 'in interval ' || to_regtypemod(ty), 'postgres', x.i, pg_temp.dtin(ty, x.i)
      from unnest(array['5', '1.5', '1-2', '1 2', '1:2', '1:2:3', '1 2:3', '1 2:3:4.5678',
        '1 year 2 months 3 days 4 hours 5 minutes 6.789 seconds', '-1 2:3:4.5', 'infinity',
        'P1Y2M3DT4H5M6.789S', '1.2345 sec', '1 2 3']) x (i);
  end loop;
end
$$;
select * from pg_temp.dtins();
-- The string types and json. The type is "in", the name in pg_type and the typmod, and the output
-- is the hex of the binary output. "inhex" has the hex of the input, for an input with a control
-- character. "coerce" is the length cast with the typmod and t for an explicit cast.
create temp table strins (n serial, i text);
insert into strins (i) values (''), (' '), ('    '), ('a'), ('ab'), ('abc'), ('abcd'), ('abc '),
  ('abc  '), ('ab  '), ('a   b'), ('abc d'), ('ééé'), ('éééé'), ('ééé  '), ('éé'), ('é é'),
  (E'abc\t'), (E'ab\n'), ('日本語'), ('日本語x'), ('日本 ');
create temp table jsonins (n serial, i text);
insert into jsonins (i) values ('1'), ('-0'), ('0'), ('1.5e10'), ('-1.5E-10'), ('1e+5'), ('0.5'),
  ('true'), ('false'), ('null'), ('"abc"'), ('""'), ('"é"'), ('"😀"'), ('"\u0000"'),
  ('"\ud800"'), ('"\uDC00x"'), ('[]'), ('{}'), ('[1,2,3]'), ('{"a":1,"b":[true,null,{"c":"d"}]}'),
  (' [ 1 , 2 ] '), ('{"a":1,"a":2}'), ('"é"'), ('"\/"'), ('"\b\f\n\r\t\"\\"'), (E'[1,\n2]'),
  (E'\t{}\r\n'), ('"a b"'), ('[[[[[]]]]]'), ('{"":{"":{}}}'), ('-0.0e-0'), ('"ኯ"'), (''),
  (' '), ('01'), ('-'), ('-a'), ('1.'), ('.5'), ('1.e5'), ('1e'), ('1e+'), ('+1'), ('tru'),
  ('True'), ('nul'), ('falsey'), ('undefined'), ('NaN'), ('Infinity'), ('-Infinity'), ('['),
  (']'), ('[1'), ('[1,'), ('[1,]'), ('[,1]'), ('[1 2]'), ('{'), ('{"a"'), ('{"a":'), ('{"a":1'),
  ('{"a":1,'), ('{"a" 1}'), ('{a:1}'), ('{1:2}'), ('{"a":1,}'), ('{"a":1 "b":2}'), ('{,}'),
  ('"abc'), ('"abc\'), ('"\x"'), ('"\u12"'), ('"\u12g4"'), ('"\uzzzz"'), (E'"a\tb"'), (E'"\n"'),
  (E'"\x01"'), ('"\é"'), ('1 2'), ('[] []'), ('{}}'), ('[]]'), ('é'), ('1é'), ('"a"b'),
  ('[1]x'), ('1.5.3'), ('#'), ('[#]'), ('"\'), ('/'), ('{"a":1}garbage'), ('nullx'), (' null '),
  ('[-]'), ('[1,-]'), ('{"a":tru}'), ('"\u"'), ('"\u00"'), ('"abc\"'), ('123abc'), ('[1e5x]'),
  ('"\q"'), ('{"a":[1,2}'), ('[{"a":1]'), ('1e5_'), ('{"a":}'), ('{"a"::1}'), ('[:]'), ('{]'),
  ('[}'), ('"日本"'), ('"\日"'), ('日本'), ('{"a":1}}'), ('1-2'), ('-01'), ('00'), ('1E5'),
  ('[1,2,]'), ('{"a"}'), ('"a""b"'), ('[true false]'), ('{"a":1 , "b" : [ ] }'), ('\'), ('"'),
  ('[,]'), (','), (':'), ('{"a":1,"b"}'), ('{"a",}');
create function pg_temp.strin(t text, i text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
  ty pg_type := (select p from pg_type p where oid = to_regtype(t));
begin
  -- A plain call keeps the type of the first call in its plan, so the checks use execute.
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message || coalesce('' DETAIL '' || detail, '''') || coalesce('' HINT '' || hint, '''') from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  if ty.typinput::text in ('json_in', 'textin') then
    execute format('select encode(%s(%s(%L::cstring)), ''hex'')', ty.typsend, ty.typinput, i) into r;
  else
    execute format('select encode(%s(%s(%L::cstring, %s, %s)), ''hex'')', ty.typsend, ty.typinput,
      i, ty.oid, to_regtypemod(t)) into r;
  end if;
  return r;
end
$$;
create function pg_temp.coerce(t text, i text, explicit boolean) returns text language plpgsql as $$
declare
  r text;
  name text := (select typname from pg_type where oid = to_regtype(t));
begin
  execute format('select encode(%s(%I(%L::%s, %s, %L)), ''hex'')', name || 'send', name, i, name,
    to_regtypemod(t), explicit) into r;
  return r;
exception when others then
  return 'ERROR ' || sqlstate || ' ' || sqlerrm;
end
$$;
create function pg_temp.strins() returns table (t text, s text, i text, o text)
language plpgsql as $$
declare
  ty text;
  ex boolean;
begin
  foreach ty in array array['text', 'varchar', 'varchar(1)', 'varchar(3)', 'bpchar', 'character(1)',
    'character(3)'] loop
    return query select case when x.i ~ '[\x01-\x1f]' then 'inhex ' else 'in ' end
      || (select typname from pg_type where oid = to_regtype(ty)) || ' ' || to_regtypemod(ty), '',
      case when x.i ~ '[\x01-\x1f]' then encode(convert_to(x.i, 'UTF8'), 'hex') else x.i end,
      pg_temp.strin(ty, x.i) from strins x order by n;
  end loop;
  foreach ty in array array['varchar(1)', 'varchar(3)', 'character(1)', 'character(3)'] loop
    foreach ex in array array[true, false] loop
      return query select 'coerce ' || (select typname from pg_type where oid = to_regtype(ty)) || ' '
        || to_regtypemod(ty) || ' ' || ex, '', x.i, pg_temp.coerce(ty, x.i, ex)
        from strins x where x.i !~ '[\x01-\x1f]' order by n;
    end loop;
  end loop;
  return query select case when x.i ~ '[\x01-\x1f]' then 'inhex ' else 'in ' end || 'json -1', '',
    case when x.i ~ '[\x01-\x1f]' then encode(convert_to(x.i, 'UTF8'), 'hex') else x.i end,
    pg_temp.strin('json', x.i) from jsonins x order by n;
end
$$;
select * from pg_temp.strins();
-- Arrays, int2vector and oidvector. The output is the hex of the binary form, a space and the
-- text form. An input or an output with a control character is in hex, and the type starts with
-- arrayhex or vectorhex.
create temp table arrins (n serial, ty text, i text, nulls text default 'on');
insert into arrins (ty, i) select ty, i from unnest(array['int4[]', 'text[]']) ty,
  unnest(array['{}', '{ }', '  {}  ', '{{}}', '{{},{}}', '{1,2,3}', '{ 1 , 2 , 3 }', '{1,NULL,3}',
  '{1,null,3}', '{NULL}', '{"NULL"}', '{"1"}', '{ "1" , "2" }', '{{1,2},{3,4}}', '{{1,2},{3}}',
  '{{1},2}', '{1,{2}}', '{{1}{2}}', '{{1},{2},{3}}', '[0:2]={1,2,3}', '[1:3]={1,2,3}',
  '[-2:-1][3:4]={{1,2},{3,4}}', '[1:2]={1}', '[1:2]={{1},{2}}', '[1:1][1:1]={1}', '[2:1]={}',
  '[1:2147483647]={1}', '[-2147483648:2147483646]={1}', '[1:99999999999]={1}', '[+1:+2]={1,2}',
  '[ 1]={1}', '[1 ]={1}', '[1]={1}', '[1] = {1}', ' [1] [1] = {{1}}', '[1]{1}', '[1]= 1', '[x]={1}',
  '[1:]={1}', '[:1]={1}', '[1:2', '[1:2]', '[1][1][1][1][1][1][1]={{{{{{{1}}}}}}}',
  '{{{{{{1}}}}}}', '{{{{{{{1}}}}}}}', '{1', '{1,', '{"1', '{"1\', '{1\', '{1,}', '{,1}', '{1,,2}',
  '{1 2}', '{"1" 2}', '{"1""2"}', '{1"2"}', '{"1"x}', '{"1" }', '{1} x', '{1}  ', '1', '', ' ',
  'x{1}', '{\1}', '{NULL,"a b"}', '{"",x}', '{"a\"b"}', '{a\,b}', '{a\\b}', '{ a b }', '{"a}',
  '{a}b}', '{a{b}', '{\NULL}', '{"\NULL"}', '{NULL }', '{ NULL}', '{nul}', '{NULLx}',
  E'{1,\t2}', E'{a\tb}', E'{"a\nb"}', E'\n{1}\n', E'{1\x0b}', E'[1]=\f{1}', '{é,"日本",a é}']) i;
insert into arrins (ty, i, nulls) select 'text[]', i, 'off' from unnest(array['{NULL}',
  '{null, NuLl ,"NULL",\NULL}', '{NULL,a}', '{}']) i;
insert into arrins (ty, i) values ('int2[]', '{1,-32768,32767}'), ('int2[]', '{32768}'),
  ('int2[]', '{ 1 , x }'), ('int8[]', '{9223372036854775807,-9223372036854775808}'),
  ('int8[]', '{0x10,1_000}'), ('oid[]', '{1,4294967295,-1}'), ('oid[]', '{4294967296}'),
  ('float8[]', '{1.5,-0,Infinity,-inf,NaN,1e308,0.1,4.9e-324}'), ('float8[]', '{1e400}'),
  ('bool[]', '{t,f,true,FALSE,yes,0, on }'), ('bool[]', '{maybe}'),
  ('numeric(5,2)[]', '{1.234,999.994,NaN,-0}'), ('numeric(5,2)[]', '{999.995}'),
  ('numeric[]', '{1e-20,-0.00,Infinity}'), ('varchar(3)[]', '{a,abc,"abc  ",abc   }'),
  ('varchar(3)[]', '{a,abcd}'), ('varchar[]', '{abcdef}'), ('character(3)[]', '{a,"",abc}'),
  ('character(3)[]', '{abcd}'), ('bpchar[]', '{"a  ",""}'), ('"char"[]', '{a,ab,"",\\,"\""}'),
  ('name[]', '{abc,"a b",""}'), ('bytea[]', '{"\\x0102",abc,""}'), ('bytea[]', '{"\\x0"}'),
  ('uuid[]', '{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11,{a0eebc999c0b4ef8bb6d6bb9bd380a11}}'),
  ('uuid[]', '{"{a0eebc999c0b4ef8bb6d6bb9bd380a11}"}'), ('uuid[]', '{x}'),
  ('json[]', '{"{\"a\": 1}","[1, 2]",null,"null","\"s\""}'), ('json[]', '{"{"}'),
  ('int2vector', ''), ('int2vector', ' '), ('int2vector', '1'), ('int2vector', '1 2 3'),
  ('int2vector', ' 1  2 '), ('int2vector', '-32768 32767'), ('int2vector', '32768'),
  ('int2vector', '1,2'), ('int2vector', '1 x'), ('int2vector', 'x'), ('int2vector', '+5 -0'),
  ('int2vector', E'1\t2'), ('int2vector', E'\t1\n'), ('int2vector', '0x10'),
  ('int2vector', '99999999999999999999'), ('int2vector', '1 99999999999999999999 x'),
  ('oidvector', ''), ('oidvector', '1 2'), ('oidvector', '1-2'), ('oidvector', '0x10 010'),
  ('oidvector', '4294967296'), ('oidvector', '-2147483649'), ('oidvector', '1 x'),
  ('oidvector', ' 3 '), ('oidvector', '1,2'), ('oidvector', '18446744073709551616 1');
create function pg_temp.arr(t text, i text, nulls text) returns text language plpgsql as $$
declare
  valid boolean;
  r text;
begin
  perform set_config('array_nulls', nulls, true);
  perform set_config('extra_float_digits', '1', true);
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select ''ERROR '' || sql_error_code || '' '' || message || coalesce('' DETAIL '' || detail, '''') || coalesce('' HINT '' || hint, '''') from pg_input_error_info(%L, %L)', i, t)
      into r;
    return r;
  end if;
  execute format('select encode(%s(%L::%s), ''hex'') || '' '' || %L::%s::text',
    (select typsend from pg_type where oid = to_regtype(t)), i, t, i, t) into r;
  return r;
end
$$;
select case when v.vector then 'vector' else 'array' end || case when h.hex then 'hex ' else ' ' end
    || case when v.vector then t.typname else e.typname || ' ' || to_regtypemod(a.ty) end,
  a.nulls,
  case when h.hex then encode(convert_to(a.i, 'UTF8'), 'hex') else a.i end,
  case when h.hex then encode(convert_to(o.r, 'UTF8'), 'hex') else o.r end
from arrins a
join pg_type t on t.oid = to_regtype(a.ty)
join pg_type e on e.oid = t.typelem
cross join lateral (select t.typname in ('int2vector', 'oidvector') as vector) v
cross join lateral (select pg_temp.arr(a.ty, a.i, a.nulls) as r) o
cross join lateral (select a.i ~ '[\x01-\x1f]' or o.r ~ '[\x01-\x1f]' as hex) h
order by a.n;
-- The names in the error of array_recv when the element type is not the expected type.
select 'format', '', x::text, format_type(x, null) from (select oid from pg_type where oid < 10000
  union all values (0::oid), (2), (9999)) s(x) order by x;
-- The OID alias types. A number or a dash needs no lookup, and the output of an OID with no object
-- is the number. A name needs the catalog, so a line has ERROR NAME for the error of a name.
create function pg_temp.reg(t text, i text) returns text language plpgsql as $$
declare
  r text;
  c text;
  m text;
begin
  execute format('select %L::%s::text', i, t) into r;
  return r;
exception when others then
  get stacked diagnostics c = returned_sqlstate, m = message_text;
  return 'ERROR ' || c || ' ' || m;
end
$$;
select 'reg ' || t, '', i, case when r like 'ERROR %' and r not like '%type oid%' then 'ERROR NAME' else r end
from unnest(array['regproc', 'regprocedure', 'regoper', 'regoperator', 'regclass', 'regtype',
  'regrole', 'regnamespace', 'regcollation', 'regconfig', 'regdictionary', 'regdatabase']) t,
  unnest(array['-', '0', '8', '010', '0x10', '09', '4294967295', '4294967296', '3000000000',
  '99999999999999999999', '00000000000000000008', '', ' ', ' 8', '8 ', '+8', '-8', '1e3', '--',
  '٣', '0b1']) i,
  lateral (select pg_temp.reg(t, i) as r) o
order by t, i;
-- The name lists of regclass. The error of a name that is not found has the parts with a dot
-- between them. One or two parts give rel, three parts give db, and more parts give many.
select 'names', '', i, case c
    when '42P01' then 'rel ' || substring(m from '^relation "(.*)" does not exist$')
    when '0A000' then 'db ' || substring(m from '^cross-database references are not implemented: "(.*)"$')
    when '42601' then 'many ' || substring(m from '^improper relation name \(too many dotted names\): (.*)$')
    else 'ERROR ' || c || ' ' || m end
from unnest(array['zq', 'Zq', 'zq . "Q""d"', '"Zq"."x y"', 'zq.b.c', 'zq.b.c.d', 'zq.b.c.d.e',
  '""', '"".zq', 'zq.', '.zq', 'zq..b', 'zq b', '"zq', '"zq"b', 'zq"b', 'ÉZQ.b', '  zq  .  b  ',
  '', ' ', repeat('X', 70), '"' || repeat('Y', 70) || '"', repeat('q', 62) || 'é', 'zq,b',
  'zq."b.c"', '"zq"""', '"zq"".b"', 'pg_catalog.zq', 'zq.pg_class', '"zq" . "b" . "c"', 'zq. ',
  'ÉÀ', '"日本"', '日本.Zq']) with ordinality u(i, n),
  lateral (select pg_temp.reg('regclass', i) as r) o,
  lateral (select substring(o.r from '^ERROR (\S+) ') as c, substring(o.r from '^ERROR \S+ (.*)$') as m) e
order by n;
