//! Benchmarks that exercise sonic-rs on a synthesized schema JSON
//! document roughly approximating the shape of a real provider's
//! `terraform providers schema -json` output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{Criterion, criterion_group, criterion_main};
use tfls_schema::ProviderSchemas;

fn synthesize_schema(n_resources: usize, n_attrs: usize) -> String {
    let mut resources = String::new();
    for i in 0..n_resources {
        let mut attrs = String::new();
        for a in 0..n_attrs {
            attrs.push_str(&format!(
                "\"attr_{a}\": {{\"type\":\"string\",\"description\":\"attr {a}\",\"optional\":true}}"
            ));
            if a + 1 != n_attrs {
                attrs.push(',');
            }
        }
        resources.push_str(&format!(
            "\"r_{i}\": {{\"version\": 1, \"block\": {{\"attributes\": {{{attrs}}}}}}}"
        ));
        if i + 1 != n_resources {
            resources.push(',');
        }
    }
    format!(
        r#"{{
            "format_version": "1.0",
            "provider_schemas": {{
                "registry.terraform.io/hashicorp/synth": {{
                    "provider": {{ "version": 0, "block": {{}} }},
                    "resource_schemas": {{ {resources} }},
                    "data_source_schemas": {{}}
                }}
            }}
        }}"#
    )
}

fn bench_deserialise(c: &mut Criterion) {
    let small = synthesize_schema(50, 20);
    let medium = synthesize_schema(200, 40);

    let mut group = c.benchmark_group("schema_deserialise");
    group.bench_function("50_resources_20_attrs", |b| {
        b.iter(|| {
            let _: ProviderSchemas = sonic_rs::from_str(&small).expect("parse");
        });
    });
    group.bench_function("200_resources_40_attrs", |b| {
        b.iter(|| {
            let _: ProviderSchemas = sonic_rs::from_str(&medium).expect("parse");
        });
    });
    group.finish();
}

criterion_group!(benches, bench_deserialise);
criterion_main!(benches);
