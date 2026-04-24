//! Extract identifier references (e.g. `var.region`, `local.x`,
//! `aws_instance.web.id`) from expressions in an hcl-edit AST, with
//! their LSP ranges for navigation.

use hcl_edit::expr::{Expression, Traversal, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use hcl_edit::template::{Directive, Element, Template};
use lsp_types::{Range, Url};
use ropey::Rope;
use serde::{Deserialize, Serialize};
use tfls_core::SymbolLocation;

use crate::position::hcl_span_to_lsp_range;

/// What a reference points to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    /// `var.<name>`
    Variable { name: String },
    /// `local.<name>`
    Local { name: String },
    /// `module.<name>`
    Module { name: String },
    /// `<resource_type>.<name>` (unqualified — first segment looks like a resource type)
    Resource { resource_type: String, name: String },
    /// `data.<type>.<name>`
    DataSource { resource_type: String, name: String },
}

/// A reference and the location where it appeared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub kind: ReferenceKind,
    pub location: SymbolLocation,
}

/// Extract references from a body, descending into blocks recursively.
pub fn extract_references(body: &Body, uri: &Url, rope: &Rope) -> Vec<Reference> {
    let mut out = Vec::new();
    visit_body(body, uri, rope, &mut out);
    out
}

fn visit_body(body: &Body, uri: &Url, rope: &Rope, out: &mut Vec<Reference>) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            visit_expression(&attr.value, uri, rope, out);
        } else if let Some(block) = structure.as_block() {
            visit_block(block, uri, rope, out);
        }
    }
}

fn visit_block(block: &Block, uri: &Url, rope: &Rope, out: &mut Vec<Reference>) {
    visit_body(&block.body, uri, rope, out);
}

fn visit_expression(expr: &Expression, uri: &Url, rope: &Rope, out: &mut Vec<Reference>) {
    match expr {
        Expression::Traversal(tv) => {
            if let Some(reference) = classify_traversal(tv, uri, rope) {
                out.push(reference);
            }
            // Descend into the base expression and any index operators.
            visit_expression(&tv.expr, uri, rope, out);
            for op in &tv.operators {
                if let TraversalOperator::Index(e) = op.value() {
                    visit_expression(e, uri, rope, out);
                }
            }
        }
        Expression::Array(array) => {
            for e in array.iter() {
                visit_expression(e, uri, rope, out);
            }
        }
        Expression::Object(object) => {
            for (_k, v) in object.iter() {
                visit_expression(v.expr(), uri, rope, out);
            }
        }
        Expression::Parenthesis(p) => visit_expression(p.inner(), uri, rope, out),
        Expression::FuncCall(fc) => {
            for e in fc.args.iter() {
                visit_expression(e, uri, rope, out);
            }
        }
        Expression::Conditional(c) => {
            visit_expression(&c.cond_expr, uri, rope, out);
            visit_expression(&c.true_expr, uri, rope, out);
            visit_expression(&c.false_expr, uri, rope, out);
        }
        Expression::UnaryOp(o) => visit_expression(&o.expr, uri, rope, out),
        Expression::BinaryOp(o) => {
            visit_expression(&o.lhs_expr, uri, rope, out);
            visit_expression(&o.rhs_expr, uri, rope, out);
        }
        Expression::ForExpr(f) => {
            visit_expression(&f.intro.collection_expr, uri, rope, out);
            if let Some(k) = f.key_expr.as_ref() {
                visit_expression(k, uri, rope, out);
            }
            visit_expression(&f.value_expr, uri, rope, out);
            if let Some(c) = f.cond.as_ref() {
                visit_expression(&c.expr, uri, rope, out);
            }
        }
        Expression::StringTemplate(t) => visit_template_elements(t.iter(), uri, rope, out),
        Expression::HeredocTemplate(h) => visit_template(&h.template, uri, rope, out),
        _ => {}
    }
}

fn visit_template(template: &Template, uri: &Url, rope: &Rope, out: &mut Vec<Reference>) {
    visit_template_elements(template.iter(), uri, rope, out);
}

fn visit_template_elements<'a, I>(elements: I, uri: &Url, rope: &Rope, out: &mut Vec<Reference>)
where
    I: IntoIterator<Item = &'a Element>,
{
    for element in elements {
        match element {
            Element::Literal(_) => {}
            Element::Interpolation(i) => visit_expression(&i.expr, uri, rope, out),
            Element::Directive(d) => match d.as_ref() {
                Directive::If(i) => {
                    visit_expression(&i.if_expr.cond_expr, uri, rope, out);
                    visit_template(&i.if_expr.template, uri, rope, out);
                    if let Some(else_part) = i.else_expr.as_ref() {
                        visit_template(&else_part.template, uri, rope, out);
                    }
                }
                Directive::For(f) => {
                    visit_expression(&f.for_expr.collection_expr, uri, rope, out);
                    visit_template(&f.for_expr.template, uri, rope, out);
                }
            },
        }
    }
}

