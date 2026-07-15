use std::hint::black_box;

use bazel_mcp_reducer::{Budget, deduplicate_lines, reduce_query};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn reduction(c: &mut Criterion) {
    let mut group = c.benchmark_group("reduction");
    for lines in [100_usize, 10_000, 100_000] {
        let input = "warning: repeated\n".repeat(lines);
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("deduplicate", lines),
            &input,
            |b, input| {
                b.iter(|| deduplicate_lines(black_box(input)));
            },
        );
        group.bench_with_input(BenchmarkId::new("query", lines), &input, |b, input| {
            b.iter(|| reduce_query(black_box(input.as_bytes()), Budget::inspect_default()));
        });
    }
    group.finish();
}

criterion_group!(benches, reduction);
criterion_main!(benches);
