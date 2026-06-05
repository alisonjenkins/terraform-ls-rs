#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{criterion_group, criterion_main, Criterion};
use tfls_core::completion::classify_context;
use tfls_core::lock_file;

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

    // 10-provider lock file — representative of a typical
    // multi-provider workspace.
    let lock_src = generate_lock_fixture(10);
    c.bench_function("lock_file_parse_10_providers", |b| {
        b.iter(|| lock_file::parse(&lock_src))
    });
}

fn generate_lock_fixture(n: usize) -> String {
    let mut out = String::new();
    for i in 0..n {
        out.push_str(&format!(
            "provider \"registry.terraform.io/hashicorp/p{i}\" {{\n  version     = \"{}.{}.{}\"\n  constraints = \"~> {}.0\"\n  hashes = [\"h1:abc\", \"zh:def\"]\n}}\n",
            i + 1,
            (i * 7) % 100,
            (i * 13) % 100,
            i + 1
        ));
    }
    out
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
