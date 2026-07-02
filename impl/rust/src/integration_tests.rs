//! The former external integration tests (tests/*.rs), included as in-crate #[cfg(test)] modules
//! so they keep their `jed::…` paths (via `extern crate self as jed`) while reaching the now-
//! private internal modules. Cargo's auto-discovery is off (autotests = false); this is the seam.
#![cfg(test)]

#[path = "../tests/api.rs"]
mod api;
#[path = "../tests/array.rs"]
mod array;
#[path = "../tests/array_composite_functions.rs"]
mod array_composite_functions;
#[path = "../tests/array_concat_search.rs"]
mod array_concat_search;
#[path = "../tests/array_containment.rs"]
mod array_containment;
#[path = "../tests/array_functions.rs"]
mod array_functions;
#[path = "../tests/array_key.rs"]
mod array_key;
#[path = "../tests/array_quantified.rs"]
mod array_quantified;
#[path = "../tests/boolean_key.rs"]
mod boolean_key;
#[path = "../tests/cancellation.rs"]
mod cancellation;
#[path = "../tests/cast_array_runtime.rs"]
mod cast_array_runtime;
#[path = "../tests/cast_bool_int.rs"]
mod cast_bool_int;
#[path = "../tests/cast_text_runtime.rs"]
mod cast_text_runtime;
#[path = "../tests/cast_uuid.rs"]
mod cast_uuid;
#[path = "../tests/check_constraint.rs"]
mod check_constraint;
#[path = "../tests/checksum.rs"]
mod checksum;
#[path = "../tests/collation.rs"]
mod collation;
#[path = "../tests/collation_host.rs"]
mod collation_host;
#[path = "../tests/comments.rs"]
mod comments;
#[path = "../tests/composite.rs"]
mod composite;
#[path = "../tests/composite_pk.rs"]
mod composite_pk;
#[path = "../tests/compressed_cost.rs"]
mod compressed_cost;
#[path = "../tests/correlated_pushdown.rs"]
mod correlated_pushdown;
#[path = "../tests/cost_limit.rs"]
mod cost_limit;
#[path = "../tests/create_table.rs"]
mod create_table;
#[path = "../tests/cte.rs"]
mod cte;
#[path = "../tests/date.rs"]
mod date;
#[path = "../tests/datetime_conversions.rs"]
mod datetime_conversions;
#[path = "../tests/decimal.rs"]
mod decimal;
#[path = "../tests/delete.rs"]
mod delete;
#[path = "../tests/depth_limit.rs"]
mod depth_limit;
#[path = "../tests/drop_table.rs"]
mod drop_table;
#[path = "../tests/encoding.rs"]
mod encoding;
#[path = "../tests/ergonomic.rs"]
mod ergonomic;
#[path = "../tests/execute_script.rs"]
mod execute_script;
#[path = "../tests/explain.rs"]
mod explain;
#[path = "../tests/expr.rs"]
mod expr;
#[path = "../tests/file_sessions.rs"]
mod file_sessions;
#[path = "../tests/fileformat_golden.rs"]
mod fileformat_golden;
#[path = "../tests/float.rs"]
mod float;
#[path = "../tests/foreign_key.rs"]
mod foreign_key;
#[path = "../tests/generate_series.rs"]
mod generate_series;
#[path = "../tests/gist_index.rs"]
mod gist_index;
#[path = "../tests/identity.rs"]
mod identity;
#[path = "../tests/incremental.rs"]
mod incremental;
#[path = "../tests/insert.rs"]
mod insert;
#[path = "../tests/interval.rs"]
mod interval;
#[path = "../tests/join_pushdown.rs"]
mod join_pushdown;
#[path = "../tests/json.rs"]
mod json;
#[path = "../tests/jsonpath.rs"]
mod jsonpath;
#[path = "../tests/lazy_inline_values.rs"]
mod lazy_inline_values;
#[path = "../tests/lazy_large_values.rs"]
mod lazy_large_values;
#[path = "../tests/lifetime_cost.rs"]
mod lifetime_cost;
#[path = "../tests/lz4_vectors.rs"]
mod lz4_vectors;
#[path = "../tests/masked_scan.rs"]
mod masked_scan;
#[path = "../tests/on_conflict.rs"]
mod on_conflict;
#[path = "../tests/overflow_cost.rs"]
mod overflow_cost;
#[path = "../tests/params.rs"]
mod params;
#[path = "../tests/point_lookup.rs"]
mod point_lookup;
#[path = "../tests/privileges.rs"]
mod privileges;
#[path = "../tests/range_key.rs"]
mod range_key;
#[path = "../tests/range_storage.rs"]
mod range_storage;
#[path = "../tests/reclamation.rs"]
mod reclamation;
#[path = "../tests/regex_vectors.rs"]
mod regex_vectors;
#[path = "../tests/returning.rs"]
mod returning;
#[path = "../tests/secondary_index.rs"]
mod secondary_index;
#[path = "../tests/select.rs"]
mod select;
#[path = "../tests/select_no_from.rs"]
mod select_no_from;
#[path = "../tests/sequence.rs"]
mod sequence;
#[path = "../tests/session.rs"]
mod session;
#[path = "../tests/shared.rs"]
mod shared;
#[path = "../tests/spec_constants.rs"]
mod spec_constants;
#[path = "../tests/spill.rs"]
mod spill;
#[path = "../tests/split_shape.rs"]
mod split_shape;
#[path = "../tests/streaming.rs"]
mod streaming;
#[path = "../tests/subquery.rs"]
mod subquery;
#[path = "../tests/timestamp.rs"]
mod timestamp;
#[path = "../tests/timezone.rs"]
mod timezone;
#[path = "../tests/transactions.rs"]
mod transactions;
#[path = "../tests/unique.rs"]
mod unique;
#[path = "../tests/unnest.rs"]
mod unnest;
#[path = "../tests/update.rs"]
mod update;
#[path = "../tests/values_body.rs"]
mod values_body;
#[path = "../tests/variables.rs"]
mod variables;
#[path = "../tests/window_persisted.rs"]
mod window_persisted;
#[path = "../tests/writable_cte.rs"]
mod writable_cte;
