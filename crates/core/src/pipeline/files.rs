//! Uploaded files → `items` (workstream W5 in `docs/demo-plan.md`).
//!
//! Placeholder: the source is registered so the registry does not change
//! when the processors land. Raw kinds: `file_pdf`, `file_image`,
//! `file_text`, `file_other` (assigned by `sources::generic_file_kind`) and
//! `model_output` for cached model responses.

use crate::sources::Source;

pub static SOURCE: Source = Source::empty("files");
