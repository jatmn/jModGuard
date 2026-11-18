//! Public policy templates and their shared, deliberately limited config projection.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
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
    render(include_str!("legal/privacy.html"), &app.cfg.legal)
}

pub(super) async fn terms(State(app): State<Arc<ApiState>>) -> Html<String> {
    render(include_str!("legal/terms.html"), &app.cfg.legal)
}

const UNCONFIGURED: &str = "Not configured";

fn render(template: &str, legal: &LegalConfig) -> Html<String> {
    let details = PublicDetails::from(legal);
    let mut section = String::from("<h2>Operator details</h2>");
    if details.privacy_contact.is_none() {
        section.push_str("<p>This instance’s operator has not configured contact details.</p>");
    }
    section.push_str("<dl>");
    for (label, value) in [
        ("Operator or controller", details.operator_name),
        ("Privacy contact", details.privacy_contact),
        ("How to request deletion", details.deletion_instructions),
        ("Jurisdiction or region", details.jurisdiction),
        ("Effective date", details.effective_date),
    ] {
        section.push_str("<dt>");
        section.push_str(label);
        section.push_str("</dt><dd>");
        push_html_text(&mut section, value.unwrap_or(UNCONFIGURED));
        section.push_str("</dd>");
    }
    section.push_str("<dt>Subprocessors</dt><dd>");
    if details.subprocessors.is_empty() {
        section.push_str(UNCONFIGURED);
    } else {
        section.push_str("<ul>");
        for provider in details.subprocessors {
            section.push_str("<li>");
            push_html_text(&mut section, provider);
            section.push_str("</li>");
        }
        section.push_str("</ul>");
    }
    section.push_str("</dd></dl>");
    Html(template.replace("<!-- operator-details -->", &section))
}

/// Values are only used in text nodes, never as markup, attributes or URLs.
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
