# Alipay bills (支付宝账单)

MirrorSelf reads the transaction CSV that Alipay sends by e-mail. Each order
in it becomes one row of `transactions`, and expenses feed the daily spending
metric `ms:spend`.

## Exporting the file

In the Alipay app: **我的 → 账单 → ⋯ (top right) → 开具交易流水证明** (in some
versions **导出账单**), choose the personal-reconciliation purpose, a time range
and an e-mail address. Menu names move between app versions. Alipay e-mails a
password-protected zip and shows or sends the password separately. Unzip it
and import the CSV inside:

```bash
mirrorself import alipay alipay_record_20261020_101500.csv
```

Uploads through the phone page and `App::import_auto` recognise the file on
their own. Import every export you have; ranges may overlap (see
de-duplication below).

## The file

- GBK text with LF line endings. UTF-8 copies (with or without a BOM, for
  example after a spreadsheet re-save) and CRLF endings are also accepted.
- About 20 preamble lines: 姓名, 支付宝账户, the export range, 导出时间, record
  counts and sums for 收入 / 支出 / 不计收支, and notes.
- A header row, found by its column names (not its line number):
  `交易时间, 交易分类, 交易对方, 对方账号, 商品说明, 收/支, 金额, 收/付款方式, 交易状态, 交易订单号, 商家订单号, 备注`.
  Cells are padded with spaces, order numbers end with a tab, and every line
  ends with a comma. Columns may come in any order. Of these columns the
  importer needs 交易时间, 交易对方, 收/支, 金额, 交易状态 and 交易订单号 (or
  the older spellings 交易创建时间 and 交易号). Any other column may be missing.
- Lines after the table that are not transactions (summaries, dashes) are
  skipped and counted in the run report as `skipped_lines`.
- 交易时间 is Beijing time without an offset. It is stored as UTC with
  `tz_offset = 28800`. `local_date` is the Beijing date.

The layout was checked against a published example export and an
open-source parser (`deb-sig/double-entry-generator`, read for facts only,
no code taken). Older web-portal exports (交易号, 交易创建时间, 商品名称,
金额（元）) are accepted through the column aliases above but have not been
tested against a real file.

Detection is strict: a file is claimed as an Alipay bill only when it decodes
as text and has a header row with all the required columns. Raw kinds are
permanent, so a WeChat Pay bill (交易单号, 当前状态) or any other CSV is not
claimed and is stored as a generic file.

Raw object: source `alipay`, kind `alipay_bill_csv`, MIME `text/csv`.

## What is stored

`transactions` (migration `0003_finance.sql`), read through
`v_transactions`. The table is meant for other payment and bank sources too,
so each row has a `source`.

| Column | Meaning |
|---|---|
| `id` | `stable_id("txn", ["alipay", 交易订单号])` |
| `order_no`, `merchant_order_no` | 交易订单号, 商家订单号 (tabs and padding removed) |
| `refund_of` | For a refund row, the original order's number |
| `ts`, `tz_offset`, `local_date`, `local_time` (view only) | UTC time, offset, Beijing date and wall-clock time |
| `counterparty`, `counterparty_account` | 交易对方, 对方账号 (`/` becomes NULL) |
| `item`, `category` | 商品说明, 交易分类 exactly as written; no taxonomy is applied |
| `direction`, `direction_label` | `expense` (支出), `income` (收入) or `neutral` (不计收支, empty or unknown); the label as written |
| `amount_minor` | Exact amount in fen (1/100 CNY), an integer and never negative |
| `amount`, `amount_text` (view only) | `amount_minor / 100.0`, and the same as text `"299.00"` |
| `currency` | `CNY` |
| `payment_method`, `status`, `note` | 收/付款方式, 交易状态, 备注 as written |
| `spend_rule` | Why the row does or does not count as spending (below) |
| `refunded_minor` | Successful refunds found for this order, in fen |
| `spend` (view only) | What the row adds to daily `ms:spend`, in CNY; 0 when it does not count |
| `source_exported_at` | 导出时间 of the export the row came from, in UTC |
| `file_name` (view only) | The imported file's name |

