#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{criterion_group, criterion_main, Criterion};
use tfls_core::completion::classify_context;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("classify_context_top_level", |b| {
        let src = "";
        b.iter(|| classify_context(src, src.len()))
    });

    c.bench_function("classify_context_resource_body", |b| {
        let src = "resource \"aws_instance\" \"web\" {\n  ";
        b.iter(|| classify_context(src, src.len()))
    });

    c.bench_function("classify_context_variable_ref", |b| {
        let src = "output \"x\" { value = var.";
        b.iter(|| classify_context(src, src.len()))
    });

    c.bench_function("classify_context_deep_path", |b| {
        let src = "output \"x\" { value = var.foo.bar.baz.qux.";
        b.iter(|| classify_context(src, src.len()))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

