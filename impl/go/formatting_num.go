package jed

// The number half of the data-type formatting functions — to_char over integer, decimal, and
// float values, and to_number (spec/design/formatting.md §6–§7): PostgreSQL 18's NUMDesc_prepare,
// NUM_processor, and the per-type to_char preparers, transcribed byte for byte. The C printf
// conversions PostgreSQL's float paths use ("%.*f", "%+.*e") are reproduced exactly from the
// float's exact decimal expansion (round half to even, glibc's rule), with PostgreSQL's own
// snprintf spellings of NaN and Infinity.

import (
	"math"
	"strconv"
	"strings"
)

// numDesc is formatting.c's NUMDesc: what a number template asks for.
type numDesc struct {
	pre, post, lsign, flag, preLsignNum, multi, zeroStart, zeroEnd int
}

// The number template flags (formatting.c NUM_F_*).
const (
	numFDecimal   = 1 << 1
	numFLDecimal  = 1 << 2
	numFZero      = 1 << 3
	numFBlank     = 1 << 4
	numFFillmode  = 1 << 5
	numFLsign     = 1 << 6
	numFBracket   = 1 << 7
	numFMinus     = 1 << 8
	numFPlus      = 1 << 9
	numFRoman     = 1 << 10
	numFMulti     = 1 << 11
	numFPlusPost  = 1 << 12
	numFMinusPost = 1 << 13
	numFEEEE      = 1 << 14
)

const (
	lsignPre  = -1
	lsignPost = 1
	lsignNone = 0
)

func (n *numDesc) has(f int) bool { return n.flag&f != 0 }

func numSyntax(msg string) error { return newError(SyntaxError, msg) }

// prepare is formatting.c's NUMDesc_prepare for one pattern.
func (n *numDesc) prepare(id int) error {
	if n.has(numFEEEE) && id != numE {
		return numSyntax("\"EEEE\" must be the last pattern used")
	}
	switch id {
	case numNine:
		if n.has(numFBracket) {
			return numSyntax("\"9\" must be ahead of \"PR\"")
		}
		if n.has(numFMulti) {
			n.multi++
			break
		}
		if n.has(numFDecimal) {
			n.post++
		} else {
			n.pre++
		}
	case numZero:
		if n.has(numFBracket) {
			return numSyntax("\"0\" must be ahead of \"PR\"")
		}
		if !n.has(numFZero) && !n.has(numFDecimal) {
			n.flag |= numFZero
			n.zeroStart = n.pre + 1
		}
		if !n.has(numFDecimal) {
			n.pre++
		} else {
			n.post++
		}
		n.zeroEnd = n.pre + n.post
	case numB:
		if n.pre == 0 && n.post == 0 && !n.has(numFZero) {
			n.flag |= numFBlank
		}
	case numD, numDEC:
		if id == numD {
			n.flag |= numFLDecimal
		}
		if n.has(numFDecimal) {
			return numSyntax("multiple decimal points")
		}
		if n.has(numFMulti) {
			return numSyntax("cannot use \"V\" and decimal point together")
		}
		n.flag |= numFDecimal
	case numFM:
		n.flag |= numFFillmode
	case numS:
		if n.has(numFLsign) {
			return numSyntax("cannot use \"S\" twice")
		}
		if n.has(numFPlus) || n.has(numFMinus) || n.has(numFBracket) {
			return numSyntax("cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together")
		}
		if !n.has(numFDecimal) {
			n.lsign = lsignPre
			n.preLsignNum = n.pre
			n.flag |= numFLsign
		} else if n.lsign == lsignNone {
			n.lsign = lsignPost
			n.flag |= numFLsign
		}
	case numMI:
		if n.has(numFLsign) {
			return numSyntax("cannot use \"S\" and \"MI\" together")
		}
		n.flag |= numFMinus
		if n.has(numFDecimal) {
			n.flag |= numFMinusPost
		}
	case numPL:
		if n.has(numFLsign) {
			return numSyntax("cannot use \"S\" and \"PL\" together")
		}
		n.flag |= numFPlus
		if n.has(numFDecimal) {
			n.flag |= numFPlusPost
		}
	case numSG:
		if n.has(numFLsign) {
			return numSyntax("cannot use \"S\" and \"SG\" together")
		}
		n.flag |= numFMinus | numFPlus
	case numPR:
		if n.has(numFLsign) || n.has(numFPlus) || n.has(numFMinus) {
			return numSyntax("cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together")
		}
		n.flag |= numFBracket
	case numRN, numrn:
		if n.has(numFRoman) {
			return numSyntax("cannot use \"RN\" twice")
		}
		n.flag |= numFRoman
	case numV:
		if n.has(numFDecimal) {
			return numSyntax("cannot use \"V\" and decimal point together")
		}
		n.flag |= numFMulti
	case numE:
		if n.has(numFEEEE) {
			return numSyntax("cannot use \"EEEE\" twice")
		}
		if n.has(numFBlank) || n.has(numFFillmode) || n.has(numFLsign) || n.has(numFBracket) ||
			n.has(numFMinus) || n.has(numFPlus) || n.has(numFRoman) || n.has(numFMulti) {
			return numSyntax("\"EEEE\" is incompatible with other formats")
		}
		n.flag |= numFEEEE
	}
	if n.has(numFRoman) && n.flag&^(numFRoman|numFFillmode) != 0 {
		return numSyntax("\"RN\" is incompatible with other formats")
	}
	return nil
}

