/// Unsigned arbitrary precision arithmetic for the exact decimal value path.
/// Limbs are little-endian base 10^9; every intermediate product fits UInt64.
/// This is engine arithmetic, independent of platform bignum libraries.
internal struct BigMagnitude: Sendable, Equatable, CustomStringConvertible {
    static let base: UInt64 = 1_000_000_000
    private(set) var limbs: [UInt32]

    init(_ limbs: [UInt32]) {
        self.limbs = limbs
        while self.limbs.last == 0 { self.limbs.removeLast() }
        assert(self.limbs.allSatisfy { UInt64($0) < Self.base })
    }

    init(_ value: UInt64) {
        var remaining = value
        var limbs: [UInt32] = []
        while remaining != 0 {
            limbs.append(UInt32(remaining % Self.base))
            remaining /= Self.base
        }
        self.limbs = limbs
    }

    init(decimalDigits: ArraySlice<UInt8>) {
        var limbs: [UInt32] = []
        var end = decimalDigits.endIndex
        while end > decimalDigits.startIndex {
            let start = max(decimalDigits.startIndex, end - 9)
            var value: UInt32 = 0
            for digit in decimalDigits[start..<end] { value = value * 10 + UInt32(digit - 48) }
            limbs.append(value)
            end = start
        }
        self.init(limbs)
    }

    var isZero: Bool { limbs.isEmpty }
    var decimalDigits: Int {
        guard var top = limbs.last else { return 1 }
        var digits = 0
        repeat { digits += 1; top /= 10 } while top != 0
        return (limbs.count - 1) * 9 + digits
    }

    var description: String {
        guard let top = limbs.last else { return "0" }
        var result = String(top)
        for limb in limbs.dropLast().reversed() {
            let digits = String(limb)
            result += String(repeating: "0", count: 9 - digits.count) + digits
        }
        return result
    }

    func compare(to other: Self) -> Int {
        if limbs.count != other.limbs.count { return limbs.count < other.limbs.count ? -1 : 1 }
        for i in limbs.indices.reversed() where limbs[i] != other.limbs[i] {
            return limbs[i] < other.limbs[i] ? -1 : 1
        }
        return 0
    }

    func adding(_ other: Self) -> Self {
        let count = max(limbs.count, other.limbs.count)
        var output: [UInt32] = []
        output.reserveCapacity(count + 1)
        var carry: UInt64 = 0
        for i in 0..<count {
            let value = carry + (i < limbs.count ? UInt64(limbs[i]) : 0)
                + (i < other.limbs.count ? UInt64(other.limbs[i]) : 0)
            output.append(UInt32(value % Self.base))
            carry = value / Self.base
        }
        if carry != 0 { output.append(UInt32(carry)) }
        return Self(output)
    }

    /// Precondition is an internal arithmetic invariant, never an input check.
    func subtracting(_ other: Self) -> Self {
        precondition(compare(to: other) >= 0)
        var output = limbs
        var borrow: Int64 = 0
        for i in limbs.indices {
            var value = Int64(limbs[i]) - (i < other.limbs.count ? Int64(other.limbs[i]) : 0) - borrow
            if value < 0 { value += Int64(Self.base); borrow = 1 } else { borrow = 0 }
            output[i] = UInt32(value)
        }
        assert(borrow == 0)
        return Self(output)
    }

    func multiplying(_ other: Self) -> Self {
        if isZero || other.isZero { return Self(0) }
        var output = [UInt32](repeating: 0, count: limbs.count + other.limbs.count)
        for i in limbs.indices {
            var carry: UInt64 = 0
            for j in other.limbs.indices {
                let value = UInt64(limbs[i]) * UInt64(other.limbs[j]) + UInt64(output[i + j]) + carry
                output[i + j] = UInt32(value % Self.base)
                carry = value / Self.base
            }
            output[i + other.limbs.count] = UInt32(carry)
        }
        return Self(output)
    }

    func multiplying(_ factor: UInt32) -> Self {
        precondition(UInt64(factor) < Self.base)
        if factor == 0 || isZero { return Self(0) }
        var output: [UInt32] = []
        output.reserveCapacity(limbs.count + 1)
        var carry: UInt64 = 0
        for limb in limbs {
            let value = UInt64(limb) * UInt64(factor) + carry
            output.append(UInt32(value % Self.base))
            carry = value / Self.base
        }
        if carry != 0 { output.append(UInt32(carry)) }
        return Self(output)
    }

