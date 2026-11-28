-- MirrorSelf index, schema v6: transactions and dated items join the
-- timeline. v_timeline keeps its columns; it only gains rows of two new
-- kinds ('transaction' and 'item'), which callers can filter by type or
-- category.

DROP VIEW v_timeline;

CREATE VIEW v_timeline AS
SELECT 'event' AS kind, e.id, e.type, e.category_id AS category, e.start_ts, e.end_ts,
       e.local_date, e.title, e.data, e.raw_id
FROM events e
UNION ALL
SELECT 'notable', 'dm:' || d.code || ':' || d.stat || ':' || d.date, 'metric_deviation',
       c.category_id, d.date || 'T12:00:00Z', d.date || 'T12:00:00Z', d.date,
       c.display || ' ' || printf('%.1f', d.value) || ' ' || c.unit || ' ('
         || printf('%+.1f', d.z) || ' SD vs 28-day baseline '
         || printf('%.1f', d.baseline_28d) || ')',
       json_object('code', d.code, 'stat', d.stat, 'value', d.value, 'unit', c.unit,
                   'baseline_28d', d.baseline_28d, 'sd_28d', d.sd_28d, 'z', d.z, 'n', d.n),
       NULL
FROM daily_metrics d
JOIN codes c ON c.code = d.code
WHERE d.notable = 1
UNION ALL
SELECT 'transaction', t.id, 'transaction', 'finance', t.ts, t.ts, t.local_date,
       coalesce(t.counterparty, t.item, t.category, 'transaction') || ' · '
         || t.amount_text || ' ' || t.currency,
       json_object('direction', t.direction, 'amount', t.amount_text, 'currency', t.currency,
                   'counterparty', t.counterparty, 'item', t.item, 'category', t.category,
                   'status', t.status, 'spend_rule', t.spend_rule, 'counted_spend', t.spend,
                   'source', t.source),
       t.raw_id
FROM v_transactions t
UNION ALL
SELECT 'item', i.id, i.type, i.category, i.event_start, coalesce(i.event_end, i.event_start),
       coalesce(i.local_date, substr(i.event_start, 1, 10)),
       coalesce(i.title, i.file_name, i.type),
       json_object('subtype', i.subtype, 'place', i.place, 'lat', i.lat, 'lon', i.lon,
                   'status', i.status, 'method', i.method, 'file_name', i.file_name),
       i.raw_id
FROM v_items i
WHERE i.event_start IS NOT NULL;
