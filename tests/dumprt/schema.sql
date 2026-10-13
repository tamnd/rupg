-- The schema of the pg_dump round trip of spec/21 section 21.4.1. The test runs each statement on rupg and on PostgreSQL. See README.md.

CREATE SCHEMA app;
CREATE TABLE app.customer (id serial PRIMARY KEY, name text NOT NULL, email varchar(200) UNIQUE, joined date DEFAULT '2020-01-01', score numeric(10,2) CHECK (score >= 0));
CREATE TABLE app.orders (id bigserial PRIMARY KEY, customer_id int NOT NULL REFERENCES app.customer (id) ON DELETE CASCADE, amount numeric(12,2), status char(1) DEFAULT 'n', note text COLLATE "C");
CREATE INDEX orders_customer_idx ON app.orders (customer_id);
CREATE UNIQUE INDEX orders_note_idx ON app.orders (lower(note)) WHERE status = 'y';
CREATE VIEW app.big AS SELECT c.name, sum(o.amount) AS total FROM app.customer c JOIN app.orders o ON o.customer_id = c.id GROUP BY c.name HAVING sum(o.amount) > 100;
COMMENT ON TABLE app.customer IS 'the customers';
COMMENT ON COLUMN app.customer.name IS 'the full name';
CREATE TABLE plain (a int, b text[], c jsonb, d timestamptz, e inet, f bool NOT NULL DEFAULT true);
CREATE VIEW rec AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r;
CREATE TABLE app.item (id int PRIMARY KEY, code text NOT NULL, kind smallint, price numeric CONSTRAINT price_positive CHECK (price > 0), CONSTRAINT item_code_kind UNIQUE (code, kind));
CREATE TABLE app.line (order_id bigint REFERENCES app.orders ON UPDATE CASCADE, item_id int REFERENCES app.item, qty int DEFAULT 1 NOT NULL, PRIMARY KEY (order_id, item_id));
CREATE TABLE app.scratch (k text, v bytea, u uuid, t time(3), iv interval, js json DEFAULT '{}', r real, d double precision, ts timestamp(0), c char(3), n name, o oid, i2 int2[], m money, x8 xid8, o8 oid8, ti tid, rc refcursor, ma money[]);
CREATE INDEX line_qty_idx ON app.line USING btree (qty DESC NULLS LAST);
CREATE INDEX scratch_k_idx ON app.scratch USING hash (k);
CREATE INDEX scratch_kv_idx ON app.scratch (k) INCLUDE (u);
COMMENT ON CONSTRAINT price_positive ON app.item IS 'more than zero';
CREATE VIEW app.mixed AS SELECT i.code, CASE WHEN i.price > 10 THEN 'high' ELSE 'low' END AS band, i.price::int AS whole FROM app.item i WHERE i.kind IN (1, 2) ORDER BY i.code;
CREATE VIEW app.sub AS SELECT c.id, (SELECT count(*) FROM app.orders o WHERE o.customer_id = c.id) AS n, EXISTS (SELECT 1 FROM app.line l WHERE l.qty > 5) AS big FROM app.customer c LEFT JOIN app.orders o2 ON o2.customer_id = c.id;
COMMENT ON INDEX app.orders_customer_idx IS 'by customer';
COMMENT ON SCHEMA app IS 'the app';
COMMENT ON VIEW rec IS 'one to five';