// numProc is formatting.c's NUMProc. For to_char, out is the output and number the digit string
// being laid out; for to_number, inout is the input and number the digits read (number[0] is the
// sign slot).
type numProc struct {
	isToChar     bool
	num          *numDesc
	sign         byte
	signWrote    bool
	numCount     int
	numIn        bool
	numCurr      int
	outPreSpaces int
	readDec      bool
	readPost     int
	readPre      int
	number       []byte
	numberP      int
	inout        []byte
	inoutP       int
	out          []byte
	lastRelevant int // -1: none
}

// The C-locale symbols (PostgreSQL's NUM_prepare_locale under lc_numeric = C).
const (
	numLocaleDecimal  = "."
	numLocaleThousand = ","
	numLocaleCurrency = " "
	numLocaleNegative = "-"
	numLocalePositive = "+"
)

// at is the number byte at i, 0 past the end (the C string's terminator).
func (np *numProc) at(i int) byte {
	if i < len(np.number) {
		return np.number[i]
	}
	return 0
}

// in is the input byte at i, 0 past the end.
func (np *numProc) in(i int) byte {
	if i < len(np.inout) {
		return np.inout[i]
	}
	return 0
}

func (np *numProc) overload() bool { return np.inoutP >= len(np.inout) }

func (np *numProc) amount(s int) bool { return np.inoutP <= len(np.inout)-s }

func (np *numProc) inputHasPrefix(p string) bool {
	return np.amount(len(p)) && strings.HasPrefix(string(np.inout[np.inoutP:]), p)
}

// isPredecSpace is formatting.c's IS_PREDEC_SPACE: a lone leading zero before the decimal point.
func (np *numProc) isPredecSpace() bool {
	return !np.num.has(numFZero) && np.numberP == 0 && np.at(0) == '0' && np.num.post != 0
}

func (np *numProc) lastRelevantIsPoint() bool {
	return np.lastRelevant >= 0 && np.at(np.lastRelevant) == '.'
}

// numpartToChar is formatting.c's NUM_numpart_to_char: write a sign, digit, or decimal point.
func (np *numProc) numpartToChar(id int) {
	if np.num.has(numFRoman) {
		return
	}
	np.numIn = false
	fill := np.num.has(numFFillmode)
	if !np.signWrote &&
		(np.numCurr >= np.outPreSpaces || (np.num.has(numFZero) && np.num.zeroStart == np.numCurr)) &&
		(!np.isPredecSpace() || np.lastRelevantIsPoint()) {
		switch {
		case np.num.has(numFLsign):
			if np.num.lsign == lsignPre {
				np.out = append(np.out, signSymbol(np.sign)...)
				np.signWrote = true
			}
		case np.num.has(numFBracket):
			if np.sign == '+' {
				np.out = append(np.out, ' ')
			} else {
				np.out = append(np.out, '<')
			}
			np.signWrote = true
		case np.sign == '+':
			if !fill {
				np.out = append(np.out, ' ')
			}
			np.signWrote = true
		case np.sign == '-':
			np.out = append(np.out, '-')
			np.signWrote = true
		}
	}
	if id == numNine || id == numZero || id == numD || id == numDEC {
		switch {
		case np.numCurr < np.outPreSpaces && (np.num.zeroStart > np.numCurr || !np.num.has(numFZero)):
			if !fill {
				np.out = append(np.out, ' ')
			}
		case np.num.has(numFZero) && np.numCurr < np.outPreSpaces && np.num.zeroStart <= np.numCurr:
			np.out = append(np.out, '0')
			np.numIn = true
		default:
			if np.at(np.numberP) == '.' {
				if !np.lastRelevantIsPoint() || fill {
					np.out = append(np.out, numLocaleDecimal...)
				}
			} else {
				switch {
				case np.lastRelevant >= 0 && np.numberP > np.lastRelevant && id != numZero:
				case np.isPredecSpace():
					if !fill {
						np.out = append(np.out, ' ')
					} else if np.lastRelevantIsPoint() {
						np.out = append(np.out, '0')
					}
				default:
					np.out = append(np.out, np.at(np.numberP))
					np.numIn = true
				}
			}
			if np.at(np.numberP) != 0 {
				np.numberP++
			}
		}
		end := np.numCount
		if np.outPreSpaces != 0 {
			end++
		}
		if np.num.has(numFDecimal) {
			end++
		}
		if np.lastRelevant >= 0 && np.lastRelevant == np.numberP {
			end = np.numCurr
		}
		if np.numCurr+1 == end {
			if np.signWrote && np.num.has(numFBracket) {
				if np.sign == '+' {
					np.out = append(np.out, ' ')
				} else {
					np.out = append(np.out, '>')
				}
			} else if np.num.has(numFLsign) && np.num.lsign == lsignPost {
				np.out = append(np.out, signSymbol(np.sign)...)
			}
		}
	}
	np.numCurr++
}

