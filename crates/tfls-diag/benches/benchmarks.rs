#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{Criterion, criterion_group, criterion_main};
use ropey::Rope;
use tfls_diag::syntax::diagnostics_for_parse_errors;
use tfls_diag::{
    deprecated_null_resource_diagnostics, deprecated_template_dir_diagnostics,
    deprecated_template_file_diagnostics,
};
use tfls_parser::parse_source;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("diagnostics_for_parse_errors", |b| {
        let src = "resource \"aws_instance\" \"web\" {\n  ami = \n}\n";
        let parsed = parse_source(src);
        b.iter(|| diagnostics_for_parse_errors(&parsed.errors))
    });
}

/// Synthetic module body with `n` blocks of a single
/// deprecated kind, padded with reference + locals usage to
/// exercise the body walker realistically.
fn synth_deprecation_body(n: usize, kind: &str, label: &str) -> String {
    let mut src = String::with_capacity(n * 200);
    src.push_str("terraform { required_version = \">= 1.5\" }\n");
    for i in 0..n {
        match (kind, label) {
            ("resource", "null_resource") => {
                src.push_str(&format!(
                    "resource \"null_resource\" \"r{i}\" {{\n  triggers = {{ k = \"v{i}\" }}\n}}\n"
                ));
            }
            ("data", "template_file") => {
                src.push_str(&format!(
                    "data \"template_file\" \"t{i}\" {{\n  template = \"hi ${{name}}\"\n  vars = {{ name = \"x{i}\" }}\n}}\n"
                ));
            }
            ("data", "template_dir") => {
                src.push_str(&format!(
                    "data \"template_dir\" \"d{i}\" {{\n  source_dir = \"./tpls{i}\"\n  destination_dir = \"./out{i}\"\n}}\n"
                ));
            }
            _ => unreachable!(),
        }
    }
    // Sprinkle a few unrelated blocks so the walker has to skip
    // matter as well as match.
    for i in 0..(n / 10).max(1) {
        src.push_str(&format!(
            "resource \"aws_instance\" \"web{i}\" {{ ami = \"a\" }}\n"
        ));
    }
    src
}

fn bench_deprecation_walks(c: &mut Criterion) {
    let mut group = c.benchmark_group("deprecation_body_walk");
    for n in [10usize, 100, 1000] {
        // null_resource — `resource "null_resource"` block walk.
        let src = synth_deprecation_body(n, "resource", "null_resource");
        let rope = Rope::from_str(&src);
        let body = parse_source(&src).body.expect("parses");
        group.bench_function(format!("null_resource/{n}_blocks"), |b| {
            b.iter(|| deprecated_null_resource_diagnostics(&body, &rope))
        });

        // template_file — `data "template_file"` block walk.
        let src = synth_deprecation_body(n, "data", "template_file");
        let rope = Rope::from_str(&src);
        let body = parse_source(&src).body.expect("parses");
        group.bench_function(format!("template_file/{n}_blocks"), |b| {
            b.iter(|| deprecated_template_file_diagnostics(&body, &rope))
        });

        // template_dir — `data "template_dir"` block walk.
        let src = synth_deprecation_body(n, "data", "template_dir");
        let rope = Rope::from_str(&src);
        let body = parse_source(&src).body.expect("parses");
        group.bench_function(format!("template_dir/{n}_blocks"), |b| {
            b.iter(|| deprecated_template_dir_diagnostics(&body, &rope))
        });
    }
    group.finish();
}

criterion_group!(benches, criterion_benchmark, bench_deprecation_walks);
criterion_main!(benches);