**Amounts.** `amount_minor` is the exact value: the file's decimal text
converted to an integer number of fen without going through floating point.
An amount with more than two decimals, a minus sign or no digits is not
rounded. The row is skipped and counted as `malformed`; the run report lists
the first few line numbers. `amount` and `spend` are floating-point
conveniences for SQL.

The account holder's name (姓名) and Alipay account from the preamble are not
stored in the index. The name is used only to recognise transfers to oneself.
The raw file in the vault keeps everything.

## De-duplication across exports

Exports overlap. The same order in two files has the same `id`, so it is one
row. The row from the export with the later 导出时间 replaces the earlier one,
so a later status wins. For example, an order that was 等待确认收货 and is now
交易成功, or one that was 交易成功 and is now 交易关闭 after a full refund.
This holds whatever order the files are imported in. When two exports have
the same 导出时间, the row imported first is kept. Importing the same file
again stores nothing new.

## What counts as spending

`spend_rule` is decided per row:

| `spend_rule` | Rows | Counts toward `ms:spend` |
|---|---|---|
| `counted` | 支出 with 交易状态 交易成功, 支付成功, 等待确认收货, 等待对方发货, 等待发货 or 已发货 (the money has left the account) | yes, net of refunds |
| `refund` | 交易分类 退款, or 交易状态 starting with 退款 | no; successful ones reduce their original |
| `not_expense` | 收入, 不计收支 (top-ups, withdrawals, moves to and from 余额宝, investments, credit repayments), or an empty 收/支 | no |
| `not_completed` | 支出 with 交易关闭 (never paid, or refunded in full and closed), 等待付款, a failure, or any status not listed above | no |
| `self_transfer` | 支出 whose 交易对方 is the account holder's own 姓名 | no |
| `internal_transfer` | 支出 in 交易分类 信用借还 (repaying 花呗 or a credit card; the purchase was counted when it was made) or 投资理财 | no |

**Refunds.** Alipay lists a refund as its own row: 交易分类 退款, 收/支 不计收支,
交易状态 退款成功, and an order number that is the original's followed by `_`
and a suffix. Successful refunds are summed into the original's
`refunded_minor`. The original then counts `amount − refunded`, on its own
purchase date, so a refund never makes a day negative. Refunds from any
imported export count, so a refund in a later file reduces a purchase from
an earlier one. After a full refund Alipay usually marks the original 交易关闭,
which already excludes it. A refund whose original is in no imported file
changes nothing.

Transfers to other people (转账红包 with 支出) count as spending: the money
has left the account. Income is stored but not aggregated.

The `finance.spend` stage writes one `ms:spend` observation per counted
expense with a non-zero net amount. It is stamped with the transaction's
time and Beijing date. Its `raw_id` names the export it came from, and
`source_name` is left empty, so the health-oriented device rules (device
entities, cross-device de-duplication) never touch spending. `derive.daily_metrics` then
gives the daily sum in CNY with a 28-day baseline, SD and z-score
(`v_metrics_daily`, category `finance`).

## Entities, links and search

- Every 交易对方 becomes a `merchant` entity, linked from its transactions
  with relation `involves` (`v_entities`, `v_links`, `from_type` =
  `transaction`). Person-to-person transfers are included. The account
  holder's own name is not.
- Transactions are in the search index with kind `transaction`. The title is
  `交易对方 · amount CNY`. The body has the item, category, status, payment
  method and order number.

## Not imported

- WeChat Pay and bank statements (the table is ready for them; they need
  their own importers).
- Category mapping beyond Alipay's own 交易分类.
- Income metrics, balances and account totals. The preamble's summary sums
  are not checked against the rows.
- The password-protected zip itself: unzip it first.

## Processors

| Name | Kind | Writes |
|---|---|---|
| `alipay.parse` 1.0.0 | per raw object | `transactions` |
| `finance.spend` 1.0.0 | pre-derive | `transactions.refunded_minor`, `ms:spend` observations |
| `finance.merchants` 1.0.0 | post-link | `merchant` entities and `involves` links |
| `finance.search` 1.0.0 | post-index | `search_fts` rows of kind `transaction` |

`process --rebuild` empties `transactions` (it is a registered derived table)
and rebuilds the same rows and metrics from the vault.