func signSymbol(sign byte) string {
	if sign == '-' {
		return numLocaleNegative
	}
	return numLocalePositive
}

// numpartFromChar is formatting.c's NUM_numpart_from_char: read a sign, digit, or decimal point.
func (np *numProc) numpartFromChar(id int) {
	if np.overload() {
		return
	}
	if np.in(np.inoutP) == ' ' {
		np.inoutP++
	}
	if np.overload() {
		return
	}
	// A sign before the number.
	if np.number[0] == ' ' && (id == numZero || id == numNine) && np.readPre+np.readPost == 0 {
		if np.num.has(numFLsign) && np.num.lsign == lsignPre {
			if np.inputHasPrefix(numLocaleNegative) {
				np.inoutP += len(numLocaleNegative)
				np.number[0] = '-'
			} else if np.inputHasPrefix(numLocalePositive) {
				np.inoutP += len(numLocalePositive)
				np.number[0] = '+'
			}
		} else {
			c := np.in(np.inoutP)
			if c == '-' || (np.num.has(numFBracket) && c == '<') {
				np.number[0] = '-'
				np.inoutP++
			} else if c == '+' {
				np.number[0] = '+'
				np.inoutP++
			}
		}
	}
	if np.overload() {
		return
	}
	isread := false
	if isDigitC(np.in(np.inoutP)) {
		if np.readDec && np.readPost == np.num.post {
			return
		}
		np.number = append(np.number, np.in(np.inoutP))
		np.numberP++
		if np.readDec {
			np.readPost++
		} else {
			np.readPre++
		}
		isread = true
	} else if np.num.has(numFDecimal) && !np.readDec {
		if np.inputHasPrefix(numLocaleDecimal) {
			np.inoutP += len(numLocaleDecimal) - 1
			np.number = append(np.number, '.')
			np.numberP++
			np.readDec = true
			isread = true
		}
	}
	if np.overload() {
		return
	}
	// A sign after the last digit.
	if np.number[0] == ' ' && np.readPre+np.readPost > 0 {
		if np.num.has(numFLsign) && isread && np.inoutP+1 < len(np.inout) && !isDigitC(np.in(np.inoutP+1)) {
			tmp := np.inoutP
			np.inoutP++
			if np.inputHasPrefix(numLocaleNegative) {
				np.inoutP += len(numLocaleNegative) - 1
				np.number[0] = '-'
			} else if np.inputHasPrefix(numLocalePositive) {
				np.inoutP += len(numLocalePositive) - 1
				np.number[0] = '+'
			}
			if np.number[0] == ' ' {
				np.inoutP = tmp
			}
		} else if !isread && !np.num.has(numFLsign) && (np.num.has(numFPlus) || np.num.has(numFMinus)) {
			c := np.in(np.inoutP)
			if c == '-' || c == '+' {
				np.number[0] = c
			}
		}
	}
}

// eatNonDataChars is formatting.c's NUM_eat_non_data_chars: skip up to n input characters that are
// not numeric data.
func (np *numProc) eatNonDataChars(n int) {
	for ; n > 0; n-- {
		if np.overload() {
			break
		}
		if strings.IndexByte("0123456789.,+-", np.in(np.inoutP)) >= 0 {
			break
		}
		np.inoutP += pgMblenBytes(np.inout, np.inoutP)
	}
}

func pgMblenBytes(b []byte, i int) int { return pgMblen(string(b[i:]), 0) }

// romanVal is formatting.c's ROMAN_VAL.
func romanVal(c byte) int {
	switch c {
	case 'I':
		return 1
	case 'V':
		return 5
	case 'X':
		return 10
	case 'L':
		return 50
	case 'C':
		return 100
	case 'D':
		return 500
	case 'M':
		return 1000
	}
	return 0
}

func validSubComb(cur, next byte) bool {
	return (cur == 'I' && (next == 'V' || next == 'X')) ||
		(cur == 'X' && (next == 'L' || next == 'C')) ||
		(cur == 'C' && (next == 'D' || next == 'M'))
}

