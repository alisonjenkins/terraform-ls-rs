//! Pure rendering functions for `tfls-lint`'s `--format` flag.
//!
//! Every `render_*` fn takes the same sorted `(relative path, Diagnostic)`
//! entries plus a [`Summary`] and returns a `String` — no I/O, so each is
//! unit-testable without spawning the binary. `main` picks the renderer and
//! writes the result to stdout; only the `text` format also gets the
//! human-readable summary line on stderr.
//!
//! Positions in every format are 1-based (line and column), matching the
//! existing `text` output.

use clap::ValueEnum;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Sarif,
    Github,
}

/// Aggregate counts handed to every renderer alongside the diagnostic list.
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub hints: usize,
    pub files: usize,
}

pub fn severity_word(d: &Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::HINT) => "hint",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::WARNING) => "warning",
        _ => "error",
    }
}

pub fn code_of(d: &Diagnostic) -> String {
    match &d.code {
        Some(NumberOrString::String(s)) => s.clone(),
        Some(NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

pub fn render_text(entries: &[(String, Diagnostic)], _summary: &Summary) -> String {
    let mut out = String::new();
    for (path, d) in entries {
        out.push_str(&format!(
            "{path}:{}:{}: {} [{}] {}\n",
            d.range.start.line + 1,
            d.range.start.character + 1,
            severity_word(d),
            code_of(d),
            d.message,
        ));
    }
    out
}

#[derive(Serialize)]
struct JsonDocument<'a> {
    version: u32,
    summary: JsonSummary,
    diagnostics: Vec<JsonDiagnostic<'a>>,
}

#[derive(Serialize)]
struct JsonSummary {
    errors: usize,
    warnings: usize,
    info: usize,
    hints: usize,
    files: usize,
}

impl From<&Summary> for JsonSummary {
    fn from(s: &Summary) -> Self {
        JsonSummary {
            errors: s.errors,
            warnings: s.warnings,
            info: s.info,
            hints: s.hints,
            files: s.files,
        }
    }
}

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    path: &'a str,
    line: u32,
    column: u32,
    end_line: u32,
    end_column: u32,
    severity: &'static str,
    code: String,
    message: &'a str,
    source: &'static str,
}

/// Falls back to an empty JSON document on serialisation failure — `Value`
/// built entirely from owned `String`/`&str`/numeric fields cannot
/// realistically fail to serialise, but a renderer returning `String` has no
/// error channel, so this is the one deliberate degrade-gracefully path.
pub fn render_json(entries: &[(String, Diagnostic)], summary: &Summary) -> String {
    let doc = JsonDocument {
        version: 1,
        summary: summary.into(),
        diagnostics: entries
            .iter()
            .map(|(path, d)| JsonDiagnostic {
                path,
                line: d.range.start.line + 1,
                column: d.range.start.character + 1,
                end_line: d.range.end.line + 1,
                end_column: d.range.end.character + 1,
                severity: severity_word(d),
                code: code_of(d),
                message: &d.message,
                source: "terraform-ls-rs",
            })
            .collect(),
    };
    serde_json::to_string_pretty(&doc).unwrap_or_else(|e| {
        format!(r#"{{"version":1,"error":"failed to serialise diagnostics: {e}"}}"#)
    })
}

#[derive(Serialize)]
struct SarifDocument {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Serialize)]
struct SarifRun {
    tool: SarifTool,
    results: Vec<SarifResult>,
}

#[derive(Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifDriver {
    name: &'static str,
    version: &'static str,
    information_uri: &'static str,
    rules: Vec<SarifRule>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRule {
    id: String,
    short_description: SarifText,
}

#[derive(Serialize)]
struct SarifText {
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule_id: String,
    level: &'static str,
    message: SarifText,
    locations: Vec<SarifLocation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifArtifactLocation {
    uri: String,
    uri_base_id: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

fn sarif_level(d: &Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::ERROR) | None => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        _ => "note",
    }
}

/// Repository URL surfaced in the SARIF `tool.driver.informationUri` field,
/// pulled from Cargo package metadata (`repository = "..."` in
/// `crates/tfls-cli/Cargo.toml`) rather than hardcoded.
const REPOSITORY_URI: &str = env!("CARGO_PKG_REPOSITORY");

pub fn render_sarif(entries: &[(String, Diagnostic)], _summary: &Summary) -> String {
    let mut rule_ids: Vec<String> = entries.iter().map(|(_, d)| code_of(d)).collect();
    rule_ids.sort();
    rule_ids.dedup();

    let rules = rule_ids
        .iter()
        .map(|id| SarifRule {
            id: id.clone(),
            short_description: SarifText { text: id.clone() },
        })
        .collect();

    let results = entries
        .iter()
        .map(|(path, d)| SarifResult {
            rule_id: code_of(d),
            level: sarif_level(d),
            message: SarifText {
                text: d.message.clone(),
            },
            locations: vec![SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        uri: path.replace('\\', "/"),
                        uri_base_id: "%SRCROOT%",
                    },
                    region: SarifRegion {
                        start_line: d.range.start.line + 1,
                        start_column: d.range.start.character + 1,
                        end_line: d.range.end.line + 1,
                        end_column: d.range.end.character + 1,
                    },
                },
            }],
        })
        .collect();

    let doc = SarifDocument {
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        version: "2.1.0",
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "tfls-lint",
                    version: env!("CARGO_PKG_VERSION"),
                    information_uri: REPOSITORY_URI,
                    rules,
                },
            },
            results,
        }],
    };
    serde_json::to_string_pretty(&doc).unwrap_or_else(|e| {
        format!(r#"{{"version":"2.1.0","error":"failed to serialise diagnostics: {e}"}}"#)
    })
}

