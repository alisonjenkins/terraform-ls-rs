//! Parsing + symbol-extraction benchmarks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{Criterion, criterion_group, criterion_main};
use lsp_types::{Position, Url};
use ropey::Rope;
use tfls_parser::{
    extract_references, extract_symbols, lsp_position_to_byte_offset, parse_source,
};

fn synthesize(n_blocks: usize) -> String {
    let mut s = String::with_capacity(n_blocks * 100);
    for i in 0..n_blocks {
        s.push_str(&format!(
            r#"
resource "aws_instance" "inst_{i}" {{
  ami           = "ami-0abcdef1234567890"
  instance_type = var.size_{i}
  tags = {{
    Name = "inst_{i}"
    Env  = local.env
  }}
}}
"#
        ));
    }
    for i in 0..n_blocks {
        s.push_str(&format!("variable \"size_{i}\" {{ default = \"t3.micro\" }}\n"));
    }
    s.push_str("locals {\n  env = \"prod\"\n}\n");
    s
}

fn bench_parse(c: &mut Criterion) {
    let src = synthesize(100);
    let mut group = c.benchmark_group("parse");
    group.bench_function("100_blocks", |b| {
        b.iter(|| {
            let _ = parse_source(&src);
        });
    });
    group.finish();
}

fn bench_extract_symbols(c: &mut Criterion) {
    let src = synthesize(100);
    let rope = Rope::from_str(&src);
    let uri = Url::parse("file:///b.tf").expect("uri");
    let body = parse_source(&src).body.expect("parses");

    c.bench_function("extract_symbols_100_blocks", |b| {
        b.iter(|| {
            let _ = extract_symbols(&body, &uri, &rope);
        });
    });
}

fn bench_extract_references(c: &mut Criterion) {
    let src = synthesize(100);
    let rope = Rope::from_str(&src);
    let uri = Url::parse("file:///b.tf").expect("uri");
    let body = parse_source(&src).body.expect("parses");

    c.bench_function("extract_references_100_blocks", |b| {
        b.iter(|| {
            let _ = extract_references(&body, &uri, &rope);
        });
    });
}

fn bench_position_roundtrip(c: &mut Criterion) {
    let src = synthesize(100);
    let rope = Rope::from_str(&src);
    let positions: Vec<Position> = (0..rope.len_lines() as u32)
        .step_by(3)
        .map(|line| Position::new(line, 0))
        .collect();

    c.bench_function("lsp_position_to_byte_offset_many", |b| {
        b.iter(|| {
            for p in &positions {
                let _ = lsp_position_to_byte_offset(&rope, *p);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_parse,
    bench_extract_symbols,
    bench_extract_references,
    bench_position_roundtrip
);
criterion_main!(benches);