// romanToInt is formatting.c's roman_to_int: a standard-form Roman numeral (1..3999), or -1.
func (np *numProc) romanToInt() int {
	for !np.overload() && isSpaceC(np.in(np.inoutP)) {
		np.inoutP++
	}
	var chars []byte
	var vals []int
	for len(chars) < 15 && !np.overload() {
		c := np.in(np.inoutP)
		if c >= 'a' && c <= 'z' {
			c -= 32
		}
		v := romanVal(c)
		if v == 0 {
			break
		}
		chars = append(chars, c)
		vals = append(vals, v)
		np.inoutP++
	}
	if len(chars) == 0 {
		return -1
	}
	result, repeat := 0, 1
	vCount, lCount, dCount := 0, 0, 0
	subtraction := false
	lastSub := 0
	for i := 0; i < len(chars); i++ {
		cur, cv := chars[i], vals[i]
		if subtraction && cv >= lastSub {
			return -1
		}
		if (vCount != 0 && cv >= 5) || (lCount != 0 && cv >= 50) || (dCount != 0 && cv >= 500) {
			return -1
		}
		switch cur {
		case 'V':
			vCount++
		case 'L':
			lCount++
		case 'D':
			dCount++
		}
		if i < len(chars)-1 {
			next, nv := chars[i+1], vals[i+1]
			if cv < nv {
				if !validSubComb(cur, next) || repeat > 1 {
					return -1
				}
				if (vCount != 0 && nv >= 5) || (lCount != 0 && nv >= 50) || (dCount != 0 && nv >= 500) {
					return -1
				}
				switch next {
				case 'V':
					vCount++
				case 'L':
					lCount++
				case 'D':
					dCount++
				}
				i++
				repeat = 1
				subtraction = true
				lastSub = cv
				result += nv - cv
			} else {
				if cur == next {
					repeat++
					if repeat > 3 {
						return -1
					}
				} else {
					repeat = 1
				}
				result += cv
			}
		} else {
			result += cv
		}
	}
	return result
}

// intToRoman is formatting.c's int_to_roman: upper-case, unpadded; 15 '#' out of 1..3999.
func intToRoman(number int64) string {
	if number > 3999 || number < 1 {
		return strings.Repeat("#", 15)
	}
	var b strings.Builder
	s := strconv.FormatInt(number, 10)
	for i := 0; i < len(s); i++ {
		d := int(s[i]-'0') - 1
		if d < 0 {
			continue
		}
		switch len(s) - i {
		case 4:
			b.WriteString(strings.Repeat("M", d+1))
		case 3:
			b.WriteString(fmtWordRnHundreds[d])
		case 2:
			b.WriteString(fmtWordRnTens[d])
		case 1:
			b.WriteString(fmtWordRnUnits[d])
		}
	}
	return b.String()
}

// numProcessor is formatting.c's NUM_processor. For to_char it returns the formatted text; for
// to_number the digits read, with num.post set to the fractional digits read.
func numProcessor(nodes []fmtNode, num *numDesc, inout, number []byte, outPreSpaces int, sign byte, isToChar bool) ([]byte, error) {
	np := &numProc{isToChar: isToChar, num: num, number: number, inout: inout, lastRelevant: -1}
	if num.zeroStart != 0 {
		num.zeroStart--
	}
	if num.has(numFEEEE) {
		if !isToChar {
			return nil, newError(FeatureNotSupported, "\"EEEE\" not supported for input")
		}
		return append([]byte(nil), number...), nil
	}
	fill := num.has(numFFillmode)
	if isToChar {
		np.sign = sign
		if num.has(numFPlus) || num.has(numFMinus) {
			np.signWrote = !(num.has(numFPlus) && !num.has(numFMinus))
		} else {
			if np.sign != '-' && fill {
				num.flag &^= numFBracket
			}
			np.signWrote = np.sign == '+' && fill && !num.has(numFLsign)
			if num.lsign == lsignPre && num.pre == num.preLsignNum {
				num.lsign = lsignPost
			}
		}
	}
	np.numCount = num.post + num.pre - 1
	if isToChar {
		np.outPreSpaces = outPreSpaces
		if fill && num.has(numFDecimal) {
			np.lastRelevant = lastRelevantDecnum(np.number)
			// Keep the digits a '0' pattern asks for, but not past the end of the number.
			if np.lastRelevant >= 0 && num.zeroEnd > np.outPreSpaces {
				lastZero := len(np.number) - 1
				if z := num.zeroEnd - np.outPreSpaces; z < lastZero {
					lastZero = z
				}
				if np.lastRelevant < lastZero {
					np.lastRelevant = lastZero
				}
			}
		}
		if !np.signWrote && np.outPreSpaces == 0 {
			np.numCount++
		}
	} else {
		np.number = []byte{' '}
		np.numberP = 1
	}
	for i := range nodes {
		n := &nodes[i]
		if n.typ == fnEnd {
			break
		}
		if !isToChar && np.overload() {
			break
		}
		if n.typ != fnAction {
			if isToChar {
				np.out = append(np.out, n.chars...)
			} else {
				np.inoutP += pgMblenBytes(np.inout, np.inoutP)
			}
			continue
		}
		switch n.key.id {
		case numNine, numZero, numDEC, numD:
			if isToChar {
				np.numpartToChar(n.key.id)
				continue
			}
			np.numpartFromChar(n.key.id)
		case numCOMMA:
			if isToChar {
				switch {
				case np.numIn:
					np.out = append(np.out, ',')
				case fill:
				default:
					np.out = append(np.out, ' ')
				}
				continue
			}
			if !np.numIn && fill {
				continue
			}
			if np.in(np.inoutP) != ',' {
				continue
			}
		case numG:
			if isToChar {
				switch {
				case np.numIn:
					np.out = append(np.out, numLocaleThousand...)
				case fill:
				default:
					np.out = append(np.out, strings.Repeat(" ", len(numLocaleThousand))...)
				}
				continue
			}
			if !np.numIn && fill {
				continue
			}
			if !np.inputHasPrefix(numLocaleThousand) {
				continue
			}
			np.inoutP += len(numLocaleThousand) - 1
		case numL:
			if isToChar {
				np.out = append(np.out, numLocaleCurrency...)
				continue
			}
			np.eatNonDataChars(len(numLocaleCurrency))
			continue
		case numRN, numrn:
			if isToChar {
				s := cString(np.number[np.numberP:])
				if n.key.id == numrn {
					s = asciiLowerStr(s)
				}
				if !fill {
					s = cPadLeft(s, 15)
				}
				np.out = append(np.out, s...)
				continue
			}
			r := np.romanToInt()
			if r < 0 {
				return nil, newError(InvalidTextRepresentation, "invalid Roman numeral")
			}
			s := strconv.Itoa(r)
			np.number = append(np.number, s...)
			np.numberP += len(s)
			num.pre = len(s)
			num.post = 0
			continue
		case numth, numTH:
			if num.has(numFRoman) || np.at(0) == '#' || np.sign == '-' || num.has(numFDecimal) {
				continue
			}
			if isToChar {
				th, err := fmtOrdinal(cString(np.number), n.key.id == numTH)
				if err != nil {
					return nil, err
				}
				np.out = append(np.out, th...)
				continue
			}
			np.eatNonDataChars(2)
			continue
		case numMI, numPL:
			want := byte('-')
			if n.key.id == numPL {
				want = '+'
			}
			if isToChar {
				switch {
				case np.sign == want:
					np.out = append(np.out, want)
				case fill:
				default:
					np.out = append(np.out, ' ')
				}
				continue
			}
			if np.in(np.inoutP) != want {
				np.eatNonDataChars(1)
				continue
			}
			np.number[0] = want
		case numSG:
			if isToChar {
				np.out = append(np.out, np.sign)
				continue
			}
			c := np.in(np.inoutP)
			if c != '-' && c != '+' {
				np.eatNonDataChars(1)
				continue
			}
			np.number[0] = c
		default:
			continue
		}
		// to_number: the action consumed one input character.
		np.inoutP++
	}
	if isToChar {
		// PostgreSQL's output is a C string: anything after a NUL byte is dropped.
		if k := strings.IndexByte(string(np.out), 0); k >= 0 {
			np.out = np.out[:k]
		}
		return np.out, nil
	}
	if np.numberP > 0 && np.number[np.numberP-1] == '.' {
		np.number = np.number[:np.numberP-1]
	}
	num.post = np.readPost
	return np.number, nil
}

