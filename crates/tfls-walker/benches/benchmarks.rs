#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{criterion_group, criterion_main, Criterion};
use std::fs;
use tempfile::tempdir;
use tfls_walker::discovery::discover_terraform_files;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("discover_terraform_files", |b| {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/main.tf"), "").unwrap();
        fs::write(root.join("a/b/variables.tf"), "").unwrap();
        fs::write(root.join("a/b/outputs.tf"), "").unwrap();

        b.iter(|| {
            discover_terraform_files(root).unwrap();
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
