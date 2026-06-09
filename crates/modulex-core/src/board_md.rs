//! Markdown round-trip for board cards.
//!
//! SQLite is the operational store; the portable, sovereign form of a card is
//! a markdown file with YAML frontmatter:
//!
//! ```text
//! ---
//! id: homelab-2026-06-09-vpn-cert
//! project: homelab
//! created: 2026-06-09
//! summary: Renew the VPN certificate
//! size: 1d
//! status: blocked
//! blocked_on:
//!   - https://example.com/issues/1   # waiting on upstream
//! refs:
//!   issue: https://example.com/issues/42
//! ---
//!
//! # Body markdown …
//! ```
//!
//! [`card_from_markdown`] is tolerant of real-world variance (free-text
//! `status`, trailing `# comments` on list items — stripped by the YAML
//! parser, a `blocked_by` alias for `blocked_on`). `lane` and `context` are
//! NOT frontmatter — they are directory facts, so the parser leaves them empty
//! and the directory walker ([`crate::store::Store::import_dir`]) fills them in.

use crate::store::{Card, CardInput, CardRef};

/// Errors from parsing a card's markdown.
#[derive(Debug, thiserror::Error)]
pub enum BoardMdError {
    /// No `--- … ---` frontmatter block.
    #[error("board card: missing or malformed YAML frontmatter (--- ... ---)")]
    MissingFrontmatter,
    /// Frontmatter parsed but had no `id`.
    #[error("board card: frontmatter is missing the required `id`")]
    MissingId,
    /// The frontmatter was not valid YAML.
    #[error("board card: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

/// Render a card to its `--- frontmatter --- + body` markdown form, emitting
/// the canonical knowledge-repo field order. `lane`/`context` are omitted
/// (they are encoded by where the file is written).
#[must_use]
pub fn card_to_markdown(card: &Card) -> String {
    let mut out = String::from("---\n");
    push_scalar(&mut out, "id", Some(&card.card_id));
    push_scalar(&mut out, "project", non_empty(&card.project));
    push_scalar(&mut out, "created", card.created.as_deref());
    push_scalar(&mut out, "updated", card.updated.as_deref());
    push_scalar(&mut out, "summary", non_empty(&card.summary));
    push_scalar(&mut out, "size", card.size.as_deref());
    push_scalar(&mut out, "status", card.status.as_deref());
    push_scalar(&mut out, "recurs", card.recurs.as_deref());
    push_scalar(&mut out, "expires", card.expires.as_deref());

    let blocked: Vec<&CardRef> = card
        .refs
        .iter()
        .filter(|r| r.kind == "blocked_on")
        .collect();
    if !blocked.is_empty() {
        out.push_str("blocked_on:\n");
        for b in blocked {
            out.push_str(&format!("  - {}\n", yaml_scalar(&b.value)));
        }
    }
    let refs: Vec<&CardRef> = card.refs.iter().filter(|r| r.kind == "ref").collect();
    if !refs.is_empty() {
        out.push_str("refs:\n");
        for r in refs {
            out.push_str(&format!("  {}: {}\n", r.label, yaml_scalar(&r.value)));
        }
    }

    push_scalar(&mut out, "author", card.author.as_deref());
    push_scalar(&mut out, "source", card.source.as_deref());
    push_scalar(&mut out, "source_id", card.source_id.as_deref());
    out.push_str("---\n");

    if !card.body.is_empty() {
        out.push('\n');
        out.push_str(&card.body);
        if !card.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Parse a card markdown file into a [`CardInput`]. `lane`/`context` are left
/// empty for the caller to fill from the file's directory position.
///
/// # Errors
/// [`BoardMdError`] when the frontmatter is missing, malformed, or has no `id`.
pub fn card_from_markdown(text: &str) -> Result<CardInput, BoardMdError> {
    let (frontmatter, body) = split_frontmatter(text)?;
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&frontmatter)?;
    let map = value.as_mapping().ok_or(BoardMdError::MissingFrontmatter)?;

    let get = |key: &str| map.get(key).and_then(scalar_to_string);
    let card_id = get("id").ok_or(BoardMdError::MissingId)?;

    let mut refs = Vec::new();
    if let Some(blocked) = map.get("blocked_on").or_else(|| map.get("blocked_by")) {
        for (i, value) in normalize_seq(blocked).into_iter().enumerate() {
            refs.push(CardRef {
                kind: "blocked_on".into(),
                label: String::new(),
                value,
                ordinal: i as i64,
            });
        }
    }
    if let Some(refs_map) = map.get("refs").and_then(serde_yaml_ng::Value::as_mapping) {
        for (k, v) in refs_map {
            if let (Some(label), Some(value)) = (k.as_str(), scalar_to_string(v)) {
                refs.push(CardRef {
                    kind: "ref".into(),
                    label: label.to_string(),
                    value,
                    ordinal: 0,
                });
            }
        }
    }

    Ok(CardInput {
        card_id,
        project: get("project").unwrap_or_default(),
        lane: String::new(),
        context: String::new(),
        summary: get("summary").unwrap_or_default(),
        size: get("size"),
        status: get("status"),
        recurs: get("recurs"),
        expires: get("expires"),
        created: get("created"),
        updated: get("updated"),
        body,
        author: get("author"),
        source: get("source"),
        source_id: get("source_id"),
        refs,
    })
}

/// Split off the leading `--- … ---` frontmatter block; returns
/// `(frontmatter_yaml, body)`.
fn split_frontmatter(text: &str) -> Result<(String, String), BoardMdError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(BoardMdError::MissingFrontmatter);
    }
    let mut frontmatter = String::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed && line.trim_end() == "---" {
            closed = true;
            continue;
        }
        if closed {
            body_lines.push(line);
        } else {
            frontmatter.push_str(line);
            frontmatter.push('\n');
        }
    }
    if !closed {
        return Err(BoardMdError::MissingFrontmatter);
    }
    let body = body_lines.join("\n").trim_start_matches('\n').to_string();
    Ok((frontmatter, body))
}

