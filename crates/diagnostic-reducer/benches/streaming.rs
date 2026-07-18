use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use diagnostic_reducer::{
    Budget, BuiltinParserOptions, EndReason, GenericRanker, NoPathMapping, NoRedaction,
    OutputPolicy, ReductionOptions, ReductionSession, Scope, SessionOptions, Stream, TextInput,
    builtin_parser_plan, reduce,
};

fn corpus(lines: usize, matches: bool) -> Vec<u8> {
    let mut output = String::new();
    for index in 0..lines {
        if matches && index % 1_000 == 999 {
            output.push_str(&format!(
                "src/file{index}.cc:{}:7: error: missing value {index}\n",
                index + 1
            ));
        } else {
            output.push_str(&format!("[{index}] ordinary command progress output\n"));
        }
    }
    output.into_bytes()
}

fn stream(input: &[u8], chunk_size: usize) {
    let mut session = ReductionSession::new(
        builtin_parser_plan(BuiltinParserOptions::default()),
        SessionOptions {
            budget: Budget {
                max_bytes: 64 * 1024,
                max_items: 100,
            },
            ..SessionOptions::default()
        },
        OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
    );
    session.begin_scope(Scope::step("benchmark"));
    for chunk in input.chunks(chunk_size) {
        session.push_chunk("benchmark", Stream::Stderr, black_box(chunk));
    }
    session.end_scope("benchmark", EndReason::Complete);
    black_box(session.finish());
}

fn generic_reducers(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("generic_reducers");
    for (name, input) in [
        ("no-match-tail", corpus(25_000, false)),
        ("mixed-tail", corpus(25_000, true)),
    ] {
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_function(format!("batch/{name}"), |bencher| {
            bencher.iter(|| {
                black_box(reduce(
                    &[TextInput::new(black_box(&input))],
                    &ReductionOptions {
                        budget: Budget {
                            max_bytes: 64 * 1024,
                            max_items: 100,
                        },
                        ..ReductionOptions::default()
                    },
                    &NoRedaction,
                ));
            });
        });
        group.bench_function(format!("stream-64k/{name}"), |bencher| {
            bencher.iter(|| stream(&input, 64 * 1024));
        });
        group.bench_function(format!("stream-1k/{name}"), |bencher| {
            bencher.iter(|| stream(&input, 1024));
        });
    }
    group.finish();
}

criterion_group!(benches, generic_reducers);
criterion_main!(benches);
