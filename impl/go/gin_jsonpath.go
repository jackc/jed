package jed

import (
	"bytes"
	"slices"
)

// GIN query trees and the jsonb_ops jsonpath term extraction (spec/design/gin.md §2.2). Mirrors
// impl/rust/src/gin_jsonpath.rs.
//
// A GIN query is a monotone AND/OR tree over terms (ginTree): a row is a candidate when the tree is
// TRUE with each term read as "the row carries this term". The ordinary strategies are one level deep
// (mode ALL is an AND of terms, mode ANY an OR); the jsonpath strategies @? / @@ extract a deeper tree
// from the path, PostgreSQL's extract_jsp_query (jsonb_gin.c) for jsonb_ops, term for term.

// ginTreeKind tags a ginTree node.
type ginTreeKind int

const (
	ginTreeTerm ginTreeKind = iota
	ginTreeAnd
	ginTreeOr
)

// ginTree is a normalized GIN query tree (gin.md §2.2). An And / Or node holds its distinct leaf
// terms in ascending byte order and its non-leaf children in extraction order; nested nodes of the
// same kind are spliced into their parent, and a node with one child is that child.
type ginTree struct {
	kind   ginTreeKind
	term   []byte     // ginTreeTerm
	leaves [][]byte   // ginTreeAnd / ginTreeOr
	rest   []*ginTree // ginTreeAnd / ginTreeOr
}

// ginTreeFromTerms is a strategy's flat term set as a tree: mode ANY an Or, mode ALL an And.
func ginTreeFromTerms(any bool, terms [][]byte) *ginTree {
	nodes := make([]*ginTree, len(terms))
	for i, t := range terms {
		nodes[i] = &ginTree{kind: ginTreeTerm, term: t}
	}
	return ginTreeNormalize(!any, nodes)
}

// mapTerms is the tree with every leaf term mapped through f (a multi-column index's column tag,
// gin.md §4.4); the mapping preserves byte order and distinctness.
func (t *ginTree) mapTerms(f func([]byte) []byte) *ginTree {
	out := &ginTree{kind: t.kind}
	if t.kind == ginTreeTerm {
		out.term = f(t.term)
		return out
	}
	for _, l := range t.leaves {
		out.leaves = append(out.leaves, f(l))
	}
	for _, c := range t.rest {
		out.rest = append(out.rest, c.mapTerms(f))
	}
	return out
}

func ginTreeNormalize(and bool, nodes []*ginTree) *ginTree {
	same := ginTreeOr
	if and {
		same = ginTreeAnd
	}
	var leaves [][]byte
	var rest []*ginTree
	for _, n := range nodes {
		switch n.kind {
		case ginTreeTerm:
			leaves = append(leaves, n.term)
		case same:
			leaves = append(leaves, n.leaves...)
			rest = append(rest, n.rest...)
		default:
			rest = append(rest, n)
		}
	}
	slices.SortFunc(leaves, bytes.Compare)
	leaves = slices.CompactFunc(leaves, bytes.Equal)
	switch {
	case len(leaves) == 1 && len(rest) == 0:
		return &ginTree{kind: ginTreeTerm, term: leaves[0]}
	case len(leaves) == 0 && len(rest) == 1:
		return rest[0]
	}
	return &ginTree{kind: same, leaves: leaves, rest: rest}
}

// jpGinItemKind tags one accessor of the path being walked (PostgreSQL's JsonPathGinPathItem): a .key
// carries its KEY term; the wildcards and subscripts carry none.
type jpGinItemKind int

const (
	jpGinKey jpGinItemKind = iota
	jpGinAnyKey
	jpGinAnyArray
	jpGinIndexArray
	jpGinAny
)

type jpGinItem struct {
	kind jpGinItemKind
	key  []byte
}

// jpGinNode is an extracted node before normalization (PostgreSQL's JsonPathGinNode).
type jpGinNode struct {
	kind     ginTreeKind
	entry    []byte
	children []*jpGinNode
}

func (n *jpGinNode) tree() *ginTree {
	if n.kind == ginTreeTerm {
		return &ginTree{kind: ginTreeTerm, term: n.entry}
	}
	nodes := make([]*ginTree, len(n.children))
	for i, c := range n.children {
		nodes[i] = c.tree()
	}
	return ginTreeNormalize(n.kind == ginTreeAnd, nodes)
}

// jsonPathGinTree is the jsonb_ops query tree of a jsonpath under @? (exists) or @@ (gin.md §2.2), or
// nil when nothing is extractable — the full-scan fallback (PostgreSQL's GIN_SEARCH_MODE_ALL).
func jsonPathGinTree(path *jsonPath, exists bool) *ginTree {
	lax := !path.Strict
	var node *jpGinNode
	switch {
	case exists && path.Pred == nil:
		node = jpGinPathExpr(lax, nil, &path.Path, nil)
	case !exists && path.Pred != nil:
		node = jpGinBool(lax, nil, path.Pred, false)
	}
	if node == nil {
		return nil
	}
	return node.tree()
}

