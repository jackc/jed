//! Parameter-sensitive ("custom") planning (spec/design/estimator.md §6.1).
//!
//! Planning runs inside a **frame** that says what the estimator may know about each `$N`:
//!
//! - **Probe**: the values are not bound yet. A parameter is a plan-time literal of unknown value:
//!   the estimator follows its literal path, and the first read of a parameter's *value* records
//!   that the statement is parameter-sensitive and takes the generic (unbound) estimate. A NULL test
//!   of a parameter answers "not NULL" and records that the plan assumed so.
//! - **Custom**: the bound values, as the constants a literal of each parameter's resolved type would
//!   resolve to. Every estimator read sees the value, so the plan is the custom plan.
//!
//! A probe plan that recorded nothing never consulted a value, so it is the custom plan for every
//! non-NULL binding (and for NULL ones too when no NULL test fired). Each plan→bind boundary plans in
//! a probe frame, binds, and plans again in a custom frame only when the probe was not value-blind.
//! Frames are thread-local and strictly nested; planning never crosses threads.

use super::*;
use std::cell::RefCell;

enum Frame {
    Probe {
        sensitive: bool,
        null_read: bool,
        /// Every type a `$N` was given during this frame (the EXPLAIN typing pass reads it).
        types: Vec<Option<ScalarType>>,
    },
    Custom {
        values: std::rc::Rc<Vec<Value>>,
        /// A restarted statement adopts this frame at its own `begin` instead of probing again.
        adopt: bool,
    },
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// The plan-time constant of bound parameter `i`, or `None` when the value is unavailable (a probe
/// frame — which records the statement as parameter-sensitive — no frame, or a value with no
/// literal form).
pub(crate) fn param_value(i: usize) -> Option<RExpr> {
    FRAMES.with(|frames| match frames.borrow_mut().last_mut() {
        Some(Frame::Probe { sensitive, .. }) => {
            *sensitive = true;
            None
        }
        Some(Frame::Custom { values, .. }) => values.get(i).and_then(value_const),
        None => None,
    })
}

/// Whether bound parameter `i` is NULL. An unbound parameter is assumed non-NULL; a probe frame
/// records the assumption so a NULL binding is planned again with its value.
pub(crate) fn param_is_null(i: usize) -> bool {
    FRAMES.with(|frames| match frames.borrow_mut().last_mut() {
        Some(Frame::Probe { null_read, .. }) => {
            *null_read = true;
            false
        }
        Some(Frame::Custom { values, .. }) => matches!(values.get(i), Some(Value::Null)),
        None => false,
    })
}

/// Record the type a probe frame's resolution gave `$(idx0+1)` (`ParamTypes::note`; `None` = no
/// context here, which still makes the slot exist).
pub(crate) fn note_type(idx0: usize, ty: Option<ScalarType>) {
    FRAMES.with(|frames| {
        if let Some(Frame::Probe { types, .. }) = frames.borrow_mut().last_mut() {
            if idx0 >= types.len() {
                types.resize(idx0 + 1, None);
            }
            if let Some(ty) = ty {
                types[idx0] = Some(match types[idx0] {
                    Some(old) => unify_param_type(old, ty, idx0).unwrap_or(ty),
                    None => ty,
                });
            }
        }
    })
}

/// The constant a literal of a bound value's type resolves to — `None` for a value with no literal
/// form (a composite), which then keeps the generic estimate.
pub(crate) fn value_const(v: &Value) -> Option<RExpr> {
    Some(match v {
        Value::Null => RExpr::ConstNull,
        Value::Int(n) => RExpr::ConstInt(*n),
        Value::Bool(b) => RExpr::ConstBool(*b),
        Value::Float32(f) => RExpr::ConstFloat32(*f),
        Value::Float64(f) => RExpr::ConstFloat64(*f),
        Value::Text(s) => RExpr::ConstText(s.clone()),
        Value::Decimal(d) => RExpr::ConstDecimal(d.clone()),
        Value::Bytea(b) => RExpr::ConstBytea(b.clone()),
        Value::Uuid(u) => RExpr::ConstUuid(*u),
        Value::Timestamp(m) => RExpr::ConstTimestamp(*m),
        Value::Timestamptz(m) => RExpr::ConstTimestamptz(*m),
        Value::Date(d) => RExpr::ConstDate(*d),
        Value::Interval(iv) => RExpr::ConstInterval(*iv),
        Value::Array(a) => RExpr::ConstArray(Box::new(a.clone())),
        Value::Range(r) => RExpr::ConstRange(Box::new(r.clone())),
        Value::Json(s) => RExpr::ConstJson(s.clone()),
        Value::Jsonb(n) => RExpr::ConstJsonb(Box::new(n.clone())),
        Value::JsonPath(s) => RExpr::ConstJsonPath(s.clone()),
        _ => return None,
    })
}

fn custom_frame(bound: &[Value], adopt: bool) -> Frame {
    Frame::Custom {
        values: std::rc::Rc::new(bound.to_vec()),
        adopt,
    }
}

/// One plan→bind boundary's frame. Dropping it pops the frame it pushed.
pub(crate) struct Planning {
    depth: usize,
}

/// Open a boundary: adopt a restart's pending custom frame, else push a probe frame.
pub(crate) fn begin() -> Planning {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        if let Some(Frame::Custom { adopt, .. }) = frames.last_mut() {
            if *adopt {
                *adopt = false;
                return Planning { depth: 0 };
            }
        }
        frames.push(Frame::Probe {
            sensitive: false,
            null_read: false,
            types: Vec::new(),
        });
        Planning {
            depth: frames.len(),
        }
    })
}