// cString is b up to its first NUL byte.
func cString(b []byte) string {
	if k := strings.IndexByte(string(b), 0); k >= 0 {
		return string(b[:k])
	}
	return string(b)
}

// lastRelevantDecnum is formatting.c's get_last_relevant_decnum: the last nonzero digit after the
// decimal point, the point itself if there is none, or -1 without a point.
func lastRelevantDecnum(num []byte) int {
	p := strings.IndexByte(string(num), '.')
	if p < 0 {
		return -1
	}
	result := p
	for i := p + 1; i < len(num) && num[i] != 0; i++ {
		if num[i] != '0' {
			result = i
		}
	}
	return result
}

// --- the C printf conversions of a double ----------------------------------------------------

// roundDigitsHalfEven rounds the digit string c to its first keep digits, half to even on the
// exact remainder; carry reports a carry out of the top digit ("999" → "000", carry).
func roundDigitsHalfEven(c string, keep int) (string, bool) {
	if keep >= len(c) {
		return c + strings.Repeat("0", keep-len(c)), false
	}
	head := []byte(c[:keep])
	rest := c[keep:]
	up := false
	switch {
	case rest[0] > '5':
		up = true
	case rest[0] == '5':
		if strings.TrimRight(rest[1:], "0") != "" {
			up = true
		} else {
			up = keep > 0 && (head[keep-1]-'0')%2 == 1
			if keep == 0 {
				up = false
			}
		}
	}
	if !up {
		return string(head), false
	}
	for i := keep - 1; i >= 0; i-- {
		if head[i] == '9' {
			head[i] = '0'
			continue
		}
		head[i]++
		return string(head), false
	}
	return string(head), true
}

// exactDigits is |v|'s exact decimal expansion as (coefficient digits without leading zeros, scale);
// zero is ("", 0).
func exactDigits(v float64) (string, int) {
	_, digits, scale := exactDecimalFromFloat64(math.Abs(v)).canonical()
	if digits == "0" {
		return "", 0
	}
	return digits, int(scale)
}