// jpGinBool is PostgreSQL's extract_jsp_bool_expr. Under not, an AND with one unextractable side
// keeps the other only when the node is still an AND after De Morgan (PostgreSQL keeps it whenever
// the operator is written &&, which drops rows under !) — a deliberate, sound divergence (gin.md §9).
func jpGinBool(lax bool, path []jpGinItem, pred *jpPred, not bool) *jpGinNode {
	switch pred.kind {
	case jpPredAnd, jpPredOr:
		and := not != (pred.kind == jpPredAnd)
		larg := jpGinBool(lax, path, pred.left, not)
		rarg := jpGinBool(lax, path, pred.right, not)
		switch {
		case larg != nil && rarg != nil:
			kind := ginTreeOr
			if and {
				kind = ginTreeAnd
			}
			return &jpGinNode{kind: kind, children: []*jpGinNode{larg, rarg}}
		case and && larg != nil:
			return larg
		case and:
			return rarg
		}
		return nil
	case jpPredNot:
		return jpGinBool(lax, path, pred.left, !not)
	case jpPredExists:
		if not {
			return nil
		}
		return jpGinPathExpr(lax, path, &pred.cmpLeft, nil)
	case jpPredCompare:
		if not || pred.cmpOp != jpCmpEq {
			return nil
		}
		if s, ok := jpGinScalar(&pred.cmpLeft); ok {
			return jpGinPathExpr(lax, path, &pred.cmpRight, s)
		}
		if s, ok := jpGinScalar(&pred.cmpRight); ok {
			return jpGinPathExpr(lax, path, &pred.cmpLeft, s)
		}
	}
	return nil
}

// jpGinScalar is a comparison operand PostgreSQL's jspIsScalar accepts: a step-less null / boolean /
// number / string literal.
func jpGinScalar(e *jpPathExpr) (*JsonNode, bool) {
	if e.primary == jpLiteral && len(e.steps) == 0 {
		return &e.lit, true
	}
	return nil, false
}

// jpGinPathExpr is PostgreSQL's extract_jsp_path_expr: the nodes of expr AND-ed, or nil when none.
func jpGinPathExpr(lax bool, path []jpGinItem, expr *jpPathExpr, scalar *JsonNode) *jpGinNode {
	nodes := jpGinPathNodes(lax, path, expr, scalar)
	switch len(nodes) {
	case 0:
		return nil
	case 1:
		return nodes[0]
	}
	return &jpGinNode{kind: ginTreeAnd, children: nodes}
}

// jpGinPathNodes is PostgreSQL's extract_jsp_path_expr_nodes + jsonb_ops__extract_nodes: walk the
// accessors, extracting each filter; an accessor the opclass cannot follow ends the walk with only the
// filter nodes found so far. A complete walk with a scalar adds a KEY entry per .key accessor (last to
// first) and the scalar's entry.
func jpGinPathNodes(lax bool, outer []jpGinItem, expr *jpPathExpr, scalar *JsonNode) []*jpGinNode {
	var nodes []*jpGinNode
	var path []jpGinItem
	switch expr.primary {
	case jpRoot:
	case jpCurrent:
		path = append(path, outer...)
	default:
		return nodes
	}
	for i := range expr.steps {
		st := &expr.steps[i]
		var item jpGinItem
		switch st.kind {
		case jpFilter:
			if n := jpGinBool(lax, path, st.pred, false); n != nil {
				nodes = append(nodes, n)
			}
			continue
		case jpMember:
			item = jpGinItem{kind: jpGinKey, key: ginJsonbKeyTerm(st.key)}
		case jpWildcardMember:
			item = jpGinItem{kind: jpGinAnyKey}
		case jpWildcardElement:
			item = jpGinItem{kind: jpGinAnyArray}
		case jpSubscripts:
			item = jpGinItem{kind: jpGinIndexArray}
		case jpAnyStep:
			item = jpGinItem{kind: jpGinAny}
		default:
			return nodes
		}
		path = append(path, item)
	}
	if scalar == nil {
		return nodes
	}
	for i := len(path) - 1; i >= 0; i-- {
		if path[i].kind == jpGinKey {
			nodes = append(nodes, &jpGinNode{kind: ginTreeTerm, entry: path[i].key})
		}
	}
	if scalar.Kind != JString {
		return append(nodes, &jpGinNode{kind: ginTreeTerm, entry: ginJsonbScalarTerm(scalar, false)})
	}
	// jsonb_ops indexes a string array element as a KEY, so the scalar may be a key, a value, or
	// either (lax mode unwraps arrays; .** reaches both).
	maybe, asKey := lax, false
	if !lax && len(path) > 0 {
		switch path[len(path)-1].kind {
		case jpGinAnyArray, jpGinIndexArray:
			asKey = true
		case jpGinAny:
			maybe = true
		}
	}
	if maybe {
		return append(nodes, &jpGinNode{kind: ginTreeOr, children: []*jpGinNode{
			{kind: ginTreeTerm, entry: ginJsonbScalarTerm(scalar, true)},
			{kind: ginTreeTerm, entry: ginJsonbScalarTerm(scalar, false)},
		}})
	}
	return append(nodes, &jpGinNode{kind: ginTreeTerm, entry: ginJsonbScalarTerm(scalar, asKey)})
}
