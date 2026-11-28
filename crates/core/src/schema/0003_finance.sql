-- MirrorSelf index: money movements (W1, docs/demo-plan.md). One row per
-- order from a payment or bank export; Alipay first, general enough for
-- WeChat Pay and bank statements. Derived from the vault and rebuilt by
-- `mirrorself process --rebuild`. See docs/sources/alipay.md.

CREATE TABLE transactions (
  id                   TEXT PRIMARY KEY,  -- stable_id("txn", [source, order_no]); the same order in a later export keeps its id
  source               TEXT NOT NULL,     -- alipay; later wechat_pay, bank:<name>, …
  order_no             TEXT,              -- the source's own transaction number (支付宝 交易订单号)
  merchant_order_no    TEXT,              -- 商家订单号
  refund_of            TEXT,              -- order_no of the original order when this row is a refund
  ts                   TEXT NOT NULL,     -- UTC, RFC 3339
  tz_offset            INTEGER NOT NULL,  -- original offset, seconds east of UTC (Alipay: 28800)
  local_date           TEXT NOT NULL,     -- the day in the original offset
  counterparty         TEXT,              -- 交易对方
  counterparty_account TEXT,              -- 对方账号, masked by the source
  item                 TEXT,              -- 商品说明
  category             TEXT,              -- the source's own category (交易分类), not normalised
  direction            TEXT NOT NULL CHECK (direction IN ('expense', 'income', 'neutral')),
  direction_label      TEXT,              -- 收/支 as written: 支出, 收入, 不计收支
  amount_minor         INTEGER NOT NULL CHECK (amount_minor >= 0),  -- exact, in 1/100 of the currency (fen)
  currency             TEXT NOT NULL DEFAULT 'CNY',
  payment_method       TEXT,              -- 收/付款方式
  status               TEXT,              -- 交易状态 as written
  note                 TEXT,              -- 备注
  spend_rule           TEXT NOT NULL,     -- why it does or does not count as spend; see docs/sources/alipay.md
  refunded_minor       INTEGER NOT NULL DEFAULT 0,  -- successful refunds found for this order, in fen
  source_exported_at   TEXT,              -- export time printed in the file (UTC); a later export's row wins
  raw_id               TEXT NOT NULL REFERENCES raw_objects(id),
  run_id               TEXT NOT NULL
);
CREATE INDEX transactions_date ON transactions(local_date);
CREATE INDEX transactions_refund_of ON transactions(source, refund_of);
CREATE INDEX transactions_raw ON transactions(raw_id);

-- Agent-facing view. `amount` is for display and arithmetic in queries;
-- `amount_minor` is the exact value. `spend` is what this row adds to the
-- daily ms:spend metric (0 when it does not count).
CREATE VIEW v_transactions AS
SELECT t.id, t.source, t.ts, t.tz_offset, t.local_date,
       datetime(t.ts, printf('%+d seconds', t.tz_offset)) AS local_time,
       t.direction, t.direction_label,
       t.amount_minor, t.amount_minor / 100.0 AS amount,
       printf('%d.%02d', t.amount_minor / 100, t.amount_minor % 100) AS amount_text,
       t.currency, t.counterparty, t.counterparty_account, t.item, t.category,
       t.payment_method, t.status, t.order_no, t.merchant_order_no, t.refund_of, t.note,
       t.spend_rule, t.refunded_minor,
       CASE WHEN t.spend_rule = 'counted' AND t.amount_minor > t.refunded_minor
            THEN (t.amount_minor - t.refunded_minor) / 100.0 ELSE 0.0 END AS spend,
       t.source_exported_at, t.raw_id,
       r.name AS file_name
FROM transactions t
JOIN raw_objects r ON r.id = t.raw_id;