impl Planning {
    /// Whether this boundary's probe consulted a value: the plan read a parameter's value, or
    /// assumed a parameter non-NULL that `bound` binds to NULL. False for an adopted custom frame.
    pub(crate) fn needs_custom(&self, bound: &[Value]) -> bool {
        if self.depth == 0 {
            return false;
        }
        FRAMES.with(|frames| match &frames.borrow()[self.depth - 1] {
            Frame::Probe {
                sensitive,
                null_read,
                ..
            } => *sensitive || (*null_read && bound.iter().any(|v| matches!(v, Value::Null))),
            Frame::Custom { .. } => false,
        })
    }

    /// The probe's two observations, `(read a value, assumed a parameter non-NULL)` — what a
    /// prepared statement's cache entry keeps (`(false, false)` once custom).
    pub(crate) fn probe_facts(&self) -> (bool, bool) {
        if self.depth == 0 {
            return (false, false);
        }
        FRAMES.with(|frames| match &frames.borrow()[self.depth - 1] {
            Frame::Probe {
                sensitive,
                null_read,
                ..
            } => (*sensitive, *null_read),
            Frame::Custom { .. } => (false, false),
        })
    }

    /// Switch this boundary to a custom frame over `bound`, returning whether the probe plan must
    /// be planned again ([`Self::needs_custom`]). Planning after the call sees the values.
    pub(crate) fn bind(&mut self, bound: &[Value]) -> bool {
        let again = self.needs_custom(bound);
        if self.depth != 0 {
            FRAMES.with(|frames| frames.borrow_mut()[self.depth - 1] = custom_frame(bound, false));
        }
        again
    }

    /// Whether this boundary is still probing (it did not adopt a restart's custom frame).
    pub(crate) fn probing(&self) -> bool {
        self.depth != 0
            && FRAMES.with(|frames| matches!(frames.borrow()[self.depth - 1], Frame::Probe { .. }))
    }

    /// The parameter types the probe frame saw (`None` for a parameter never typed).
    pub(crate) fn probe_types(&self) -> Vec<Option<ScalarType>> {
        if self.depth == 0 {
            return Vec::new();
        }
        FRAMES.with(|frames| match &frames.borrow()[self.depth - 1] {
            Frame::Probe { types, .. } => types.clone(),
            Frame::Custom { .. } => Vec::new(),
        })
    }
}

impl Drop for Planning {
    fn drop(&mut self) {
        if self.depth != 0 {
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                debug_assert_eq!(frames.len(), self.depth, "plan frames are strictly nested");
                frames.truncate(self.depth - 1);
            });
        }
    }
}

/// Run `f` (a restarted statement) under a custom frame over `bound` that its own [`begin`] adopts.
pub(crate) fn restart<T>(bound: &[Value], f: impl FnOnce() -> T) -> T {
    FRAMES.with(|frames| frames.borrow_mut().push(custom_frame(bound, true)));
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            FRAMES.with(|frames| {
                frames.borrow_mut().pop();
            });
        }
    }
    let _pop = Pop;
    f()
}

/// Run `f` with the bound values visible to the estimator (planning that happens after binding,
/// with no probe of its own — an UPDATE/DELETE target scan).
pub(crate) fn with_values<T>(bound: &[Value], f: impl FnOnce() -> T) -> T {
    restart(bound, || {
        FRAMES.with(|frames| {
            if let Some(Frame::Custom { adopt, .. }) = frames.borrow_mut().last_mut() {
                *adopt = false;
            }
        });
        f()
    })
}

/// How many custom plans a parameter-sensitive prepared statement remembers (estimator.md §6.1:
/// any bounded policy; the memo is invisible).
pub(crate) const MEMO_CAPACITY: usize = 8;

/// The exact identity of a binding for the custom-plan memo (estimator.md §6.1): the `Debug`
/// rendering distinguishes every bit of every value (a float's sign and NaN, a decimal's scale).
pub(crate) fn memo_key(bound: &[Value]) -> String {
    format!("{bound:?}")
}