    func shiftedDecimal(_ digits: Int) -> Self {
        precondition(digits >= 0)
        if isZero || digits == 0 { return self }
        let groups = digits / 9
        var factor: UInt32 = 1
        for _ in 0..<(digits % 9) { factor *= 10 }
        let shifted = multiplying(factor)
        return Self([UInt32](repeating: 0, count: groups) + shifted.limbs)
    }

    func divided(by divisor: UInt32) -> (quotient: Self, remainder: UInt32) {
        precondition(divisor > 0)
        var output = limbs
        var remainder: UInt64 = 0
        for i in limbs.indices.reversed() {
            let value = remainder * Self.base + UInt64(limbs[i])
            output[i] = UInt32(value / UInt64(divisor))
            remainder = value % UInt64(divisor)
        }
        return (Self(output), UInt32(remainder))
    }

    /// Normalized long division. Quotient estimates use the top two limbs and
    /// are corrected against the next divisor limb before the full subtraction.
    func divided(by divisor: Self) -> (quotient: Self, remainder: Self) {
        precondition(!divisor.isZero)
        if compare(to: divisor) < 0 { return (Self(0), self) }
        if divisor.limbs.count == 1 {
            let result = divided(by: divisor.limbs[0])
            return (result.quotient, Self(UInt64(result.remainder)))
        }
        let n = divisor.limbs.count
        let m = limbs.count - n
        let normalization = UInt32(Self.base / (UInt64(divisor.limbs[n - 1]) + 1))
        let v = divisor.multiplying(normalization).limbs.map(UInt64.init)
        var u = multiplying(normalization).limbs.map(UInt64.init)
        while u.count < limbs.count + 1 { u.append(0) }
        var quotient = [UInt32](repeating: 0, count: m + 1)
        for j in stride(from: m, through: 0, by: -1) {
            let high = u[j + n] * Self.base + u[j + n - 1]
            var estimate = high / v[n - 1]
            var remainder = high % v[n - 1]
            while estimate >= Self.base || estimate * v[n - 2] > remainder * Self.base + u[j + n - 2] {
                estimate -= 1
                remainder += v[n - 1]
                if remainder >= Self.base { break }
            }
            var carry: UInt64 = 0
            var borrow: UInt64 = 0
            for i in 0..<n {
                let product = estimate * v[i] + carry
                carry = product / Self.base
                let subtrahend = product % Self.base + borrow
                if u[j + i] < subtrahend {
                    u[j + i] = u[j + i] + Self.base - subtrahend
                    borrow = 1
                } else {
                    u[j + i] -= subtrahend
                    borrow = 0
                }
            }
            let topSubtract = carry + borrow
            let negative = u[j + n] < topSubtract
            u[j + n] = negative ? u[j + n] + Self.base - topSubtract : u[j + n] - topSubtract
            if negative {
                estimate -= 1
                var addCarry: UInt64 = 0
                for i in 0..<n {
                    let value = u[j + i] + v[i] + addCarry
                    u[j + i] = value % Self.base
                    addCarry = value / Self.base
                }
                u[j + n] = (u[j + n] + addCarry) % Self.base
            }
            quotient[j] = UInt32(estimate)
        }
        let remainder = Self(u[0..<n].map(UInt32.init)).divided(by: normalization).quotient
        return (Self(quotient), remainder)
    }

    func splitDecimal(_ digits: Int) -> (quotient: Self, remainder: Self) {
        precondition(digits >= 0)
        if digits == 0 { return (self, Self(0)) }
        if digits >= decimalDigits { return (Self(0), self) }
        let groups = digits / 9
        var factor: UInt32 = 1
        for _ in 0..<(digits % 9) { factor *= 10 }
        let high = Self(Array(limbs.dropFirst(groups))).divided(by: factor)
        var remainder = Array(limbs.prefix(groups))
        remainder.append(high.remainder)
        return (high.quotient, Self(remainder))
    }

    /// Floor square root using integer Newton steps from a strict upper bound.
    /// The owning decimal kernel admits the entire work before reaching here.
    func squareRoot() -> Self {
        if isZero { return self }
        var upper = Self(1).shiftedDecimal((decimalDigits + 1) / 2)
        while true {
            let next = upper.adding(divided(by: upper).quotient).divided(by: 2).quotient
            if next.compare(to: upper) >= 0 { return upper }
            upper = next
        }
    }
}