// cFormatF is PostgreSQL's snprintf "%.*f" (a forced sign with forceSign): NaN and Infinity in
// PostgreSQL's spelling, the precision capped at 350 with the rest zero-filled.
func cFormatF(v float64, prec int) string {
	if math.IsNaN(v) {
		return "NaN"
	}
	sign := ""
	if v < 0 || (v == 0 && math.Signbit(v)) {
		sign = "-"
	}
	if math.IsInf(v, 0) {
		return sign + "Infinity"
	}
	p := prec
	if p > 350 {
		p = 350
	}
	c, s := exactDigits(v)
	// Align to p fractional digits: the integer value round(|v| * 10^p).
	var scaled string
	carry := false
	switch {
	case c == "":
		scaled = ""
	case s <= p:
		scaled = c + strings.Repeat("0", p-s)
	default:
		keep := len(c) - (s - p)
		if keep < 0 {
			// Every kept digit is zero and the value is below half a unit: it rounds to zero.
			scaled = ""
		} else {
			scaled, carry = roundDigitsHalfEven(c, keep)
			if carry {
				scaled = "1" + scaled
			}
		}
	}
	scaled = strings.TrimLeft(scaled, "0")
	if len(scaled) < p+1 {
		scaled = strings.Repeat("0", p+1-len(scaled)) + scaled
	}
	out := scaled[:len(scaled)-p]
	if p > 0 {
		out += "." + scaled[len(scaled)-p:]
	}
	return sign + out + strings.Repeat("0", prec-p)
}

// cFormatE is PostgreSQL's snprintf "%+.*e" (forceSign) or "%.*e".
func cFormatE(v float64, prec int, forceSign bool) string {
	if math.IsNaN(v) {
		return "NaN"
	}
	sign := ""
	if v < 0 || (v == 0 && math.Signbit(v)) {
		sign = "-"
	} else if forceSign {
		sign = "+"
	}
	if math.IsInf(v, 0) {
		return sign + "Infinity"
	}
	p := prec
	if p > 350 {
		p = 350
	}
	c, s := exactDigits(v)
	exp := 0
	var digits string
	if c == "" {
		digits = strings.Repeat("0", p+1)
	} else {
		exp = len(c) - 1 - s
		var carry bool
		digits, carry = roundDigitsHalfEven(c, p+1)
		if carry {
			digits = "1" + digits[:p]
			exp++
		}
	}
	out := digits[:1]
	if p > 0 {
		out += "." + digits[1:]
	}
	out += strings.Repeat("0", prec-p)
	es := "+"
	if exp < 0 {
		es = "-"
		exp = -exp
	}
	return sign + out + "e" + es + cPadInt(int64(exp), 2)
}

// numericOutSci is numeric.c's numeric_out_sci: d as a significand rounded (half away from zero)
// to scale digits, then "e" and a signed exponent of at least two digits. A significand that
// rounds up to 10 stays 10 (PostgreSQL does not renormalize).
func numericOutSci(d Decimal, scale int) string {
	if scale < 0 {
		scale = 0
	}
	neg, digits, dscale := d.canonical()
	exp := 0
	var sig string
	if digits == "0" {
		sig = "0"
		if scale > 0 {
			sig += "." + strings.Repeat("0", scale)
		}
		neg = false
	} else {
		exp = len(digits) - 1 - int(dscale)
		keep := scale + 1
		var head string
		if keep >= len(digits) {
			head = digits + strings.Repeat("0", keep-len(digits))
		} else {
			hb := []byte(digits[:keep])
			if digits[keep] >= '5' {
				carried := true
				for i := keep - 1; i >= 0 && carried; i-- {
					if hb[i] == '9' {
						hb[i] = '0'
					} else {
						hb[i]++
						carried = false
					}
				}
				if carried {
					hb = append([]byte{'1'}, hb...)
				}
			}
			head = string(hb)
		}
		ip := len(head) - scale
		sig = head[:ip]
		if scale > 0 {
			sig += "." + head[ip:]
		}
	}
	es := "+"
	if exp < 0 {
		es = "-"
		exp = -exp
	}
	out := sig + "e" + es + cPadInt(int64(exp), 2)
	if neg {
		return "-" + out
	}
	return out
}

// --- to_char over numbers --------------------------------------------------------------------

// numToCharFinish is NUM_TOCHAR_finish: run the processor over the laid-out number.
func numToCharFinish(nodes []fmtNode, num *numDesc, numstr string, outPreSpaces int, sign byte) (Value, error) {
	out, err := numProcessor(nodes, num, nil, []byte(numstr), outPreSpaces, sign, true)
	if err != nil {
		return Value{}, err
	}
	return TextValue(string(out)), nil
}

// numLayout splits a rendered number into its sign and digits and sizes the leading padding; a
// number wider than the template's integer digits becomes '#' fill.
func numLayout(orgnum string, num *numDesc) (numstr string, sign byte, outPreSpaces int) {
	sign = '+'
	numstr = orgnum
	if strings.HasPrefix(orgnum, "-") {
		sign = '-'
		numstr = orgnum[1:]
	}
	preLen := strings.IndexByte(numstr, '.')
	if preLen < 0 {
		preLen = len(numstr)
	}
	if preLen < num.pre {
		outPreSpaces = num.pre - preLen
	} else if preLen > num.pre {
		numstr = hashFill(num.pre, num.post)
	}
	return numstr, sign, outPreSpaces
}

// hashFill is the overflow fill: pre + post + 1 '#', with '.' at position pre.
func hashFill(pre, post int) string {
	b := []byte(strings.Repeat("#", pre+post+1))
	b[pre] = '.'
	return string(b)
}

