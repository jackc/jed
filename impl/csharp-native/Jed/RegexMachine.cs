using System.Text;

namespace Jed;

// Each machine is local to one evaluation; programs are immutable and can be shared.
internal sealed class RegexMachine
{
    private sealed record Slots(int[]? Leaf, Slots?[]? Branches)
    {
        internal static Slots Set(Slots? previous, int slot, int value, int depth, int count)
        {
            if (depth == 1)
            {
                var leaf = previous?.Leaf?.ToArray() ?? Enumerable.Repeat(-1, Math.Min(count, 16)).ToArray(); leaf[slot & 15] = value; return new(leaf, null);
            }
            var branches = previous?.Branches?.ToArray() ?? new Slots?[16]; var index = slot >> ((depth - 1) * 4) & 15;
            branches[index] = Set(branches[index], slot, value, depth - 1, 16); return new(null, branches);
        }
        internal static int Get(Slots? node, int slot, int depth)
        {
            while (node is not null && depth > 1) { node = node.Branches![slot >> (--depth * 4) & 15]; }
            return node?.Leaf?[slot & 15] ?? -1;
        }
    }
    private readonly record struct Thread(int Pc, Slots? Saves);
    private readonly RegexProgram program;
    private readonly uint[] marks;
    private uint generation;
    private readonly int slotCount, depth;
    internal RegexMachine(RegexProgram program)
    {
        this.program = program; marks = new uint[program.Code.Length]; slotCount = 2 * (program.Groups + 1); depth = 1;
        for (var capacity = 16; capacity < slotCount; capacity *= 16) depth++;
    }
    internal (int[]? Captures, long Steps) Search(string input, int start = 0, Meter? meter = null)
        => Search(program.FoldSubject(input).EnumerateRunes().Select(rune => rune.Value).ToArray(), start, meter);
    internal (int[]? Captures, long Steps) Search(int[] input, int start = 0, Meter? meter = null, HashSet<int>? gates = null, HashSet<int>? ends = null)
    {
        var current = new List<Thread>(); var next = new List<Thread>(); var pending = new Stack<Thread>(); Slots? matched = null; var found = false; long steps = 0;
        void AdvanceGeneration() { if (++generation == 0) { Array.Clear(marks); generation = 1; } }
        void Add(List<Thread> list, Thread seed, int position)
        {
            pending.Push(seed);
            while (pending.TryPop(out var thread))
            {
                if (marks[thread.Pc] == generation) continue; marks[thread.Pc] = generation; steps++; meter?.Charge(Costs.RegexStep);
                var instruction = program.Code[thread.Pc]; var following = thread with { Pc = thread.Pc + 1 };
                switch (instruction.Op)
                {
                    case "jmp": pending.Push(thread with { Pc = instruction.A }); break;
                    case "split": pending.Push(thread with { Pc = instruction.B }); pending.Push(thread with { Pc = instruction.A }); break;
                    case "save": pending.Push(following with { Saves = Slots.Set(thread.Saves, instruction.A, position, depth, slotCount) }); break;
                    case "gate": if (gates?.Contains(position) == true) pending.Push(following); break;
                    case "assertstart": if (position == 0) pending.Push(following); break;
                    case "assertend": if (position == input.Length) pending.Push(following); break;
                    case "assertlinestart": if (position == 0 || input[position - 1] == 10) pending.Push(following); break;
                    case "assertlineend": if (position == input.Length || input[position] == 10) pending.Push(following); break;
                    default: list.Add(thread); break;
                }
            }
        }
        if (start > input.Length) return (null, 0);
        AdvanceGeneration(); Add(current, new(0, null), start);
        for (var position = start; position <= input.Length; position++)
        {
            next.Clear(); AdvanceGeneration();
            foreach (var thread in current)
            {
                var instruction = program.Code[thread.Pc];
                if (instruction.Op == "match") { matched = thread.Saves; found = true; if (ends is not null) { ends.Add(position); continue; } break; }
                if (position == input.Length) continue;
                var consume = instruction.Op switch { "char" => input[position] == instruction.A, "any" => input[position] != 10, "anychar" => true, "class" => program.Classes[instruction.A].Contains(input[position]), _ => false };
                if (consume) Add(next, thread with { Pc = thread.Pc + 1 }, position + 1);
            }
            (current, next) = (next, current); meter?.Guard(); if (current.Count == 0) break;
        }
        if (!found) return (null, steps);
        var captures = new int[slotCount]; for (var i = 0; i < captures.Length; i++) captures[i] = Slots.Get(matched, i, depth); return (captures, steps);
    }
}