/// Escapes `%`, `\r`, `\n` per the GitHub Actions workflow-command spec.
/// Order matters: `%` must go first so the later escapes' own `%` sequences
/// don't get re-escaped.
pub fn escape_workflow_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// As [`escape_workflow_data`], plus `:` and `,` — required for property
/// values (`file=`, `line=`, ...) but not for the trailing message.
pub fn escape_workflow_property(s: &str) -> String {
    escape_workflow_data(s)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

fn github_level(d: &Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::ERROR) | None => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        _ => "notice",
    }
}

pub fn render_github(entries: &[(String, Diagnostic)], _summary: &Summary) -> String {
    let mut out = String::new();
    for (path, d) in entries {
        let line = d.range.start.line + 1;
        let end_line = d.range.end.line + 1;
        let col = d.range.start.character + 1;
        let end_col = d.range.end.character + 1;
        out.push_str(&format!(
            "::{level} file={path},line={line},endLine={end_line},col={col},endColumn={end_col},title={title}::{message}\n",
            level = github_level(d),
            path = escape_workflow_property(path),
            title = escape_workflow_property(&code_of(d)),
            message = escape_workflow_data(&d.message),
        ));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(severity: DiagnosticSeverity, code: &str, message: &str, line: u32) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 2), Position::new(line, 5)),
            severity: Some(severity),
            code: Some(NumberOrString::String(code.to_string())),
            code_description: None,
            source: Some("terraform-ls-rs".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn fixture() -> (Vec<(String, Diagnostic)>, Summary) {
        let entries = vec![
            (
                "main.tf".to_string(),
                diag(
                    DiagnosticSeverity::ERROR,
                    "terraform_syntax",
                    "bad syntax",
                    0,
                ),
            ),
            (
                "vars.tf".to_string(),
                diag(
                    DiagnosticSeverity::WARNING,
                    "terraform_unused_declarations",
                    "unused variable",
                    3,
                ),
            ),
        ];
        let summary = Summary {
            errors: 1,
            warnings: 1,
            info: 0,
            hints: 0,
            files: 2,
        };
        (entries, summary)
    }

    #[test]
    fn escape_workflow_data_covers_all_five_characters() {
        let input = "100% done: line1\r\nline2, ok";
        let escaped = escape_workflow_data(input);
        assert_eq!(escaped, "100%25 done: line1%0D%0Aline2, ok");
    }

    #[test]
    fn escape_workflow_property_also_escapes_colon_and_comma() {
        let input = "100% done: line1\r\nline2, ok";
        let escaped = escape_workflow_property(input);
        assert_eq!(escaped, "100%25 done%3A line1%0D%0Aline2%2C ok");
    }

    #[test]
    fn render_text_matches_existing_format() {
        let (entries, summary) = fixture();
        let text = render_text(&entries, &summary);
        assert_eq!(
            text,
            "main.tf:1:3: error [terraform_syntax] bad syntax\n\
             vars.tf:4:3: warning [terraform_unused_declarations] unused variable\n"
        );
    }

    #[test]
    fn render_json_round_trips_expected_fields() {
        let (entries, summary) = fixture();
        let json = render_json(&entries, &summary);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["version"], 1);
        assert_eq!(value["summary"]["errors"], 1);
        assert_eq!(value["summary"]["warnings"], 1);
        assert_eq!(value["summary"]["files"], 2);
        assert_eq!(value["diagnostics"][0]["path"], "main.tf");
        assert_eq!(value["diagnostics"][0]["line"], 1);
        assert_eq!(value["diagnostics"][0]["column"], 3);
        assert_eq!(value["diagnostics"][0]["severity"], "error");
        assert_eq!(value["diagnostics"][0]["code"], "terraform_syntax");
        assert_eq!(value["diagnostics"][1]["severity"], "warning");
    }

    #[test]
    fn render_sarif_has_expected_shape() {
        let (entries, summary) = fixture();
        let sarif = render_sarif(&entries, &summary);
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("valid json");
        assert_eq!(
            value["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        assert_eq!(value["version"], "2.1.0");
        assert_eq!(value["runs"][0]["tool"]["driver"]["name"], "tfls-lint");
        let results = value["runs"][0]["results"]
            .as_array()
            .expect("results array");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["level"], "error");
        assert_eq!(results[1]["level"], "warning");
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uriBaseId"],
            "%SRCROOT%"
        );
    }

    #[test]
    fn render_sarif_maps_info_and_hint_to_note() {
        let entries = vec![
            (
                "main.tf".to_string(),
                diag(
                    DiagnosticSeverity::INFORMATION,
                    "terraform_fmt",
                    "not formatted",
                    0,
                ),
            ),
            (
                "main.tf".to_string(),
                diag(DiagnosticSeverity::HINT, "terraform_hint", "a hint", 1),
            ),
        ];
        let sarif = render_sarif(&entries, &Summary::default());
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("valid json");
        assert_eq!(value["runs"][0]["results"][0]["level"], "note");
        assert_eq!(value["runs"][0]["results"][1]["level"], "note");
    }

    #[test]
    fn render_github_produces_exact_lines() {
        let (entries, summary) = fixture();
        let github = render_github(&entries, &summary);
        assert_eq!(
            github,
            "::error file=main.tf,line=1,endLine=1,col=3,endColumn=6,title=terraform_syntax::bad syntax\n\
             ::warning file=vars.tf,line=4,endLine=4,col=3,endColumn=6,title=terraform_unused_declarations::unused variable\n"
        );
    }
}
