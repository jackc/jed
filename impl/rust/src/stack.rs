//! Native-stack budget helpers (spec/design/cost.md §7.1; CLAUDE.md §13).
//!
//! `MAX_EXPR_DEPTH` bounds how deep the parse / resolve / plan / eval recursions go; what each
//! level costs is the size of the stack frames on the recursive path. A function that matches over
//! many node kinds reserves one frame big enough for its largest arm, so every recursion through it
//! pays for every arm. [`outline`] moves an arm's body into its own frame, which is pushed only when
//! that arm runs.

/// Run `f` in a frame of its own that is never inlined into the caller.
#[inline(never)]
pub(crate) fn outline<R>(f: impl FnOnce() -> R) -> R {
    f()
}