// toCharNumberPrepare is NUM_TOCHAR_prepare: an empty template gives the empty string.
func toCharNumberPrepare(tmpl string) ([]fmtNode, *numDesc, bool, error) {
	if tmpl == "" {
		return nil, nil, false, nil
	}
	num := &numDesc{}
	nodes, err := parseFormat(tmpl, false, num)
	if err != nil {
		return nil, nil, false, err
	}
	return nodes, num, true, nil
}

// toCharInt is to_char(i16 / i32 / i64, text): int4_to_char for the 32-bit (and smaller) types,
// int8_to_char for i64 (whose EEEE goes through numeric, not float8).
func toCharInt(v int64, is8 bool, tmpl string) (Value, error) {
	nodes, num, ok, err := toCharNumberPrepare(tmpl)
	if err != nil || !ok {
		return TextValue(""), err
	}
	switch {
	case num.has(numFRoman):
		if v > math.MaxInt32 || v < math.MinInt32 {
			v = math.MaxInt32
		}
		return numToCharFinish(nodes, num, intToRoman(v), 0, 0)
	case num.has(numFEEEE):
		var s string
		if is8 {
			s = numericOutSci(decimalFromInt64(v), num.post)
			if !strings.HasPrefix(s, "-") {
				s = " " + s
			}
		} else {
			s = cFormatE(float64(v), num.post, true)
			if strings.HasPrefix(s, "+") {
				s = " " + s[1:]
			}
		}
		return numToCharFinish(nodes, num, s, 0, 0)
	}
	if num.has(numFMulti) {
		multi := pow10Float(num.multi)
		var err error
		if v, err = intMulMulti(v, multi, is8); err != nil {
			return Value{}, err
		}
		num.pre += num.multi
	}
	orgnum := strconv.FormatInt(v, 10)
	sign := byte('+')
	if v < 0 {
		sign = '-'
		orgnum = orgnum[1:]
	}
	preLen := len(orgnum)
	numstr := orgnum
	if num.post != 0 {
		numstr = orgnum + "." + strings.Repeat("0", num.post)
	}
	outPreSpaces := 0
	if preLen < num.pre {
		outPreSpaces = num.pre - preLen
	} else if preLen > num.pre {
		numstr = hashFill(num.pre, num.post)
	}
	return numToCharFinish(nodes, num, numstr, outPreSpaces, sign)
}

// intMulMulti is the V multiplier on an integer: PostgreSQL's dtoi4 / dtoi8 of pow(10, n) and
// int4mul / int8mul, each 22003 out of range.
func intMulMulti(v int64, multi float64, is8 bool) (int64, error) {
	m := math.RoundToEven(multi)
	if !is8 {
		over := newError(NumericValueOutOfRange, "integer out of range")
		if math.IsNaN(m) || m < -2147483648.0 || m >= 2147483648.0 {
			return 0, over
		}
		r := v * int64(m)
		if r > math.MaxInt32 || r < math.MinInt32 {
			return 0, over
		}
		return r, nil
	}
	over := newError(NumericValueOutOfRange, "bigint out of range")
	if math.IsNaN(m) || m < -9223372036854775808.0 || m >= 9223372036854775808.0 {
		return 0, over
	}
	r, ok := mul64(v, int64(m))
	if !ok {
		return 0, over
	}
	return r, nil
}

// toCharDecimal is to_char(decimal, text) — numeric_to_char.
func toCharDecimal(d Decimal, tmpl string) (Value, error) {
	nodes, num, ok, err := toCharNumberPrepare(tmpl)
	if err != nil || !ok {
		return TextValue(""), err
	}
	switch {
	case num.has(numFRoman):
		iv, fits := d.ToInt64Round()
		if !fits || iv > math.MaxInt32 || iv < math.MinInt32 {
			iv = math.MaxInt32
		}
		return numToCharFinish(nodes, num, intToRoman(iv), 0, 0)
	case num.has(numFEEEE):
		s := numericOutSci(d, num.post)
		if !strings.HasPrefix(s, "-") {
			s = " " + s
		}
		return numToCharFinish(nodes, num, s, 0, 0)
	}
	val := d
	if num.has(numFMulti) {
		p := decimalFromDigitsScale(false, "1"+strings.Repeat("0", num.multi), 0)
		if val, err = d.Mul(p); err != nil {
			return Value{}, err
		}
		num.pre += num.multi
	}
	x, err := val.RoundPlaces(int64(num.post))
	if err != nil {
		return Value{}, err
	}
	numstr, sign, outPreSpaces := numLayout(x.Render(), num)
	return numToCharFinish(nodes, num, numstr, outPreSpaces, sign)
}

