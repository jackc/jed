# spec/functions/ — function / operator catalog, as data

The function and operator catalog (CLAUDE.md §5): each entry names a function/operator,
its argument types, return type, and NULL behavior — **as data**, authored once. This is
the prime candidate for the **codegen middle path**: generate per-language stubs from the
shared definition rather than hand-writing N times.

Operator *result types* (e.g. the type of `int32 + int32`) live here, not in
[../types/](../types/): `types/` defines the scalars and how they compare/promote;
`functions/` defines what operators do with them.

> Status: empty. Populated when the first operators are needed — at minimum the
> comparison operators required by `SELECT ... WHERE pk = $1` (CLAUDE.md §11 step 5).
