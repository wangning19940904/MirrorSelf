//! Feishu calendar → meeting events (workstream W2 in `docs/demo-plan.md`).
//!
//! Placeholder: the source is registered so the registry does not change
//! when the importer lands. Raw kind: `feishu_calendar_events`.

use crate::sources::Source;

pub static SOURCE: Source = Source::empty("feishu_calendar");
