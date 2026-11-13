using System.Text;

namespace Jed;

internal readonly record struct FormatNode(string Text, string Id, bool Digit, string Mode, bool Fill = false, bool Translate = false, string Suffix = "")
{
    internal bool Literal => Id.Length == 0;
}

internal static class FormatTemplate
{
    internal static FormatNode[] Parse(string template, bool numeric = false)
    {
        var nodes = new List<FormatNode>(); var position = 0;
        var keywords = numeric ? FormattingCatalog.Num : FormattingCatalog.Dch;
        while (position < template.Length)
        {
            var fill = false; var translate = false;
            if (!numeric && position + 2 <= template.Length)
            {
                var prefix = template.AsSpan(position, 2);
                if (prefix.SequenceEqual("FM") || prefix.SequenceEqual("fm")) { fill = true; position += 2; }
                else if (prefix.SequenceEqual("TM") || prefix.SequenceEqual("tm")) { translate = true; position += 2; }
            }
            if (position >= template.Length) break;
            var found = false;
            foreach (var keyword in keywords)
            {
                if (!template.AsSpan(position).StartsWith(keyword.Name, StringComparison.Ordinal)) continue;
                position += keyword.Name.Length;
                var suffix = "";
                if (!numeric)
                    foreach (var modifier in FormattingCatalog.Suffixes)
                    {
                        if (modifier.Type != "postfix" || !template.AsSpan(position).StartsWith(modifier.Name, StringComparison.Ordinal)) continue;
                        suffix = modifier.Name; position += modifier.Name.Length; break;
                    }
                nodes.Add(new(keyword.Name, keyword.Id, keyword.Digit, keyword.Mode, fill, translate, suffix)); found = true; break;
            }
            if (found) continue;
            if (template[position] == '"')
            {
                position++;
                while (position < template.Length && template[position] != '"')
                {
                    if (template[position] == '\\' && position + 1 < template.Length) position++;
                    AddLiteral();
                }
                if (position < template.Length) position++;
            }
            else
            {
                if (template[position] == '\\' && position + 1 < template.Length && template[position + 1] == '"') position++;
                AddLiteral();
            }
            void AddLiteral()
            {
                var width = char.IsHighSurrogate(template[position]) && position + 1 < template.Length && char.IsLowSurrogate(template[position + 1]) ? 2 : 1;
                nodes.Add(new(template.Substring(position, width), "", false, "none")); position += width;
            }
        }
        return nodes.ToArray();
    }
    internal static string Ordinal(long value, bool upper)
    {
        var magnitude = value < 0 ? -(Int128)value : value;
        var suffix = magnitude % 100 >= 11 && magnitude % 100 <= 13 ? "th" : (int)(magnitude % 10) switch { 1 => "st", 2 => "nd", 3 => "rd", _ => "th" };
        return upper ? suffix.ToUpperInvariant() : suffix;
    }
}
