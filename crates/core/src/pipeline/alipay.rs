//! Alipay bills → `transactions` (workstream W1 in `docs/demo-plan.md`).
//!
//! Placeholder: the source is registered so the registry does not change
//! when the importer lands. Raw kind: `alipay_bill_csv`.

use crate::sources::Source;

pub static SOURCE: Source = Source::empty("alipay");
