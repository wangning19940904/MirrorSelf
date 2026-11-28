//! File-format parsers. Each turns one raw object into typed records; the
//! pipeline decides what to keep and how to normalize it.

pub mod alipay;
pub mod apple_health;
pub mod feishu_calendar;
pub mod files;
pub mod report;