/// A YAML scalar can be `value`, or a list of values; normalize to a `Vec`.
fn normalize_seq(value: &serde_yaml_ng::Value) -> Vec<String> {
    match value {
        serde_yaml_ng::Value::Sequence(items) => {
            items.iter().filter_map(scalar_to_string).collect()
        }
        other => scalar_to_string(other).into_iter().collect(),
    }
}

/// Stringify a scalar YAML value (string/number/bool); `None` for collections.
fn scalar_to_string(value: &serde_yaml_ng::Value) -> Option<String> {
    match value {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

fn push_scalar(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        out.push_str(&format!("{key}: {}\n", yaml_scalar(v)));
    }
}

/// Emit a YAML-safe scalar: plain when unambiguous, else double-quoted. Keeps
/// URLs (which contain `:` but not `: `) unquoted, matching the source format.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.contains(": ")
        || s.contains(" #")
        || s.ends_with(':')
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.starts_with(|c: char| "!&*?|>%@`\"'#,[]{}".contains(c))
        || s.starts_with("- ");
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A synthetic but format-faithful fixture (no real board content).
    const FIXTURE: &str = r#"---
id: homelab-2026-06-09-vpn-cert
project: homelab
created: 2026-06-09
updated: 2026-06-09
summary: "Renew VPN cert: blocked on upstream"
size: 1d
status: blocked
blocked_on:
  - https://example.com/issues/1   # waiting on upstream
  - https://example.com/issues/2
refs:
  issue: https://example.com/issues/42
  doc: docs/vpn.md
---

# Renew the VPN certificate

Steps go here.
"#;

    fn card_of(input: CardInput, id: i64) -> Card {
        Card {
            id,
            card_id: input.card_id,
            project: input.project,
            lane: input.lane,
            context: input.context,
            summary: input.summary,
            size: input.size,
            status: input.status,
            recurs: input.recurs,
            expires: input.expires,
            created: input.created,
            updated: input.updated,
            body: input.body,
            author: input.author,
            source: input.source,
            source_id: input.source_id,
            created_gen: 1,
            updated_gen: 1,
            closed_gen: None,
            refs: input.refs,
        }
    }

    #[test]
    fn parses_real_world_fixture() {
        let input = card_from_markdown(FIXTURE).unwrap();
        assert_eq!(input.card_id, "homelab-2026-06-09-vpn-cert");
        assert_eq!(input.project, "homelab");
        assert_eq!(input.summary, "Renew VPN cert: blocked on upstream");
        assert_eq!(input.size.as_deref(), Some("1d"));
        assert_eq!(input.status.as_deref(), Some("blocked"));
        assert_eq!(input.created.as_deref(), Some("2026-06-09"));
        assert!(input.lane.is_empty(), "lane is a directory fact");
        assert!(input.body.starts_with("# Renew the VPN certificate"));

        let blocked: Vec<_> = input
            .refs
            .iter()
            .filter(|r| r.kind == "blocked_on")
            .collect();
        assert_eq!(blocked.len(), 2);
        assert_eq!(
            blocked[0].value, "https://example.com/issues/1",
            "trailing # comment is stripped"
        );
        assert_eq!(blocked[0].ordinal, 0);
        assert_eq!(blocked[1].ordinal, 1);

        let refs: Vec<_> = input.refs.iter().filter(|r| r.kind == "ref").collect();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label, "issue");
    }

    #[test]
    fn round_trips_semantically() {
        let from_fixture = card_from_markdown(FIXTURE).unwrap();
        let markdown = card_to_markdown(&card_of(from_fixture.clone(), 1));
        let reparsed = card_from_markdown(&markdown).unwrap();

        assert_eq!(reparsed.card_id, from_fixture.card_id);
        assert_eq!(
            reparsed.summary, from_fixture.summary,
            "colon summary survives"
        );
        assert_eq!(reparsed.status, from_fixture.status);
        assert_eq!(reparsed.created, from_fixture.created);
        assert_eq!(reparsed.body, from_fixture.body);
        assert_eq!(
            reparsed.refs, from_fixture.refs,
            "refs + blocked_on survive"
        );
    }

    #[test]
    fn blocked_by_alias_and_freetext_status() {
        let text = r#"---
id: x-1
status: scoped — not started
blocked_by: a free-text reason
summary: s
---

body
"#;
        let input = card_from_markdown(text).unwrap();
        assert_eq!(input.status.as_deref(), Some("scoped — not started"));
        let blocked: Vec<_> = input
            .refs
            .iter()
            .filter(|r| r.kind == "blocked_on")
            .collect();
        assert_eq!(blocked.len(), 1, "blocked_by aliases blocked_on");
        assert_eq!(blocked[0].value, "a free-text reason");
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        assert!(matches!(
            card_from_markdown("# just a heading\n"),
            Err(BoardMdError::MissingFrontmatter)
        ));
    }

    #[test]
    fn missing_id_is_an_error() {
        assert!(matches!(
            card_from_markdown("---\nproject: x\n---\nbody\n"),
            Err(BoardMdError::MissingId)
        ));
    }

    #[test]
    fn yaml_scalar_quotes_only_when_needed() {
        assert_eq!(
            yaml_scalar("https://example.com/x"),
            "https://example.com/x"
        );
        assert_eq!(yaml_scalar("plain text"), "plain text");
        assert_eq!(yaml_scalar("has: colon"), "\"has: colon\"");
    }
}