/// Match `foo.bar` and `foo.bar.baz` traversal shapes against Terraform's
/// reference conventions.
fn classify_traversal(tv: &Traversal, uri: &Url, rope: &Rope) -> Option<Reference> {
    let base_ident = match &tv.expr {
        Expression::Variable(v) => v.as_str().to_string(),
        _ => return None,
    };

    // Gather `.ident.ident...` prefix only (stop at Index/Splat).
    let mut segments: Vec<&str> = Vec::new();
    for op in &tv.operators {
        match op.value() {
            TraversalOperator::GetAttr(ident) => segments.push(ident.as_str()),
            _ => break,
        }
    }

    let span = tv.span()?;
    let range = hcl_span_to_lsp_range(rope, span).ok()?;
    let location = location(uri, range);

    let kind = match (base_ident.as_str(), segments.as_slice()) {
        ("var", [name, ..]) => ReferenceKind::Variable {
            name: (*name).to_string(),
        },
        ("local", [name, ..]) => ReferenceKind::Local {
            name: (*name).to_string(),
        },
        ("module", [name, ..]) => ReferenceKind::Module {
            name: (*name).to_string(),
        },
        ("data", [type_, name, ..]) => ReferenceKind::DataSource {
            resource_type: (*type_).to_string(),
            name: (*name).to_string(),
        },
        // Any `<type>.<name>` that isn't a known prefix is a resource reference.
        (ty, [name, ..]) if !is_builtin_prefix(ty) && is_resource_type(ty) => {
            ReferenceKind::Resource {
                resource_type: ty.to_string(),
                name: (*name).to_string(),
            }
        }
        _ => return None,
    };

    Some(Reference { kind, location })
}

fn location(uri: &Url, range: Range) -> SymbolLocation {
    SymbolLocation::new(uri.clone(), range)
}

fn is_builtin_prefix(s: &str) -> bool {
    matches!(
        s,
        "var" | "local" | "module" | "data" | "path" | "terraform" | "each" | "count" | "self"
    )
}

/// Treat identifiers that look like Terraform resource type names (contain
/// an underscore, e.g. `aws_instance`) as resource references. This is a
/// heuristic — full resolution requires provider schemas.
fn is_resource_type(s: &str) -> bool {
    s.contains('_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::parse::parse_source;

    fn uri() -> Url {
        Url::parse("file:///test.tf").expect("valid url")
    }

    fn refs(src: &str) -> Vec<Reference> {
        let rope = Rope::from_str(src);
        let parsed = parse_source(src);
        let body = parsed.body.expect("should parse");
        extract_references(&body, &uri(), &rope)
    }

    #[test]
    fn finds_variable_reference() {
        let refs = refs(r#"resource "aws_instance" "x" { ami = var.ami_id }"#);
        assert!(refs.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::Variable { name } if name == "ami_id"
        )));
    }

    #[test]
    fn finds_local_reference() {
        let refs = refs(r#"output "x" { value = local.name }"#);
        assert!(refs.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::Local { name } if name == "name"
        )));
    }

    #[test]
    fn finds_module_reference() {
        let refs = refs(r#"output "x" { value = module.network.subnet_id }"#);
        assert!(refs.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::Module { name } if name == "network"
        )));
    }

    #[test]
    fn finds_data_reference() {
        let refs = refs(r#"output "x" { value = data.aws_ami.ubuntu.id }"#);
        assert!(refs.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::DataSource { resource_type, name }
                if resource_type == "aws_ami" && name == "ubuntu"
        )));
    }

    #[test]
    fn finds_resource_reference() {
        let refs = refs(r#"output "x" { value = aws_instance.web.id }"#);
        assert!(refs.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::Resource { resource_type, name }
                if resource_type == "aws_instance" && name == "web"
        )));
    }

    #[test]
    fn skips_unrecognised_bases() {
        let refs = refs(r#"output "x" { value = count.index }"#);
        // `count.index` is a builtin, not a user symbol, so it should not be a reference.
        assert!(!refs
            .iter()
            .any(|r| matches!(&r.kind, ReferenceKind::Variable { .. })));
    }

    #[test]
    fn finds_reference_inside_string_interpolation() {
        // `"${var.region}"` is a StringTemplate containing an
        // Interpolation whose expression is `var.region`. The
        // old visit_expression bailed on the StringTemplate arm
        // and lost every reference inside — causing every var
        // used only in interpolated strings to be falsely
        // flagged "declared but not used".
        let refs = refs(r#"output "x" { value = "hi ${var.region}" }"#);
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "region"
            )),
            "expected var.region from string interpolation; got {refs:?}"
        );
    }

    #[test]
    fn finds_reference_inside_heredoc_interpolation() {
        let src = "output \"x\" {\n  value = <<-EOT\n    hi ${var.greeting}\n  EOT\n}\n";
        let refs = refs(src);
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "greeting"
            )),
            "expected var.greeting from heredoc; got {refs:?}"
        );
    }

    #[test]
    fn finds_reference_inside_template_if_directive() {
        // Template `%{ if ... }%{ endif }` — condition + body
        // must both be scanned.
        let src = r#"output "x" { value = "%{ if var.enabled }${var.payload}%{ endif }" }"#;
        let refs = refs(src);
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "enabled"
            )),
            "expected var.enabled from if-cond: {refs:?}"
        );
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "payload"
            )),
            "expected var.payload from if-body: {refs:?}"
        );
    }

    #[test]
    fn finds_reference_inside_for_expr_key_and_cond() {
        // `{ for k, v in local.m : k => var.suffix if var.want }`
        // — all three of key_expr, value_expr, cond were ignored.
        let src = r#"output "x" { value = { for k, v in local.m : k => var.suffix if var.want } }"#;
        let refs = refs(src);
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Local { name } if name == "m"
            )),
            "expected local.m: {refs:?}"
        );
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "suffix"
            )),
            "expected var.suffix (value_expr): {refs:?}"
        );
        assert!(
            refs.iter().any(|r| matches!(
                &r.kind,
                ReferenceKind::Variable { name } if name == "want"
            )),
            "expected var.want (cond): {refs:?}"
        );
    }
}
