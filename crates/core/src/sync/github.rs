//! Typed mapping for the official GitHub MCP server
//! (`github/github-mcp-server`): the user's commits and pull requests
//! become `events` of type `commit` and `pull_request`, category `work`.
//!
//! Runs on `mcp_response` objects whose source is `github`. The response
//! shape decides the mapping, so it does not depend on which tool produced
//! it:
//! - `list_commits`: a JSON array of commits (`sha`, `html_url`,
//!   `commit.message`, `commit.author.date`, `author.login`);
//! - `search_commits`: `{total_count, incomplete_results, items}` with the
//!   same commit fields plus `repository.full_name`;
//! - `search_pull_requests`: the issue-search envelope whose items carry
//!   `number`, `title`, `state`, `html_url`, `user.login`, `created_at`,
//!   `updated_at`, `closed_at` and `pull_request.merged_at`;
//! - `list_pull_requests`: an array of pull requests (`number`, `title`,
//!   `state`, `merged_at`, `html_url`, `user.login`, `created_at`, …).
//!
//! Anything else (for example `get_me`) is left to the generic tier.
//! Overlapping windows return the same commit or pull request again; one
//! event per commit or pull request is kept, updated when a response
//! reports a newer `updated_at`.

use anyhow::Result;
use chrono::FixedOffset;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::ids::{fingerprint, from_fp};
use crate::pipeline::Processor;
use crate::sources::{add_rule_link, upsert_entity, GlobalCtx, GlobalStage, RawCtx};
use crate::sync::generic::{get_path, parse_when, payload, Payload};
use crate::sync::process::clean_text;
use crate::sync::runner::recipe_for_origin;

pub const SOURCE_NAME: &str = "github";
pub const PROCESSOR: Processor = Processor { name: "sync.github", version: "1.0.0" };

pub const LINKS: GlobalStage = GlobalStage {
    processor: Processor { name: "sync.github_links", version: "1.0.0" },
    run: link_repositories,
};

const UPSERT_EVENT: &str = "INSERT INTO events
  (id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, body, data, raw_id, run_id, derived)
  VALUES (?1, ?2, ?3, 'work', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)
  ON CONFLICT (fp) DO UPDATE SET
    start_ts = excluded.start_ts, end_ts = excluded.end_ts, tz_offset = excluded.tz_offset,
    local_date = excluded.local_date, title = excluded.title, body = excluded.body, data = excluded.data,
    raw_id = excluded.raw_id, run_id = excluded.run_id
  WHERE coalesce(json_extract(excluded.data, '$.updated_at'), '') > coalesce(json_extract(events.data, '$.updated_at'), '')";

/// One mapped record.
#[derive(Debug, Clone, PartialEq)]
pub struct GithubEvent {
    pub kind: &'static str,
    pub fp: String,
    pub start: crate::sync::generic::When,
    pub title: String,
    pub body: Option<String>,
    pub data: Value,
}

pub fn load(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    if ctx.meta.source != SOURCE_NAME {
        return Ok(json!({ "skipped": "not a github response" }));
    }
    let recipe = recipe_for_origin(ctx.config, ctx.meta.origin_ref.as_deref());
    let local = recipe.map(|r| r.utc_offset).unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero"));
    let login = recipe.and_then(|r| r.def.github.as_ref()).and_then(|g| g.login.clone());
    let Payload::Json(v) = payload(&ctx.meta.kind, content) else {
        return Ok(json!({ "commits": 0, "pull_requests": 0 }));
    };
    let mut commits = 0u64;
    let mut prs = 0u64;
    let mut skipped = 0u64;
    let mut stmt = conn.prepare_cached(UPSERT_EVENT)?;
    for rec in records(&v) {
        let Some(e) = map_record(rec, local, login.as_deref()) else {
            skipped += 1;
            continue;
        };
        stmt.execute(params![
            from_fp("evt", &e.fp),
            e.fp,
            e.kind,
            e.start.utc,
            e.start.utc,
            e.start.tz_offset,
            e.start.local_date,
            e.title,
            e.body,
            e.data.to_string(),
            ctx.raw_id,
            ctx.run_id
        ])?;
        match e.kind {
            "commit" => commits += 1,
            _ => prs += 1,
        }
    }
    Ok(json!({ "commits": commits, "pull_requests": prs, "skipped": skipped }))
}

fn records(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => match o.get("items") {
            Some(Value::Array(a)) => a.iter().collect(),
            _ => vec![v],
        },
        _ => Vec::new(),
    }
}

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    get_path(v, path).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `owner/repo` from `https://github.com/owner/repo/…` or an API URL
/// `https://api.github.com/repos/owner/repo`.
fn repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("https://api.github.com/repos/"))?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

