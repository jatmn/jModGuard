//! Public policy templates and their shared, deliberately limited config projection.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Url;
use serde::Serialize;

use super::ApiState;
use crate::config::LegalConfig;

#[derive(Serialize)]
struct PublicDetails<'a> {
    operator_name: Option<&'a str>,
    privacy_contact: Option<&'a str>,
    deletion_instructions: Option<&'a str>,
    subprocessors: Vec<&'a str>,
    jurisdiction: Option<&'a str>,
    effective_date: Option<&'a str>,
}

impl<'a> From<&'a LegalConfig> for PublicDetails<'a> {
    fn from(legal: &'a LegalConfig) -> Self {
        Self {
            operator_name: published(&legal.operator_name),
            privacy_contact: published(&legal.privacy_contact),
            deletion_instructions: published(&legal.deletion_instructions),
            subprocessors: legal
                .subprocessors
                .iter()
                .filter_map(|s| published(s))
                .collect(),
            jurisdiction: published(&legal.jurisdiction),
            effective_date: published(&legal.effective_date),
        }
    }
}

fn published(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub(super) async fn details(State(app): State<Arc<ApiState>>) -> Response {
    Json(PublicDetails::from(&app.cfg.legal)).into_response()
}

pub(super) async fn privacy(State(app): State<Arc<ApiState>>) -> Html<String> {
    render(Policy::Privacy, &app.cfg.legal)
}

pub(super) async fn terms(State(app): State<Arc<ApiState>>) -> Html<String> {
    render(Policy::Terms, &app.cfg.legal)
}

const UNCONFIGURED: &str = "Not configured";

enum Policy {
    Privacy,
    Terms,
}

fn render(policy: Policy, legal: &LegalConfig) -> Html<String> {
    let (title, scope, template) = match policy {
        Policy::Privacy => (
            "Privacy Policy",
            "This policy covers the hosted jModGuard Discord bot and its dashboard.",
            include_str!("legal/privacy.html"),
        ),
        Policy::Terms => (
            "Terms of Service",
            "These terms cover the hosted jModGuard Discord bot and its dashboard.",
            include_str!("legal/terms.html"),
        ),
    };
    let (contents, body) = template
        .split_once("<!-- policy-content -->")
        .expect("legal template separates its contents list from the policy body");
    let current = r#" aria-current="page""#;
    let (privacy_current, terms_current) = match policy {
        Policy::Privacy => (current, ""),
        Policy::Terms => ("", current),
    };

    let details = PublicDetails::from(legal);
    let operator = html_text(details.operator_name.unwrap_or(UNCONFIGURED));
    let effective_date = html_text(details.effective_date.unwrap_or(UNCONFIGURED));
    let body = body.trim_matches('\n').replace(
        "<!-- operator-details -->",
        &render_operator_details(&details),
    );
    Html(format!(
        include_str!("legal/layout.html"),
        title = title,
        scope = scope,
        privacy_current = privacy_current,
        terms_current = terms_current,
        operator = operator,
        effective_date = effective_date,
        contents = contents.trim_end(),
        body = body,
    ))
}

fn render_operator_details(details: &PublicDetails<'_>) -> String {
    let mut section = String::from(
        r#"<section class="legal-operator" aria-labelledby="operator-details"><h2 id="operator-details">Operator details</h2>"#,
    );
    if details.privacy_contact.is_none() {
        section.push_str("<p>This instance’s operator has not configured contact details.</p>");
    }
    section.push_str("<dl>");
    for (label, value, href) in [
        ("Operator or controller", details.operator_name, None),
        (
            "Privacy contact",
            details.privacy_contact,
            details.privacy_contact.and_then(contact_href),
        ),
        (
            "How to request deletion",
            details.deletion_instructions,
            None,
        ),
        ("Jurisdiction or region", details.jurisdiction, None),
        ("Effective date", details.effective_date, None),
    ] {
        section.push_str("<dt>");
        section.push_str(label);
        section.push_str("</dt><dd>");
        if let Some(href) = &href {
            section.push_str(r#"<a href=""#);
            section.push_str(&html_text(href).replace('"', "&quot;"));
            section.push_str(r#"">"#);
        }
        push_html_text(&mut section, value.unwrap_or(UNCONFIGURED));
        if href.is_some() {
            section.push_str("</a>");
        }
        section.push_str("</dd>");
    }
    section.push_str("<dt>Subprocessors</dt><dd>");
    if details.subprocessors.is_empty() {
        section.push_str(UNCONFIGURED);
    } else {
        section.push_str("<ul>");
        for provider in &details.subprocessors {
            section.push_str("<li>");
            push_html_text(&mut section, provider);
            section.push_str("</li>");
        }
        section.push_str("</ul>");
    }
    section.push_str("</dd></dl></section>");
    section
}

fn contact_href(value: &str) -> Option<String> {
    if let Some((scheme, rest)) = value.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next()?;
        if !scheme.eq_ignore_ascii_case("https")
            || authority.is_empty()
            || authority.contains('@')
            || value.chars().any(|ch| {
                ch.is_whitespace() || ch.is_control() || matches!(ch, '\\' | '"' | '<' | '>')
            })
        {
            return None;
        }
        let url = Url::parse(value).ok()?;
        return url.has_host().then(|| url.into());
    }

    let (local, domain) = value.split_once('@')?;
    // A bare ASCII dot-atom mailbox with a dotted DNS name.
    if value.len() > 254
        || local.len() > 64
        || !local.split('.').all(|atom| {
            !atom.is_empty()
                && atom
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&ch))
        })
        || !domain.contains('.')
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        })
    {
        return None;
    }
    // Encode characters that could alter the mailto destination or add headers.
    const MAILBOX_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'.')
        .remove(b'-')
        .remove(b'_')
        .remove(b'~')
        .remove(b'+');
    let local = utf8_percent_encode(local, MAILBOX_ENCODE_SET);
    Some(format!("mailto:{local}@{domain}"))
}

fn html_text(value: &str) -> String {
    let mut escaped = String::new();
    push_html_text(&mut escaped, value);
    escaped
}

/// Escape an operator-provided text node.
fn push_html_text(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}
