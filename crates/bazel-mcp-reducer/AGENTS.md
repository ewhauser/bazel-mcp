# Reducer crate instructions

Reducers are deterministic, bounded, and independent of storage and async
runtime concerns. Prefer root causes, preserve stable ordering, normalize only
exact duplicates, and apply serialized byte budgets before returning. Review
golden diagnostic diffs as product behavior, not mechanical snapshots.
