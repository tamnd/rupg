-- The static rows of the system catalogs on PostgreSQL 19 at the pin, for tests/oracle.rs. Run it with psql -X -q on a cluster that initdb made with the C locale, the encoding UTF8 and the superuser postgres, and write the output to tests/oracle.tsv.
-- Each line is the catalog name and the columns of one row in the text form of COPY. A row is static if the first column is below FirstUnpinnedObjectId, 12000. A regproc column is written as its OID.
SELECT format('COPY (SELECT %L, %s FROM pg_catalog.%I WHERE %s::oid < 12000) TO STDOUT',
              c.relname,
              string_agg(CASE WHEN t.typname LIKE 'reg%' THEN quote_ident(a.attname) || '::oid' ELSE quote_ident(a.attname) END, ', ' ORDER BY a.attnum),
              c.relname,
              (array_agg(quote_ident(a.attname) ORDER BY a.attnum))[1])
FROM pg_class c
JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
JOIN pg_type t ON t.oid = a.atttypid
WHERE c.relnamespace = 'pg_catalog'::regnamespace
  AND c.relname IN ('pg_proc', 'pg_type', 'pg_class', 'pg_operator', 'pg_opfamily', 'pg_opclass', 'pg_am', 'pg_amop', 'pg_amproc', 'pg_language', 'pg_aggregate', 'pg_description', 'pg_cast', 'pg_namespace', 'pg_conversion', 'pg_database', 'pg_tablespace', 'pg_authid', 'pg_auth_members', 'pg_shdescription', 'pg_ts_config', 'pg_ts_config_map', 'pg_ts_dict', 'pg_ts_parser', 'pg_ts_template', 'pg_collation', 'pg_range')
GROUP BY c.relname
ORDER BY c.relname
\gexec
