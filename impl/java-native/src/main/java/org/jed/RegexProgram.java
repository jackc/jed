package org.jed;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;

/** Handwritten regex parser, deterministic NFA lowering, and bounded Pike VM. */
final class RegexProgram {
    static final int MAX_PROGRAM = 32768;
    record Instruction(String op, int x, int y) {
        @Override public String toString() { return op + (List.of("char", "class", "jmp", "save").contains(op) ? " " + x : op.equals("split") ? " " + x + " " + y : ""); }
    }
    record Range(int lo, int hi) {}
    record CharacterClass(List<Range> ranges, boolean negated) {
        boolean admits(int point) { boolean inside = false; for (Range r : ranges) { if (point < r.lo) break; if (point <= r.hi) { inside = true; break; } } return inside != negated; }
        @Override public String toString() { var out = new StringBuilder(negated ? "^" : ""); for (Range r : ranges) { if (out.length() > (negated ? 1 : 0)) out.append(','); out.append(r.lo).append('-').append(r.hi); } return out.toString(); }
    }
    private record Node(String kind, int value, List<Node> children, CharacterClass characterClass, long min, long max, boolean lazy) {
        Node(String kind, int value) { this(kind, value, List.of(), null, 0, 0, false); }
        Node(String kind, List<Node> children) { this(kind, 0, children, null, 0, 0, false); }
        boolean nullable() { return switch (kind) { case "char", "any", "class" -> false; case "concat" -> children.stream().allMatch(Node::nullable); case "alt" -> children.stream().anyMatch(Node::nullable); case "group", "plus" -> children.getFirst().nullable(); case "repeat" -> min == 0 || children.getFirst().nullable(); default -> true; }; }
    }
    private final List<Instruction> instructions;
    private final List<CharacterClass> classes;
    private final int groups;
    private final boolean insensitive;
    private final ThreadLocal<Runner> runners;
    private RegexProgram(List<Instruction> instructions, List<CharacterClass> classes, int groups, boolean insensitive) { this.instructions = List.copyOf(instructions); this.classes = List.copyOf(classes); this.groups = groups; this.insensitive = insensitive; runners = ThreadLocal.withInitial(() -> new Runner(this)); }
    static RegexProgram compile(String pattern, String flags, boolean jsonPath) { return compile(pattern,flags,jsonPath,false,false); }
    static RegexProgram gated(String pattern,boolean rightmost) { return compile(pattern,"s",false,true,rightmost); }
    private static RegexProgram compile(String pattern, String flags, boolean jsonPath, boolean gated, boolean rightmost) {
        boolean fold = flags.indexOf('i') >= 0, quote = flags.indexOf('q') >= 0, dotAll = flags.indexOf('s') >= 0, lines = flags.indexOf('m') >= 0;
        Parser parser = new Parser(pattern, fold, quote, dotAll, lines, jsonPath && !dotAll);
        Node root = parser.parse(); Compiler compiler = new Compiler(); compiler.emit(new Instruction("split", rightmost ? 1 : 3, rightmost ? 3 : 1)); compiler.emit(new Instruction("anychar", 0, 0)); compiler.emit(new Instruction("jmp", 0, 0)); if (gated) compiler.emit(new Instruction("gate",0,0)); compiler.emit(new Instruction("save", 0, 0));
        if (size(root) + (gated ? 7 : 6) > MAX_PROGRAM) throw tooLarge();
        compiler.node(root); compiler.emit(new Instruction("save", 1, 0)); compiler.emit(new Instruction("match", 0, 0));
        return new RegexProgram(compiler.code, compiler.classes, parser.groups, fold);
    }
    Match gatedSearch(int[] input, boolean[] gates, CostMeter meter) { return runners.get().search(input,0,meter,gates,null); }
    boolean[] ends(int[] input, boolean[] gates, CostMeter meter) { boolean[] ends = new boolean[input.length + 1]; runners.get().search(input,0,meter,gates,ends); return ends; }
    int groups() { return groups; }
    int size() { return instructions.size(); }
    List<String> listing() { return instructions.stream().map(Instruction::toString).toList(); }
    List<String> classListing() { return classes.stream().map(CharacterClass::toString).toList(); }
    void chargeCompile(CostMeter meter) { meter.charge(CostMeter.saturatingMultiply(size(), Costs.REGEX_COMPILE)); meter.guard(); }
    Match search(String subject, int start, CostMeter meter) { int[] input = subject.codePoints().toArray(); if (insensitive) for (int i = 0; i < input.length; i++) input[i] = fold(input[i]); return search(input, start, meter); }
    Match search(int[] input, int start, CostMeter meter) { if (start > input.length) return null; return runners.get().search(input, start, meter); }
    int[] input(String subject) { int[] input = subject.codePoints().toArray(); if (insensitive) for (int i = 0; i < input.length; i++) input[i] = fold(input[i]); return input; }
    static int fold(int c) { return c >= 'A' && c <= 'Z' ? c + 32 : c; }
    static SqlException invalid(String detail) { return new SqlException("2201B", "invalid regular expression: " + detail); }
    private static SqlException tooLarge() { return new SqlException("54001", "regular expression program is too large"); }
    private static long add(long a, long b) { return Math.min(MAX_PROGRAM + 1L, a + b); }
    private static long multiply(long a, long b) { return a == 0 || b == 0 ? 0 : a > MAX_PROGRAM / b ? MAX_PROGRAM + 1L : a * b; }
    private static long size(Node node) {
        if (node.kind.equals("concat")) { long count = 0; for (Node child : node.children) count = add(count, size(child)); return count; }
        if (node.kind.equals("alt")) { long count = 2L * (node.children.size() - 1); for (Node child : node.children) count = add(count, size(child)); return count; }
        if (node.kind.equals("plus")) return add(size(node.children.getFirst()), 1);
        if (node.kind.equals("group")) return add(size(node.children.getFirst()), 2);
        if (node.kind.equals("repeat")) { long body = size(node.children.getFirst()); return node.max < 0 ? add(multiply(node.min, body), add(body, 2)) : add(multiply(node.max, body), node.max - node.min); }
        return 1;
    }
    private static final class Compiler {
        final ArrayList<Instruction> code = new ArrayList<>(); final ArrayList<CharacterClass> classes = new ArrayList<>();
        int emit(Instruction instruction) { if (code.size() >= MAX_PROGRAM) throw tooLarge(); code.add(instruction); return code.size() - 1; }
        int emit(String op, int x) { return emit(new Instruction(op, x, 0)); }
        void split(int at, int x, int y, boolean lazy) { code.set(at, new Instruction("split", lazy ? y : x, lazy ? x : y)); }
        void node(Node n) {
            switch (n.kind) {
                case "concat" -> { for (Node child : n.children) node(child); }
                case "alt" -> { var jumps = new ArrayList<Integer>(); for (int i = 0; i < n.children.size() - 1; i++) { int split = emit("split", 0); int left = code.size(); node(n.children.get(i)); jumps.add(emit("jmp", 0)); split(split, left, code.size(), false); } node(n.children.getLast()); for (int jump : jumps) code.set(jump, new Instruction("jmp", code.size(), 0)); }
                case "group" -> { emit("save", n.value * 2); node(n.children.getFirst()); emit("save", n.value * 2 + 1); }
                case "class" -> { emit("class", classes.size()); classes.add(n.characterClass); }
                case "plus" -> { int begin = code.size(); node(n.children.getFirst()); int at = emit("split", 0); split(at, begin, code.size(), n.lazy); }
                case "repeat" -> { Node body = n.children.getFirst(); for (long i = 0; i < n.min; i++) node(body); if (n.max < 0) star(body, n.lazy); else for (long i = n.min; i < n.max; i++) quest(body, n.lazy); }
                default -> emit(n.kind, n.value);
            }
        }
        void quest(Node body, boolean lazy) { int at = emit("split", 0), begin = code.size(); node(body); split(at, begin, code.size(), lazy); }
        void star(Node body, boolean lazy) {
            int at = emit("split", 0), begin = code.size(); node(body);
            if (body.nullable()) { int loop = emit("split", 0); split(loop, begin, code.size(), lazy); }
            else emit("jmp", at);
            split(at, begin, code.size(), lazy);
        }
    }
    private static final class Parser {
        final int[] points; final boolean fold, quote, dotAll, lines, newlineSensitive; int at, groups, depth;
        Parser(String pattern, boolean fold, boolean quote, boolean dotAll, boolean lines, boolean newlineSensitive) { points = pattern.codePoints().toArray(); this.fold = fold; this.quote = quote; this.dotAll = dotAll; this.lines = lines; this.newlineSensitive = newlineSensitive; }
        Node parse() { if (quote) { var literals = new ArrayList<Node>(); for (int c : points) literals.add(literal(c)); return new Node("concat", literals); } Node result = alternative(); if (at != points.length) throw invalid("unbalanced parenthesis"); return result; }
        Node alternative() { var branches = new ArrayList<Node>(); branches.add(concat()); while (take('|')) branches.add(concat()); return branches.size() == 1 ? branches.getFirst() : new Node("alt", branches); }
        Node concat() { var nodes = new ArrayList<Node>(); while (at < points.length && points[at] != ')' && points[at] != '|') nodes.add(quantified()); return new Node("concat", nodes); }
        Node quantified() {
            Node atom = atom(); if (at == points.length) return atom;
            long min, max; int c = points[at];
            if (c == '*') { at++; min = 0; max = -1; } else if (c == '+') { at++; min = 1; max = -1; } else if (c == '?') { at++; min = 0; max = 1; }
            else if (c == '{') { long[] bounds = bounds(); if (bounds == null) return atom; min = bounds[0]; max = bounds[1]; }
            else return atom;
            boolean lazy = take('?');
            // Plus has its own exact lowering, unlike a mandatory copy followed by Star.
            if (c == '+') return new Node("plus", 0, List.of(atom), null, 1, -1, lazy);
            return new Node("repeat", 0, List.of(atom), null, min, max, lazy);
        }
        Node atom() {
            int c = points[at++];
            if (c == '*' || c == '+' || c == '?' || c == '{' && validBounds(at - 1)) throw invalid("quantifier operand missing");
            if (c == '(') { if (++depth > 256) throw tooLarge(); boolean capture = true; if (take('?')) { if (!take(':')) throw invalid("unbalanced parenthesis"); capture = false; } int group = capture ? ++groups : 0; Node child = alternative(); if (!take(')')) throw invalid("unbalanced parenthesis"); depth--; return capture ? new Node("group", group, List.of(child), null, 0, 0, false) : child; }
            if (c == '[') return characterClass();
            if (c == '\\') return escape(false);
            if (c == '.') return dotAll ? new Node("class", 0, List.of(), new CharacterClass(List.of(), true), 0, 0, false) : new Node("any", 0);
            if (c == '^') return new Node(lines ? "assertlinestart" : "assertstart", 0);
            if (c == '$') return new Node(lines ? "assertlineend" : "assertend", 0);
            return literal(c);
        }
        Node literal(int c) { return new Node("char", fold ? fold(c) : c); }
        Node escape(boolean inClass) {
            if (at == points.length) throw invalid("trailing backslash"); int c = points[at++];
            if ("dDwWsS".indexOf(c) >= 0) { List<Range> ranges = switch (Character.toLowerCase(c)) { case 'd' -> List.of(new Range(48,57)); case 'w' -> List.of(new Range(48,57),new Range(65,90),new Range(95,95),new Range(97,122)); default -> List.of(new Range(9,13),new Range(32,32)); }; return new Node("class", 0, List.of(), new CharacterClass(ranges, Character.isUpperCase(c)), 0, 0, false); }
            if (c == 'A' && !inClass) return new Node("assertstart", 0); if (c == 'z' && !inClass) return new Node("assertend", 0);
            int control = switch (c) { case 'n' -> 10; case 't' -> 9; case 'r' -> 13; case 'f' -> 12; case 'v' -> 11; default -> -1; }; if (control >= 0) return literal(control);
            if (c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9') throw invalid("invalid escape \\" + Character.toString(c)); return literal(c);
        }
        Node classItem() { if (at == points.length) throw invalid("unbalanced bracket expression"); int c = points[at++]; if (c == '[' && at < points.length && ":.=".indexOf(points[at]) >= 0) throw invalid("unbalanced bracket expression"); return c == '\\' ? escape(true) : literal(c); }
        Node characterClass() {
            boolean negated = take('^'); var ranges = new ArrayList<Range>(); boolean first = true;
            while (at < points.length && (first || points[at] != ']')) {
                Node left = classItem(); if (left.kind.equals("char") && at + 1 < points.length && points[at] == '-' && points[at + 1] != ']' && !(first && left.value == '-')) { at++; Node right = classItem(); if (!right.kind.equals("char") || right.value < left.value) throw invalid("unbalanced bracket expression"); ranges.add(new Range(left.value, right.value)); }
                else appendRanges(ranges, left);
                first = false;
            }
            if (!take(']')) throw invalid("unbalanced bracket expression"); if (negated && newlineSensitive) ranges.add(new Range(10,10));
            ranges.sort(Comparator.comparingInt(Range::lo)); var merged = new ArrayList<Range>(); for (Range range : ranges) { if (!merged.isEmpty() && range.lo <= merged.getLast().hi + 1) { Range previous = merged.removeLast(); merged.add(new Range(previous.lo, Math.max(previous.hi, range.hi))); } else merged.add(range); }
            return new Node("class", 0, List.of(), new CharacterClass(List.copyOf(merged), negated), 0, 0, false);
        }
        void appendRanges(List<Range> result, Node item) { if (item.kind.equals("char")) { result.add(new Range(item.value,item.value)); return; } CharacterClass c = item.characterClass; if (!c.negated) result.addAll(c.ranges); else { int lo = 0; for (Range range : c.ranges) { if (lo < range.lo) result.add(new Range(lo,range.lo - 1)); lo = range.hi + 1; } if (lo <= 0x10ffff) result.add(new Range(lo,0x10ffff)); } }
        boolean validBounds(int position) { int saved = at; at = position; long[] result = bounds(); at = saved; return result != null; }
        long[] bounds() { int saved = at; at++; if (at == points.length || !digit(points[at])) { at = saved; return null; } long min = number(), max = min; if (take(',')) max = at < points.length && digit(points[at]) ? number() : -1; if (!take('}')) { at = saved; return null; } if (max >= 0 && max < min) throw invalid("invalid repetition count"); return new long[] {min,max}; }
        long number() { long result = 0; while (at < points.length && digit(points[at])) result = Math.min(MAX_PROGRAM + 1L, result * 10 + points[at++] - '0'); return result; }
        static boolean digit(int c) { return c >= '0' && c <= '9'; }
        boolean take(int c) { if (at < points.length && points[at] == c) { at++; return true; } return false; }
    }
    record Match(Slots slots) { int start(int group) { return slots.get(group * 2); } int end(int group) { return slots.get(group * 2 + 1); } }
    /** Persistent radix-16 capture array; each Save copies at most four bounded nodes. */
    private static final class Slots {
        final Object root; final int depth, count;
        Slots(Object root, int depth, int count) { this.root = root; this.depth = depth; this.count = count; }
        static Slots empty(int count) { int depth = 1, capacity = 16; while (capacity < count) { depth++; capacity *= 16; } return new Slots(null, depth, count); }
        Slots with(int slot, int value) { return new Slots(write(root, depth, slot, value, count), depth, count); }
        private static Object write(Object node, int depth, int slot, int value, int count) { if (depth == 1) { int[] values; if (node == null) { values = new int[Math.min(16,count)]; Arrays.fill(values,-1); } else values = ((int[])node).clone(); values[slot & 15] = value; return values; } Object[] children = node == null ? new Object[16] : ((Object[])node).clone(); int index = slot >>> ((depth - 1) * 4) & 15; children[index] = write(children[index],depth - 1,slot,value,16); return children; }
        int get(int slot) { if (slot < 0 || slot >= count) return -1; Object current = root; for (int level = depth; level > 1; level--) { if (current == null) return -1; current = ((Object[])current)[slot >>> ((level - 1) * 4) & 15]; } return current == null ? -1 : ((int[])current)[slot & 15]; }
    }
    private record Thread(int pc, Slots slots) {}
    private static final class ThreadList { final ArrayList<Thread> threads = new ArrayList<>(); final int generation; ThreadList(int generation) { this.generation = generation; } }
    private static final class Runner {
        final RegexProgram program; final int[] marks; int generation;
        Runner(RegexProgram program) { this.program = program; marks = new int[program.size()]; }
        int generation() { if (++generation == 0) { Arrays.fill(marks,0); generation = 1; } return generation; }
        Match search(int[] input, int start, CostMeter meter) { return search(input,start,meter,null,null); }
        Match search(int[] input, int start, CostMeter meter, boolean[] gates, boolean[] ends) {
            ThreadList current = new ThreadList(generation()); add(current, 0, Slots.empty(2 * (program.groups + 1)), start,input,meter,gates); Slots matched = null;
            for (int position = start; position <= input.length; position++) {
                ThreadList next = new ThreadList(generation());
                for (Thread thread : current.threads) { Instruction instruction = program.instructions.get(thread.pc); if (instruction.op.equals("match")) { matched = thread.slots; if (ends != null) { ends[position] = true; continue; } break; } if (position == input.length) continue;
                    boolean admit = switch (instruction.op) { case "char" -> input[position] == instruction.x; case "any" -> input[position] != 10; case "anychar" -> true; case "class" -> program.classes.get(instruction.x).admits(input[position]); default -> false; };
                    if (admit) add(next,thread.pc + 1,thread.slots,position + 1,input,meter,gates);
                }
                meter.guard(); if (next.threads.isEmpty()) break; current = next;
            }
            return matched == null ? null : new Match(matched);
        }
        void add(ThreadList list, int pc, Slots slots, int position, int[] input, CostMeter meter, boolean[] gates) {
            var work = new ArrayDeque<Thread>(); work.push(new Thread(pc,slots));
            while (!work.isEmpty()) { Thread thread = work.pop(); int index = thread.pc; if (marks[index] == list.generation) continue; marks[index] = list.generation; meter.charge(Costs.REGEX_STEP); Instruction instruction = program.instructions.get(index);
                switch (instruction.op) {
                    case "gate" -> { if (gates != null && gates[position]) work.push(new Thread(index + 1,thread.slots)); }
                    case "jmp" -> work.push(new Thread(instruction.x,thread.slots));
                    case "split" -> { work.push(new Thread(instruction.y,thread.slots)); work.push(new Thread(instruction.x,thread.slots)); }
                    case "save" -> work.push(new Thread(index + 1,thread.slots.with(instruction.x,position)));
                    case "assertstart", "assertend", "assertlinestart", "assertlineend" -> { boolean admit = switch (instruction.op) { case "assertstart" -> position == 0; case "assertend" -> position == input.length; case "assertlinestart" -> position == 0 || input[position - 1] == 10; default -> position == input.length || input[position] == 10; }; if (admit) work.push(new Thread(index + 1,thread.slots)); }
                    default -> list.threads.add(thread);
                }
            }
        }
    }
}
