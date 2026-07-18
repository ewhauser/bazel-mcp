use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::{
    Diagnostic, EndReason, LogLine, OutputPolicy, Provenance, Reduction, ReductionStats, Scope,
    SessionOptions, Stream, TestFailure, finalize_findings, normalize_terminal_text,
};

/// Parser lifecycle notification for a caller-owned scope.
#[derive(Clone, Debug)]
pub enum ScopeBoundary<'a> {
    Begin(&'a Scope),
    End {
        scope: &'a Scope,
        reason: EndReason,
        input_truncated: bool,
    },
}

/// Deterministic, synchronous parser contract.
pub trait Parser: Send {
    fn id(&self) -> &'static str;

    fn observe(&mut self, line: &LogLine<'_>, emitter: &mut Emitter<'_>);

    fn boundary(&mut self, _boundary: ScopeBoundary<'_>, _emitter: &mut Emitter<'_>) {}

    fn finish(&mut self, _emitter: &mut Emitter<'_>) {}
}

/// Immutable ordered set of trusted native parsers.
#[derive(Default)]
pub struct ParserPlan {
    parsers: Vec<Box<dyn Parser>>,
    ids: BTreeSet<&'static str>,
}

impl ParserPlan {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push<P: Parser + 'static>(&mut self, parser: P) -> Result<(), ParserPlanError> {
        let id = parser.id();
        if id.is_empty() {
            return Err(ParserPlanError::EmptyId);
        }
        if !self.ids.insert(id) {
            return Err(ParserPlanError::DuplicateId(id));
        }
        self.parsers.push(Box::new(parser));
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.parsers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parsers.is_empty()
    }
}

/// Invalid immutable parser-plan construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParserPlanError {
    EmptyId,
    DuplicateId(&'static str),
}

impl Display for ParserPlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("parser identifiers must not be empty"),
            Self::DuplicateId(id) => write!(formatter, "duplicate parser identifier {id:?}"),
        }
    }
}

impl Error for ParserPlanError {}

#[derive(Default)]
struct CandidateStore {
    diagnostics: Vec<Diagnostic>,
    test_failures: Vec<TestFailure>,
    dropped: u64,
}

/// Bounded parser output sink. Contextual provenance is supplied by the engine.
pub struct Emitter<'a> {
    store: &'a mut CandidateStore,
    maximum: usize,
    provenance: Option<Provenance>,
}

impl Emitter<'_> {
    pub fn diagnostic(&mut self, mut diagnostic: Diagnostic) {
        if self
            .store
            .diagnostics
            .len()
            .saturating_add(self.store.test_failures.len())
            >= self.maximum
        {
            self.store.dropped = self.store.dropped.saturating_add(1);
            return;
        }
        if diagnostic.provenance.is_none() {
            diagnostic.provenance = self.provenance.clone();
        }
        self.store.diagnostics.push(diagnostic);
    }

    pub fn test_failure(&mut self, mut failure: TestFailure) {
        if self
            .store
            .diagnostics
            .len()
            .saturating_add(self.store.test_failures.len())
            >= self.maximum
        {
            self.store.dropped = self.store.dropped.saturating_add(1);
            return;
        }
        if failure.provenance.is_none() {
            failure.provenance = self.provenance.clone();
        }
        self.store.test_failures.push(failure);
    }
}

#[derive(Clone, Debug)]
struct ScopeState {
    scope: Scope,
    retained_bytes: usize,
    truncated: bool,
    stream_lines: BTreeMap<Stream, u64>,
}

#[derive(Clone, Debug, Default)]
struct LineBuffer {
    bytes: Vec<u8>,
    truncated: bool,
    previous_was_cr: bool,
}

/// Incremental, chunk-invariant reduction session.
pub struct ReductionSession<'a> {
    plan: ParserPlan,
    options: SessionOptions,
    policy: OutputPolicy<'a>,
    scopes: BTreeMap<String, ScopeState>,
    buffers: BTreeMap<(String, Stream), LineBuffer>,
    candidates: CandidateStore,
    stats: ReductionStats,
    next_ordinal: u64,
}

impl<'a> ReductionSession<'a> {
    #[must_use]
    pub fn new(plan: ParserPlan, options: SessionOptions, policy: OutputPolicy<'a>) -> Self {
        Self {
            plan,
            options,
            policy,
            scopes: BTreeMap::new(),
            buffers: BTreeMap::new(),
            candidates: CandidateStore::default(),
            stats: ReductionStats::default(),
            next_ordinal: 0,
        }
    }

    pub fn begin_scope(&mut self, scope: Scope) -> bool {
        if self.scopes.contains_key(&scope.id)
            || self.scopes.len() >= self.options.limits.max_scopes
        {
            self.stats.candidates_dropped = self.stats.candidates_dropped.saturating_add(1);
            return false;
        }
        let id = scope.id.clone();
        self.scopes.insert(
            id,
            ScopeState {
                scope: scope.clone(),
                retained_bytes: 0,
                truncated: false,
                stream_lines: BTreeMap::new(),
            },
        );
        self.stats.scopes_started = self.stats.scopes_started.saturating_add(1);
        self.dispatch_boundary(ScopeBoundary::Begin(&scope));
        true
    }

