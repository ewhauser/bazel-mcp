/// Infallible transformation applied to all untrusted returned strings.
pub trait Redactor {
    fn redact(&self, value: &str) -> String;
}

impl<F> Redactor for F
where
    F: Fn(&str) -> String,
{
    fn redact(&self, value: &str) -> String {
        self(value)
    }
}

/// Explicit identity redactor for already-sanitized or non-sensitive inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRedaction;

impl Redactor for NoRedaction {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}
