package jed

// PROTOTYPE (spec/design/leaf-write-cost.md §4, option a): leave slack in a leaf when it is written.
// JED_LEAF_SLACK=d makes a right-edge leaf split (an append to a tree's last leaf, which is how a
// table loaded in key order is filled) give its left leaf at most C - C/d bytes instead of the most
// that fits, so a later edit that grows the leaf, such as one value widening a base-plus-offsets
// column, finds room instead of splitting it. The fit rule is unchanged: a leaf still holds up to C.
// Changes the split point, which is part of the byte contract; off (0) by default.

import (
	"os"
	"strconv"
)

var leafSlack = func() int {
	d, err := strconv.Atoi(os.Getenv("JED_LEAF_SLACK"))
	if err != nil || d < 2 {
		return 0
	}
	return d
}()