    pub fn push_chunk(&mut self, scope_id: &str, stream: Stream, chunk: &[u8]) -> bool {
        let Some(state) = self.scopes.get_mut(scope_id) else {
            return false;
        };
        self.stats.bytes_seen = self
            .stats
            .bytes_seen
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let available = self
            .options
            .limits
            .max_scope_bytes
            .saturating_sub(state.retained_bytes);
        let retained = chunk.len().min(available);
        state.retained_bytes = state.retained_bytes.saturating_add(retained);
        self.stats.bytes_retained = self
            .stats
            .bytes_retained
            .saturating_add(u64::try_from(retained).unwrap_or(u64::MAX));
        if retained < chunk.len() {
            state.truncated = true;
        }

        let key = (scope_id.to_owned(), stream);
        let buffer = self.buffers.entry(key).or_default();
        let mut completed = Vec::<(Vec<u8>, bool)>::new();
        for byte in &chunk[..retained] {
            let boundary = *byte == b'\n' || *byte == b'\r';
            if boundary {
                if *byte == b'\n' && buffer.previous_was_cr {
                    buffer.previous_was_cr = false;
                    continue;
                }
                completed.push((std::mem::take(&mut buffer.bytes), buffer.truncated));
                buffer.truncated = false;
                buffer.previous_was_cr = *byte == b'\r';
            } else {
                buffer.previous_was_cr = false;
                if buffer.bytes.len() < self.options.limits.max_line_bytes {
                    buffer.bytes.push(*byte);
                } else {
                    buffer.truncated = true;
                }
            }
        }
        for (bytes, truncated) in completed {
            self.dispatch_bytes(scope_id, stream, bytes, truncated);
        }
        true
    }

    /// Adds a trusted structured finding to the same redaction and budget path.
    pub fn emit_structured(&mut self, scope_id: &str, mut diagnostic: Diagnostic) -> bool {
        let Some(state) = self.scopes.get(scope_id) else {
            return false;
        };
        if diagnostic.provenance.is_none() {
            diagnostic.provenance = Some(
                Provenance::new("structured")
                    .with_scope(&state.scope)
                    .with_parser("adapter"),
            );
        }
        let mut emitter = Emitter {
            store: &mut self.candidates,
            maximum: self.options.limits.max_candidates,
            provenance: None,
        };
        emitter.diagnostic(diagnostic);
        true
    }

    pub fn end_scope(&mut self, scope_id: &str, reason: EndReason) -> bool {
        let Some(mut state) = self.scopes.remove(scope_id) else {
            return false;
        };
        let keys = self
            .buffers
            .keys()
            .filter(|(id, _)| id == scope_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(buffer) = self.buffers.remove(&key)
                && (!buffer.bytes.is_empty() || buffer.truncated)
            {
                self.scopes.insert(scope_id.to_owned(), state.clone());
                self.dispatch_bytes(scope_id, key.1, buffer.bytes, buffer.truncated);
                state = self
                    .scopes
                    .remove(scope_id)
                    .expect("temporary scope state must remain present");
            }
        }
        let effective_reason = if state.truncated && reason == EndReason::Complete {
            EndReason::Truncated
        } else {
            reason
        };
        self.dispatch_boundary(ScopeBoundary::End {
            scope: &state.scope,
            reason: effective_reason,
            input_truncated: state.truncated,
        });
        self.stats.scopes_completed = self.stats.scopes_completed.saturating_add(1);
        true
    }

    #[must_use]
    pub fn finish(mut self) -> Reduction {
        let scope_ids = self.scopes.keys().cloned().collect::<Vec<_>>();
        for scope_id in scope_ids {
            self.end_scope(&scope_id, EndReason::Interrupted);
        }
        for parser in &mut self.plan.parsers {
            let provenance = Some(Provenance::new("parser").with_parser(parser.id()));
            let mut emitter = Emitter {
                store: &mut self.candidates,
                maximum: self.options.limits.max_candidates,
                provenance,
            };
            parser.finish(&mut emitter);
        }
        self.stats.candidates_emitted = u64::try_from(
            self.candidates
                .diagnostics
                .len()
                .saturating_add(self.candidates.test_failures.len()),
        )
        .unwrap_or(u64::MAX);
        self.stats.candidates_dropped = self
            .stats
            .candidates_dropped
            .saturating_add(self.candidates.dropped);
        finalize_findings(
            self.candidates.diagnostics,
            self.candidates.test_failures,
            self.options.budget,
            self.policy,
            self.stats,
        )
    }

