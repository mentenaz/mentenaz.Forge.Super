//! The pieces of an advisory card on the Vulnerabilities page: the table of
//! packages an advisory reaches, and the short texts around it.
//!
//! The texts are plain functions so they can be tested; the table is a
//! `gpui_component` `DataTable` driven by [`AffectedDelegate`].

use gpui::{App, Context, Hsla, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{
    ActiveTheme as _, Theme,
    table::{Column, TableDelegate, TableState},
};
use npm_backend::{AuditAdvisory, AuditAffected, AuditFix};

/// The most rows a card's table shows before it scrolls inside the card.
pub const MAX_TABLE_ROWS: usize = 8;
/// Pixel heights the card gives its table: the header, and each row.
const TABLE_HEADER_HEIGHT: f32 = 34.0;
const TABLE_ROW_HEIGHT: f32 = 30.0;

/// The tint of a severity: red for critical and high, amber for moderate, grey below.
pub fn severity_color(theme: &Theme, severity: &str) -> Hsla {
    match severity {
        "critical" | "high" => theme.danger,
        "moderate" => theme.warning,
        _ => theme.muted_foreground,
    }
}

/// How tall a card's table is for `rows` rows, in pixels.
pub fn table_height(rows: usize) -> f32 {
    TABLE_HEADER_HEIGHT + TABLE_ROW_HEIGHT * rows.min(MAX_TABLE_ROWS) as f32 + 2.0
}

/// What the card's footer says about fixing an advisory.
pub fn fix_label(fix: &AuditFix) -> String {
    match fix {
        AuditFix::None => "No fix yet".to_string(),
        AuditFix::Available(None) => "Fix available".to_string(),
        AuditFix::Available(Some(version)) => format!("Fix available ({version})"),
        AuditFix::Breaking(None) => "Fix needs a breaking upgrade".to_string(),
        AuditFix::Breaking(Some(version)) => {
            format!("Fix needs a breaking upgrade ({version})")
        }
    }
}

/// A fix `npm audit fix` can apply without breaking changes.
pub fn is_plain_fix(fix: &AuditFix) -> bool {
    matches!(fix, AuditFix::Available(_))
}

/// The card's "who to upgrade" line: the project's own dependencies the
/// advisory reaches. `None` when none is known.
pub fn via_label(advisory: &AuditAdvisory) -> Option<String> {
    const SHOWN: usize = 3;
    let direct = &advisory.direct_via;
    match direct.as_slice() {
        [] => None,
        [only] if *only == advisory.package => Some("Your dependency".to_string()),
        list => {
            let mut text = format!(
                "Via {}",
                list.iter()
                    .take(SHOWN)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if list.len() > SHOWN {
                text.push_str(&format!(" +{}", list.len() - SHOWN));
            }
            Some(text)
        }
    }
}

/// Whether the advisory's own package is one the project lists.
pub fn is_direct(advisory: &AuditAdvisory) -> bool {
    advisory.direct_via.contains(&advisory.package)
}

/// Where an advisory's written text stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdvisoryState {
    Loading,
    /// The advisory as markdown.
    Ready(String),
    Failed(String),
}

/// A fetch failure in words a reader can act on.
pub fn friendly_fetch_error(error: &str) -> String {
    if error.contains("403") || error.contains("429") {
        "GitHub's request limit was reached for now. Try again later, or open it in the browser."
            .to_string()
    } else {
        format!("Could not load the advisory ({error}).")
    }
}

/// The rows of one card's table.
pub struct AffectedDelegate {
    columns: Vec<Column>,
    rows: Vec<AuditAffected>,
}

impl AffectedDelegate {
    pub fn new(rows: Vec<AuditAffected>) -> Self {
        Self {
            columns: vec![
                Column::new("package", "Package").width(200.),
                Column::new("severity", "Severity").width(90.),
                Column::new("type", "Type").width(80.),
                Column::new("location", "Location").width(260.),
                Column::new("fix", "Fix").width(220.),
            ],
            rows,
        }
    }
}

impl TableDelegate for AffectedDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns.get(col_ix).cloned().unwrap_or_default()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (Some(row), Some(column)) = (self.rows.get(row_ix), self.columns.get(col_ix)) else {
            return div().child("--").into_any_element();
        };
        match column.key.as_ref() {
            "package" => div()
                .truncate()
                .font_family("Cascadia Mono")
                .child(row.package.clone())
                .into_any_element(),
            "severity" => div()
                .text_color(severity_color(cx.theme(), &row.severity))
                .child(row.severity.clone())
                .into_any_element(),
            "type" => div()
                .text_color(cx.theme().muted_foreground)
                .child(if row.is_direct { "direct" } else { "indirect" })
                .into_any_element(),
            "location" => div()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(row.location.clone())
                .into_any_element(),
            "fix" => div()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(fix_label(&row.fix))
                .into_any_element(),
            _ => div().child("--").into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advisory(package: &str, direct_via: &[&str]) -> AuditAdvisory {
        AuditAdvisory {
            id: "1".into(),
            package: package.into(),
            severity: "moderate".into(),
            title: "A title".into(),
            url: String::new(),
            cwe: Vec::new(),
            range: String::new(),
            fix: AuditFix::None,
            affected: Vec::new(),
            direct_via: direct_via.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn fix_text_names_the_kind_of_fix() {
        assert_eq!(fix_label(&AuditFix::None), "No fix yet");
        assert_eq!(fix_label(&AuditFix::Available(None)), "Fix available");
        assert_eq!(
            fix_label(&AuditFix::Available(Some("1.2.3".into()))),
            "Fix available (1.2.3)"
        );
        assert_eq!(
            fix_label(&AuditFix::Breaking(Some("2.0.0".into()))),
            "Fix needs a breaking upgrade (2.0.0)"
        );
        assert!(is_plain_fix(&AuditFix::Available(None)));
        assert!(!is_plain_fix(&AuditFix::Breaking(None)));
        assert!(!is_plain_fix(&AuditFix::None));
    }

    #[test]
    fn via_names_what_to_upgrade() {
        assert_eq!(via_label(&advisory("glob", &[])), None);
        assert_eq!(
            via_label(&advisory("lodash", &["lodash"])).as_deref(),
            Some("Your dependency")
        );
        assert_eq!(
            via_label(&advisory("glob", &["@jest/core"])).as_deref(),
            Some("Via @jest/core")
        );
        assert_eq!(
            via_label(&advisory("glob", &["a", "b", "c", "d", "e"])).as_deref(),
            Some("Via a, b, c +2")
        );
        assert!(is_direct(&advisory("lodash", &["lodash"])));
        assert!(!is_direct(&advisory("glob", &["@jest/core"])));
    }

    #[test]
    fn a_rate_limit_is_explained() {
        assert!(friendly_fetch_error("HTTP 403 Forbidden").contains("limit"));
        assert!(friendly_fetch_error("HTTP 429").contains("limit"));
        assert!(friendly_fetch_error("request failed: dns").contains("dns"));
    }

    #[test]
    fn the_table_stops_growing_and_scrolls() {
        assert!(table_height(1) < table_height(3));
        assert_eq!(
            table_height(MAX_TABLE_ROWS),
            table_height(MAX_TABLE_ROWS + 20)
        );
    }
}
