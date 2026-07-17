use diagnostic_reducer::{Budget, NoRedaction, ReductionOptions, TextInput, reduce};

fn main() {
    let stderr = b"src/main.go:12:4: undefined: total";
    let result = reduce(
        &[TextInput::new(stderr)],
        &ReductionOptions {
            budget: Budget {
                max_bytes: 4096,
                max_items: 20,
            },
            ..ReductionOptions::default()
        },
        &NoRedaction,
    );

    for diagnostic in result.diagnostics {
        println!("{}", diagnostic.message);
    }
}
