#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{criterion_group, criterion_main, Criterion};
use lsp_types::Url;
use tfls_state::{document::DocumentState, store::StateStore};

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("upsert_document", |b| {
        let store = StateStore::new();
        let uri = Url::parse("file:///test.tf").unwrap();
        let content = r#"
            variable "region" {
                type = string
            }

            resource "aws_instance" "web" {
                ami           = "ami-0c55b159cbfafe1f0"
                instance_type = "t2.micro"
            }
        "#;

        b.iter(|| {
            let doc = DocumentState::new(uri.clone(), content, 1);
            store.upsert_document(doc);
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
