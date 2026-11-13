using System.Globalization;
using System.Text;

namespace Jed;

internal static class NumberFormat
{
    private sealed class Description
    {
        internal readonly FormatNode[] Nodes;
        internal int Before, After, Shift, FirstZero = -1, LastFractionZero = -1;
        internal bool Fill, Decimal, Roman, LowerRoman, Scientific, Sign, SignPost, Minus, Plus, Both, Brackets;
        internal Description(string template)
        {
            Nodes = FormatTemplate.Parse(template, numeric: true); var shift = false; var digit = 0;
            foreach (var node in Nodes)
            {
                if (node.Literal) continue;
                var id = node.Id;
                if (Scientific) throw Errors.Syntax("EEEE must be the last numeric pattern");
                if (Roman && id is not ("FM" or "9" or "0")) throw Errors.Syntax("RN cannot be combined with numeric patterns");
                switch (id)
                {
                    case "FM": Fill = true; break;
                    case "9": case "0":
                        if (Brackets) throw Errors.Syntax("PR must follow numeric digits");
                        if (shift) Shift++;
                        else if (Decimal) { if (id == "0") LastFractionZero = After; After++; }
                        else { if (id == "0" && FirstZero < 0) FirstZero = Before; Before++; }
                        digit++; break;
                    case "DEC": case "D": if (Decimal || shift) throw Errors.Syntax("multiple decimal points or decimal point with V"); Decimal = true; break;
                    case "V": if (Decimal) throw Errors.Syntax("V cannot be combined with a decimal point"); shift = true; break;
                    case "RN": case "rn":
                        if (Decimal || Sign || Minus || Plus || Both || Brackets || shift) throw Errors.Syntax("RN cannot be combined with numeric patterns"); Roman = true; LowerRoman = id == "rn"; break;
                    case "E":
                        if (Fill || Sign || Minus || Plus || Both || Brackets || shift || Nodes.Any(n => !n.Literal && n.Id is not ("9" or "0" or "DEC" or "D" or "E"))) throw Errors.Syntax("invalid EEEE format"); Scientific = true; break;
                    case "S": if (Sign || Minus || Plus || Both || Brackets) throw Errors.Syntax("incompatible numeric sign patterns"); Sign = true; SignPost = digit != 0; break;
                    case "MI": if (Sign) throw Errors.Syntax("incompatible numeric sign patterns"); Minus = true; break;
                    case "PL": if (Sign) throw Errors.Syntax("incompatible numeric sign patterns"); Plus = true; break;
                    case "SG": if (Sign) throw Errors.Syntax("incompatible numeric sign patterns"); Both = true; break;
                    case "PR": if (Sign || digit == 0) throw Errors.Syntax("invalid PR pattern"); Brackets = true; break;
                }
            }
        }
    }
    internal static SqlValue Render(SqlValue source, string template, Meter meter)
    {
        var format = new Description(template); if (template.Length == 0) return SqlValue.Text("");
        var floating = source.Type.IsFloat(); var finite = !floating || double.IsFinite(source.AsFloat());
        var negative = floating ? BitConverter.DoubleToInt64Bits(source.AsFloat()) < 0 : source.Type == SqlType.Decimal ? source.AsDecimal().Negative : source.AsInteger() < 0;
        if (floating && (double.IsNaN(source.AsFloat()) || source.AsFloat() == 0)) negative = false;
        var value = !finite ? ExactDecimal.Zero : floating ? FloatValue.ToDecimal(source.AsFloat()).Abs() : source.Type == SqlType.Decimal ? source.AsDecimal().Abs() : ExactDecimal.FromInt64(source.AsInteger()).Abs();
        if (format.Shift != 0)
        {
            if (floating)
            {
                var power = FloatValue.Parse("1e" + format.Shift.ToString(CultureInfo.InvariantCulture));
                if (format.Shift is 23 or 210) power = Math.BitIncrement(power);
                var product = source.Type == SqlType.F32 ? (double)((float)source.AsFloat() * (float)power) : source.AsFloat() * power;
                finite = double.IsFinite(product); value = finite ? FloatValue.ToDecimal(product).Abs() : ExactDecimal.Zero;
            }
            else
            {
                var factor = ExactDecimal.Parse("1e" + format.Shift.ToString(CultureInfo.InvariantCulture));
                if (source.Type == SqlType.Decimal) { meter.Charge(value.DecimalWork(factor, "*")); meter.Guard(); }
                value = value.Multiply(factor);
                if (source.Type.IsInteger()) _ = SqlValue.Integer((negative ? value.Negate() : value).ToInt64(), source.Type);
            }
        }
        if (format.Roman)
        {
            var rounded = floating ? HalfEven(value, 0) : value.Round();
            var roman = negative || !finite || rounded.CompareTo(ExactDecimal.FromInt64(1)) < 0 || rounded.CompareTo(ExactDecimal.FromInt64(3999)) > 0 ? "###############" : Roman((int)rounded.ToInt64());
            if (format.LowerRoman) roman = roman.ToLowerInvariant();
            if (!format.Fill) roman = roman.PadLeft(15);
            var output = new StringBuilder();
            foreach (var node in format.Nodes) if (node.Literal) output.Append(node.Text); else if (node.Id is "RN" or "rn") output.Append(roman);
            return SqlValue.Text(output.ToString());
        }
        if (format.Scientific)
        {
            if (!finite) return SqlValue.Text(" " + new string('#', format.Before) + (format.Decimal ? "." + new string('#', format.After + 4) : new string('#', 4)));
            if (value.IsZero) negative = false;
            var exponent = value.IsZero ? 0 : value.Digits - value.Scale - 1;
            var significand = value.IsZero ? value : ExactDecimal.Parse(value.ToString() + "e" + (-exponent).ToString(CultureInfo.InvariantCulture));
            var even = floating || source.Type is var t && (t == SqlType.I16 || t == SqlType.I32);
            significand = even ? HalfEven(significand, format.After) : significand.Round(format.After);
            return SqlValue.Text((negative ? "-" : " ") + significand.ToString() + "e" + (exponent < 0 ? "-" : "+") + Math.Abs(exponent).ToString("D2", CultureInfo.InvariantCulture));
        }
        var precision = format.After; var effective = precision;
        if (floating) effective = Math.Min(precision, Math.Max(0, (source.Type == SqlType.F32 ? 6 : 15) - (finite ? Math.Max(1, value.Digits - value.Scale) : double.IsNaN(source.AsFloat()) ? 3 : 8)));
        var roundedValue = floating ? HalfEven(value, Math.Min(350, effective)) : value.Round(precision);
        var rendered = roundedValue.ToString().Split('.');
        var integer = rendered[0]; var fraction = rendered.Length > 1 ? rendered[1] : "";
        if (floating && !finite) integer = double.IsNaN(source.AsFloat()) ? "NaN" : "Infinity";
        if (precision > 0 && integer == "0" && format.FirstZero < 0 && (!format.Fill || format.LastFractionZero >= 0 || !roundedValue.IsZero)) integer = "";
        if (!floating) fraction = fraction.PadRight(precision, '0');
        var digitCount = format.Before + format.Shift;
        var overflow = integer.Length > digitCount;
        if (overflow) { integer = new string('#', digitCount); fraction = new string('#', floating ? effective : precision); }
        var leading = digitCount - integer.Length;
        var lastFraction = format.Fill ? Math.Max(format.LastFractionZero + 1, fraction.TrimEnd('0').Length) : fraction.Length;
        var builder = new StringBuilder(); var beforeIndex = 0; var fractionIndex = 0; var after = false; var started = false; var stopped = false;
        var ordinarySign = !format.Sign && !format.Minus && !format.Both && !format.Brackets;
        void Start()
        {
            if (started) return; started = true;
            if (format.Brackets) { if (negative) builder.Append('<'); else if (!format.Fill) builder.Append(' '); }
            else if (format.Sign && !format.SignPost) builder.Append(negative ? '-' : '+');
            else if (ordinarySign) { if (negative) builder.Append('-'); else if (!format.Fill) builder.Append(' '); }
        }
        foreach (var node in format.Nodes)
        {
            if (stopped) break;
            if (node.Literal) { builder.Append(node.Text); continue; }
            switch (node.Id)
            {
                case "9": case "0":
                    if (!after)
                    {
                        if (beforeIndex >= leading) { Start(); builder.Append(integer[beforeIndex - leading]); }
                        else if (format.FirstZero >= 0 && beforeIndex >= format.FirstZero) { Start(); builder.Append('0'); }
                        else if (!format.Fill) builder.Append(' ');
                        beforeIndex++;
                    }
                    else
                    {
                        Start();
                        if (fractionIndex >= fraction.Length && floating) { stopped = true; break; }
                        if (fractionIndex < lastFraction) builder.Append(fraction[fractionIndex]);
                        else if (!format.Fill && floating) stopped = true;
                        fractionIndex++;
                    }
                    break;
                case "DEC": case "D":
                    if (format.Fill && precision == 0) break;
                    if (floating && (!finite && !overflow || effective == 0 && precision > 0 && !overflow)) { stopped = true; break; }
                    Start(); builder.Append('.'); after = true; break;
                case "COMMA": case "G": if (started) builder.Append(','); else if (!format.Fill) builder.Append(' '); break;
                case "S": if (format.SignPost) builder.Append(negative ? '-' : '+'); break;
                case "MI": if (negative) builder.Append('-'); else if (!format.Fill) builder.Append(' '); break;
                case "PL": if (!negative) builder.Append('+'); else if (!format.Fill) builder.Append(' '); break;
                case "SG": builder.Append(negative ? '-' : '+'); break;
                case "PR": if (negative) builder.Append('>'); else if (!format.Fill) builder.Append(' '); break;
                case "L": builder.Append(' '); break;
                case "TH": case "th": if (!negative && !format.Decimal && !overflow) builder.Append(FormatTemplate.Ordinal(roundedValue.Remainder(ExactDecimal.FromInt64(100)).ToInt64(), node.Id == "TH")); break;
            }
        }
        return SqlValue.Text(builder.ToString());
    }
    internal static ExactDecimal HalfEven(ExactDecimal value, int precision)
    {
        var below = value.Truncate(precision); if (below.CompareTo(value) == 0) return below;
        var step = ExactDecimal.Parse("1e" + (-precision).ToString(CultureInfo.InvariantCulture));
        var half = step.DivideAtScale(ExactDecimal.FromInt64(2), Math.Min(ExactDecimal.MaxScale, precision + 1));
        var comparison = value.Subtract(below).CompareTo(half);
        var digits = below.ToString(); var last = digits[^1];
        return comparison > 0 || comparison == 0 && (last - '0') % 2 != 0 ? below.Add(step) : below;
    }
    internal static string Roman(int number)
    {
        var output = new StringBuilder(); output.Append('M', number / 1000); number %= 1000;
        if (number / 100 != 0) output.Append(FormattingCatalog.RnHundreds[number / 100 - 1]); number %= 100;
        if (number / 10 != 0) output.Append(FormattingCatalog.RnTens[number / 10 - 1]); number %= 10;
        if (number != 0) output.Append(FormattingCatalog.RnUnits[number - 1]);
        return output.ToString();
    }
    internal static SqlValue Parse(string input, string template, Meter meter)
    {
        if (template.Length == 0) return SqlValue.Null(SqlType.Decimal);
        var format = new Description(template); if (format.Scientific) throw Errors.Unsupported("EEEE not supported for input");
        if (format.Roman)
        {
            var word = input.TrimStart().Split(' ', '\t', '\r', '\n')[0].ToUpperInvariant(); var total = 0;
            if (word.Length > 15) throw new SqlException("22P02", "invalid Roman numeral");
            for (var i = 0; i < word.Length; i++)
            {
                var digit = RomanDigit(word[i]); if (digit == 0) throw new SqlException("22P02", "invalid numeric input");
                total += i + 1 < word.Length && RomanDigit(word[i + 1]) > digit ? -digit : digit;
            }
            if (total is < 1 or > 3999 || Roman(total) != word) throw new SqlException("22P02", "invalid numeric input");
            return SqlValue.Decimal(ExactDecimal.FromInt64(total));
        }
        var output = new StringBuilder(); var position = 0; var negative = false; var decimalRead = false; var fractional = 0; var integer = 0;
        void Sign()
        {
            if (position < input.Length && input[position] is '+' or '-' or '<') { negative |= input[position] is '-' or '<'; position++; }
        }
        void Spaces() { while (position < input.Length && char.IsAscii(input[position]) && char.IsWhiteSpace(input[position])) position++; }
        foreach (var node in format.Nodes)
        {
            if (position >= input.Length) break;
            if (node.Literal) { if (node.Text.All(char.IsWhiteSpace)) Spaces(); else position++; continue; }
            switch (node.Id)
            {
                case "9": case "0": case "DEC": case "D":
                    Spaces(); Sign();
                    if (position < input.Length && input[position] == '.' && !decimalRead && format.Decimal) { output.Append('.'); decimalRead = true; position++; }
                    if (node.Id is "DEC" or "D") break;
                    Sign();
                    if (position < input.Length && char.IsAsciiDigit(input[position]))
                    {
                        if (decimalRead && fractional >= format.After) break;
                        output.Append(input[position++]); if (decimalRead) fractional++; else integer++;
                    }
                    break;
                case "G": case "COMMA": if (position < input.Length && input[position] == ',') position++; break;
                case "S": case "MI": case "PL": case "SG": case "PR": Spaces(); Sign(); break;
                case "L": Spaces(); if (position < input.Length && !char.IsAsciiDigit(input[position]) && input[position] is not ('.' or '+' or '-')) position++; break;
                case "TH": case "th": position = Math.Min(input.Length, position + 2); break;
            }
        }
        if (integer == 0 && fractional == 0) { if (format.Before == 0 && input.Any(char.IsAsciiDigit)) throw Errors.Overflow(); throw new SqlException("22P02", "invalid numeric input"); }
        if (integer > format.Before + format.Shift) throw Errors.Overflow();
        var parsed = ExactDecimal.Parse((negative ? "-" : "") + output);
        if (format.Shift != 0)
        {
            var factor = ExactDecimal.Parse("1e-" + format.Shift.ToString(CultureInfo.InvariantCulture));
            meter.Charge(parsed.DecimalWork(factor, "*")); meter.Guard();
            parsed = parsed.Multiply(factor).Round(16 + format.Shift);
        }
        return SqlValue.Decimal(parsed);
    }
    private static int RomanDigit(char letter) => letter switch { 'I' => 1, 'V' => 5, 'X' => 10, 'L' => 50, 'C' => 100, 'D' => 500, 'M' => 1000, _ => 0 };
}