    fn dispatch_bytes(&mut self, scope_id: &str, stream: Stream, bytes: Vec<u8>, truncated: bool) {
        let normalized = normalize_terminal_text(&bytes);
        if normalized.is_empty() {
            return;
        }
        for text in normalized.lines() {
            let Some(state) = self.scopes.get_mut(scope_id) else {
                return;
            };
            let stream_line = state.stream_lines.entry(stream).or_default();
            *stream_line = stream_line.saturating_add(1);
            let stream_line = *stream_line;
            let scope = state.scope.clone();
            let ordinal = self.next_ordinal;
            self.next_ordinal = self.next_ordinal.saturating_add(1);
            self.stats.lines_seen = self.stats.lines_seen.saturating_add(1);
            if truncated {
                self.stats.truncated_lines = self.stats.truncated_lines.saturating_add(1);
                state.truncated = true;
            }
            let line = LogLine {
                scope: &scope,
                stream,
                ordinal,
                stream_line,
                text,
                truncated,
            };
            for parser in &mut self.plan.parsers {
                let provenance = Some(
                    Provenance::new(stream_name(stream))
                        .with_scope(&scope)
                        .with_span(stream, stream_line, stream_line)
                        .with_parser(parser.id()),
                );
                let mut emitter = Emitter {
                    store: &mut self.candidates,
                    maximum: self.options.limits.max_candidates,
                    provenance,
                };
                parser.observe(&line, &mut emitter);
            }
        }
    }

    fn dispatch_boundary(&mut self, boundary: ScopeBoundary<'_>) {
        for parser in &mut self.plan.parsers {
            let scope = match &boundary {
                ScopeBoundary::Begin(scope) | ScopeBoundary::End { scope, .. } => scope,
            };
            let provenance = Some(
                Provenance::new("boundary")
                    .with_scope(scope)
                    .with_parser(parser.id()),
            );
            let mut emitter = Emitter {
                store: &mut self.candidates,
                maximum: self.options.limits.max_candidates,
                provenance,
            };
            parser.boundary(boundary.clone(), &mut emitter);
        }
    }
}

fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
        Stream::Combined => "combined",
        Stream::Annotation => "annotation",
        Stream::Structured => "structured",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Budget, DiagnosticClass, EvidenceQuality, GenericRanker, NoPathMapping, NoRedaction,
        OutputPolicy, Severity,
    };

    #[derive(Default)]
    struct ErrorParser;

    impl Parser for ErrorParser {
        fn id(&self) -> &'static str {
            "test.error"
        }

        fn observe(&mut self, line: &LogLine<'_>, emitter: &mut Emitter<'_>) {
            if line.text.contains("error:") {
                emitter.diagnostic(Diagnostic {
                    severity: Severity::Error,
                    class: DiagnosticClass::Tool,
                    code: Some("test.error".to_owned()),
                    message: line.text.to_owned(),
                    location: None,
                    provenance: None,
                    quality: EvidenceQuality::Structured,
                    repetition_count: 1,
                });
            }
        }
    }

    fn run(chunks: &[&[u8]]) -> Reduction {
        let mut plan = ParserPlan::new();
        plan.push(ErrorParser).unwrap();
        let mut session = ReductionSession::new(
            plan,
            SessionOptions {
                budget: Budget::unbounded(),
                ..SessionOptions::default()
            },
            OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
        );
        assert!(session.begin_scope(Scope::step("compile")));
        for chunk in chunks {
            assert!(session.push_chunk("compile", Stream::Stderr, chunk));
        }
        assert!(session.end_scope("compile", EndReason::Complete));
        session.finish()
    }

    #[test]
    fn arbitrary_chunking_produces_the_same_findings() {
        let whole = run(&[b"note\nerror: broken\n"]);
        let split = run(&[b"no", b"te\ner", b"ror: bro", b"ken\n"]);
        assert_eq!(whole.diagnostics, split.diagnostics);
        assert_eq!(whole.diagnostics.len(), 1);
    }

    #[test]
    fn enforces_scope_and_line_bounds() {
        let mut plan = ParserPlan::new();
        plan.push(ErrorParser).unwrap();
        let mut session = ReductionSession::new(
            plan,
            SessionOptions {
                budget: Budget::unbounded(),
                limits: crate::Limits {
                    max_scope_bytes: 12,
                    max_line_bytes: 8,
                    ..crate::Limits::default()
                },
            },
            OutputPolicy::new(&NoRedaction, &NoPathMapping, &GenericRanker),
        );
        session.begin_scope(Scope::step("compile"));
        session.push_chunk("compile", Stream::Stderr, b"error: this line is too long\n");
        session.end_scope("compile", EndReason::Complete);
        let reduction = session.finish();
        assert!(reduction.truncated);
        assert!(reduction.stats.bytes_seen > reduction.stats.bytes_retained);
    }

    #[test]
    fn rejects_duplicate_parser_ids() {
        let mut plan = ParserPlan::new();
        plan.push(ErrorParser).unwrap();
        assert_eq!(
            plan.push(ErrorParser).unwrap_err(),
            ParserPlanError::DuplicateId("test.error")
        );
    }
}
