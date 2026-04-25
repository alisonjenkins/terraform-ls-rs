#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{criterion_group, criterion_main, Criterion};
use tfls_diag::syntax::diagnostics_for_parse_errors;
use tfls_parser::parse_source;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("diagnostics_for_parse_errors", |b| {
        let src = "resource \"aws_instance\" \"web\" {\n  ami = \n}\n";
        let parsed = parse_source(src);
        b.iter(|| diagnostics_for_parse_errors(&parsed.errors))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