fn repo_of(rec: &Value) -> Option<String> {
    str_at(rec, "repository.full_name")
        .map(str::to_string)
        .or_else(|| str_at(rec, "base.repo.full_name").map(str::to_string))
        .or_else(|| str_at(rec, "html_url").and_then(repo_from_url))
        .or_else(|| str_at(rec, "repository_url").and_then(repo_from_url))
}

fn login_matches(actual: Option<&str>, wanted: Option<&str>) -> bool {
    match wanted {
        None => true,
        Some(w) => actual.is_some_and(|a| a.eq_ignore_ascii_case(w)),
    }
}

/// Maps one commit or pull request; `None` for anything else, or for a
/// record by someone other than `login` when it is set.
pub fn map_record(rec: &Value, local: FixedOffset, login: Option<&str>) -> Option<GithubEvent> {
    if let (Some(sha), Some(_)) = (str_at(rec, "sha"), rec.get("commit").filter(|c| c.is_object())) {
        let author = str_at(rec, "author.login");
        if !login_matches(author, login) {
            return None;
        }
        let repo = repo_of(rec)?;
        let when = get_path(rec, "commit.author.date")
            .or_else(|| get_path(rec, "commit.committer.date"))
            .and_then(|v| parse_when(v, local))?;
        let message = str_at(rec, "commit.message").unwrap_or("");
        let subject = clean_text(message.lines().next().unwrap_or(""), 160);
        return Some(GithubEvent {
            kind: "commit",
            fp: fingerprint(&["github", "commit", &repo, sha]),
            title: format!("{repo}: {}", if subject.is_empty() { &sha[..sha.len().min(7)] } else { &subject }),
            body: (!message.is_empty()).then(|| message.chars().take(10_000).collect()),
            data: json!({
                "source": "github",
                "repo": repo,
                "sha": sha,
                "html_url": str_at(rec, "html_url"),
                "author_login": author,
                "author_name": str_at(rec, "commit.author.name"),
            }),
            start: when,
        });
    }
    let number = rec.get("number").and_then(Value::as_u64)?;
    let html_url = str_at(rec, "html_url");
    let is_pr = rec.get("pull_request").is_some_and(|p| p.is_object())
        || html_url.is_some_and(|u| u.contains("/pull/"));
    if !is_pr {
        return None;
    }
    let author = str_at(rec, "user.login");
    if !login_matches(author, login) {
        return None;
    }
    let repo = repo_of(rec)?;
    let when = get_path(rec, "created_at").and_then(|v| parse_when(v, local))?;
    let merged_at = str_at(rec, "merged_at").or_else(|| str_at(rec, "pull_request.merged_at"));
    let state = if merged_at.is_some() || rec.get("merged").and_then(Value::as_bool) == Some(true) {
        "merged"
    } else {
        str_at(rec, "state").unwrap_or("unknown")
    };
    let title = clean_text(str_at(rec, "title").unwrap_or(""), 160);
    Some(GithubEvent {
        kind: "pull_request",
        fp: fingerprint(&["github", "pull_request", &repo, &number.to_string()]),
        title: format!("{repo}#{number} {title}").trim().to_string(),
        body: None,
        data: json!({
            "source": "github",
            "repo": repo,
            "number": number,
            "state": state,
            "draft": rec.get("draft").and_then(Value::as_bool).unwrap_or(false),
            "html_url": html_url,
            "author_login": author,
            "created_at": str_at(rec, "created_at"),
            "updated_at": str_at(rec, "updated_at"),
            "closed_at": str_at(rec, "closed_at"),
            "merged_at": merged_at,
        }),
        start: when,
    })
}

/// Repositories become `repository` entities linked from their commits and
/// pull requests. Runs after the built-in link rules, which rebuild
/// entities from scratch.
fn link_repositories(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT e.id, json_extract(e.data, '$.repo') FROM events e
             WHERE e.type IN ('commit', 'pull_request') AND json_extract(e.data, '$.source') = 'github'
               AND json_extract(e.data, '$.repo') IS NOT NULL
             ORDER BY e.start_ts, e.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut links = 0u64;
    for (event_id, repo) in rows {
        let ent = upsert_entity(conn, "repository", &repo)?;
        if add_rule_link(conn, "event", &event_id, &ent, "in_repository")? {
            links += 1;
        }
    }
    Ok(json!({ "links": links }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls() {
        assert_eq!(repo_from_url("https://github.com/o/r/commit/abc").as_deref(), Some("o/r"));
        assert_eq!(repo_from_url("https://api.github.com/repos/o/r").as_deref(), Some("o/r"));
        assert_eq!(repo_from_url("https://example.com/o/r"), None);
    }
}