// toCharFloat is to_char(f32 / f64, text) — float4_to_char / float8_to_char. The digits beyond
// FLT_DIG (6) / DBL_DIG (15) significant ones are dropped, as PostgreSQL does.
func toCharFloat(f float64, isF32 bool, tmpl string) (Value, error) {
	nodes, num, ok, err := toCharNumberPrepare(tmpl)
	if err != nil || !ok {
		return TextValue(""), err
	}
	switch {
	case num.has(numFRoman):
		var iv int64 = math.MaxInt32
		r := math.RoundToEven(f)
		if !math.IsNaN(r) && r >= -2147483648.0 && r < 2147483648.0 {
			iv = int64(r)
		}
		return numToCharFinish(nodes, num, intToRoman(iv), 0, 0)
	case num.has(numFEEEE):
		var s string
		if math.IsNaN(f) || math.IsInf(f, 0) {
			b := []byte(strings.Repeat("#", num.pre+num.post+6))
			b[0] = ' '
			b[num.pre+1] = '.'
			s = string(b)
		} else {
			s = cFormatE(f, num.post, true)
			if strings.HasPrefix(s, "+") {
				s = " " + s[1:]
			}
		}
		return numToCharFinish(nodes, num, s, 0, 0)
	}
	val := f
	if num.has(numFMulti) {
		multi := pow10Float(num.multi)
		if isF32 {
			val = float64(float32(f) * float32(multi))
		} else {
			val = f * multi
		}
		num.pre += num.multi
	}
	dig := 15
	if isF32 {
		dig = 6
	}
	preLen := len(cFormatF(math.Abs(val), 0))
	if preLen >= dig {
		num.post = 0
	} else if preLen+num.post > dig {
		num.post = dig - preLen
	}
	numstr, sign, outPreSpaces := numLayout(cFormatF(val, num.post), num)
	return numToCharFinish(nodes, num, numstr, outPreSpaces, sign)
}

// --- to_number ---------------------------------------------------------------------------------

// toNumberText is to_number(text, text) → decimal (numeric_to_number): the digits the template
// reads, as numeric(precision, scale) of the template's digit counts, then scaled by any V.
func toNumberText(input, tmpl string, m *costMeter) (Value, error) {
	if tmpl == "" {
		return NullValue(), nil
	}
	num := &numDesc{}
	nodes, err := parseFormat(tmpl, false, num)
	if err != nil {
		return Value{}, err
	}
	numstr, err := numProcessor(nodes, num, []byte(input), nil, 0, 0, false)
	if err != nil {
		return Value{}, err
	}
	scale := num.post
	precision := num.pre + num.multi + scale
	d, err := parseNumericIn(string(numstr))
	if err != nil {
		return Value{}, err
	}
	if d, err = applyNumericTypmod(d, precision, scale); err != nil {
		return Value{}, err
	}
	if num.has(numFMulti) {
		p, err := decPowerMetered(decimalFromInt64(10), decimalFromInt64(int64(-num.multi)), m)
		if err != nil {
			return Value{}, err
		}
		if d, err = d.Mul(p); err != nil {
			return Value{}, err
		}
	}
	return DecimalValue(d), nil
}

// parseNumericIn reads the processor's digits as numeric_in does: [sign] digits [. digits],
// surrounded by optional whitespace.
func parseNumericIn(s string) (Decimal, error) {
	bad := newError(InvalidTextRepresentation, "invalid input syntax for type numeric: \""+lossyUTF8(s)+"\"")
	i := 0
	for i < len(s) && isSpaceC(s[i]) {
		i++
	}
	neg := false
	if i < len(s) && (s[i] == '+' || s[i] == '-') {
		neg = s[i] == '-'
		i++
	}
	var intPart, frac []byte
	for i < len(s) && isDigitC(s[i]) {
		intPart = append(intPart, s[i])
		i++
	}
	if i < len(s) && s[i] == '.' {
		i++
		for i < len(s) && isDigitC(s[i]) {
			frac = append(frac, s[i])
			i++
		}
	}
	for i < len(s) && isSpaceC(s[i]) {
		i++
	}
	if i != len(s) || len(intPart)+len(frac) == 0 {
		return Decimal{}, bad
	}
	return decimalFromDigitsScale(neg, string(intPart)+string(frac), uint32(len(frac))).CheckCap()
}

// applyNumericTypmod is numeric.c's apply_typmod: round to scale, then 22003 when the integer
// digits exceed precision − scale.
func applyNumericTypmod(d Decimal, precision, scale int) (Decimal, error) {
	r := d.RoundToScale(uint32(scale))
	if r.IsZero() {
		return r, nil
	}
	_, digits, rs := r.canonical()
	intDigits := len(digits) - int(rs)
	if intDigits > precision-scale {
		return Decimal{}, newError(NumericValueOutOfRange, "numeric field overflow")
	}
	return r, nil
}

// pow10Float is glibc's pow(10, n) for a non-negative integer n (formatting.md §6.3): the nearest
// double to 10^n, which the decimal spelling "1e<n>" gives, except at the two exact halfway cases
// below 10^309 (n = 23 and n = 210), where glibc takes the upper neighbor instead of the even one
// (checked against PostgreSQL's 10::float8 ^ n for every n from 0 to 308). Go's math.Pow is not
// correctly rounded and is not used.
func pow10Float(n int) float64 {
	f, _ := strconv.ParseFloat("1e"+strconv.Itoa(n), 64)
	if n == 23 || n == 210 {
		f = math.Nextafter(f, math.Inf(1))
	}
	return f
}
