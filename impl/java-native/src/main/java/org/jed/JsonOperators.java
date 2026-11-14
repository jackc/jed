package org.jed;

import java.util.ArrayList;
import java.util.Set;

/** Classic document operators, including the textual json source-span contract. */
final class JsonOperators {
    private JsonOperators() {}
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression path = JsonPathFunctions.binary(syntax, resolver); if (path != null) return path;
        String op = syntax.op();
        if (!Set.of("->", "->>", "#>", "#>>", "@>", "<@", "?", "?|", "?&", "||", "-", "#-").contains(op)) return null;
        ScalarPlan.Expression a = resolver.resolve(syntax.left(), null), b = resolver.resolve(syntax.right(), null);
        if (a.type() != SqlType.JSON && a.type() != SqlType.JSONB && b.type() != SqlType.JSONB) return null;
        boolean access = Set.of("->", "->>", "#>", "#>>").contains(op);
        if (a.type() == SqlType.UNKNOWN || syntax.left() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) a = resolver.resolve(syntax.left(), SqlType.JSONB);
        if (!access && a.type() != SqlType.JSONB || access && a.type() != SqlType.JSON && a.type() != SqlType.JSONB) throw missing();
        SqlType wanted = Set.of("#>", "#>>", "#-", "?|", "?&").contains(op) ? SqlType.array(SqlType.TEXT)
            : Set.of("@>", "<@", "||").contains(op) ? SqlType.JSONB : op.equals("?") ? SqlType.TEXT : b.type() == SqlType.UNKNOWN ? SqlType.TEXT : b.type();
        b = resolver.resolve(syntax.right(), wanted);
        if (Set.of("->", "->>", "-").contains(op)) {
            if (!b.type().isInteger() && b.type().kind() != SqlType.Kind.TEXT && !(op.equals("-") && b.type().equals(SqlType.array(SqlType.TEXT)))) throw missing();
        } else if (!b.type().equals(wanted)) throw missing();
        SqlType result = op.equals("->>") || op.equals("#>>") ? SqlType.TEXT : access ? a.type() : Set.of("||", "-", "#-").contains(op) ? SqlType.JSONB : SqlType.BOOLEAN;
        ScalarPlan.Expression left = a, right = b;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue x = left.run(meter); if (x.isNull()) return SqlValue.nullValue(result);
            SqlValue y = right.run(meter); if (y.isNull()) return SqlValue.nullValue(result);
            if (access && op.startsWith("#")) for (SqlValue step : y.asArray().elements()) if (step.isNull()) return SqlValue.nullValue(result);
            JsonNode node = x.type() == SqlType.JSON ? JsonNode.parse(x.asJson(), true) : x.asJsonb();
            if (access) {
                JsonNode selected = node;
                if (op.startsWith("#")) {
                    if (x.type() == SqlType.JSON) selected = verbatimPath(node, y.asArray());
                    else for (SqlValue step : y.asArray().elements()) { if (step.isNull()) { selected = null; break; } selected = path(selected, step.asText()); if (selected == null) break; }
                } else if (y.type().isInteger()) {
                    long index = y.asInteger(); selected = node.get(index);
                    if (selected == null && x.type() == SqlType.JSONB && node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT && (index == 0 || index == -1)) selected = node;
                } else selected = node.get(y.asText());
                if (selected == null || result == SqlType.TEXT && selected.kind() == JsonNode.Kind.NULL) return SqlValue.nullValue(result);
                boolean leadingScalar = op.startsWith("#") && y.asArray().size() == 0 && selected.kind() != JsonNode.Kind.ARRAY && selected.kind() != JsonNode.Kind.OBJECT;
                if (result == SqlType.TEXT && x.type() == SqlType.JSONB && (!left.constant() || !right.constant()) && (selected.kind() == JsonNode.Kind.OBJECT || selected.kind() == JsonNode.Kind.ARRAY)) meter.scalarInput(selected.renderedBytes());
                String raw = x.type() == SqlType.JSON ? x.asJson().substring(leadingScalar ? 0 : selected.start, selected.end) : selected.render();
                if (result == SqlType.TEXT) return SqlValue.ofText(selected.kind() == JsonNode.Kind.STRING ? selected.string() : raw);
                return result == SqlType.JSON ? SqlValue.ofJson(raw) : SqlValue.ofJsonb(selected);
            }
            if (op.equals("@>")) return SqlValue.ofBoolean(contains(node, y.asJsonb(), true));
            if (op.equals("<@")) return SqlValue.ofBoolean(contains(y.asJsonb(), node, true));
            if (op.equals("?")) return SqlValue.ofBoolean(exists(node, y.asText()));
            if (op.equals("?|") || op.equals("?&")) {
                boolean all = op.equals("?&"); for (SqlValue key : y.asArray().elements()) if (!key.isNull() && exists(node, key.asText()) != all) return SqlValue.ofBoolean(!all); return SqlValue.ofBoolean(all);
            }
            JsonNode output;
            if (op.equals("||")) {
                JsonNode other = y.asJsonb();
                if (node.kind() == JsonNode.Kind.OBJECT && other.kind() == JsonNode.Kind.OBJECT) { var members = new ArrayList<>(node.members()); members.addAll(other.members()); output = JsonNode.object(members).canonical(); }
                else { var items = new ArrayList<JsonNode>(); if (node.kind() == JsonNode.Kind.ARRAY) items.addAll(node.elements()); else items.add(node); if (other.kind() == JsonNode.Kind.ARRAY) items.addAll(other.elements()); else items.add(other); output = JsonNode.array(items); }
                output.checkDepth(); if (!left.constant() || !right.constant()) meter.scalarInput(JsonCodec.memory(output) - MemorySchedule.VALUE);
            } else if (op.equals("#-")) output = deletePath(node, y.asArray(), 0);
            else output = delete(node, y);
            return SqlValue.ofJsonb(output);
        }, left.constant() && right.constant());
    }
    private static JsonNode verbatimPath(JsonNode node, SqlArray path) {
        Long[] indexes = new Long[path.size()]; boolean[] rebased = new boolean[path.size()];
        for (int i = 0; i < path.size(); i++) if (!path.element(i).isNull()) indexes[i] = pathIndex(path.element(i).asText());
        rebaseIndexes(node, 0, indexes, rebased);
        return verbatimPath(node, path, 0, indexes);
    }
    private static void rebaseIndexes(JsonNode node, int depth, Long[] indexes, boolean[] rebased) {
        if (depth >= indexes.length) return;
        if (node.kind() == JsonNode.Kind.ARRAY) {
            if (!rebased[depth] && indexes[depth] != null && indexes[depth] < 0) { indexes[depth] += node.elements().size(); rebased[depth] = true; }
            for (JsonNode child : node.elements()) rebaseIndexes(child, depth + 1, indexes, rebased);
        } else if (node.kind() == JsonNode.Kind.OBJECT) for (JsonNode.Member member : node.members()) rebaseIndexes(member.value(), depth + 1, indexes, rebased);
    }
    private static JsonNode verbatimPath(JsonNode node, SqlArray path, int at, Long[] indexes) {
        if (at == path.size()) return node;
        SqlValue step = path.element(at); if (step.isNull()) return null;
        if (node.kind() == JsonNode.Kind.OBJECT) {
            JsonNode result = null;
            for (JsonNode.Member member : node.members()) if (member.key().equals(step.asText())) { JsonNode found = verbatimPath(member.value(), path, at + 1, indexes); if (found != null) result = found; }
            return result;
        }
        if (node.kind() != JsonNode.Kind.ARRAY || indexes[at] == null || indexes[at] < 0) return null;
        JsonNode next = node.get(indexes[at]); return next == null ? null : verbatimPath(next, path, at + 1, indexes);
    }
    static JsonNode path(JsonNode node, String key) {
        if (node == null) return null;
        if (node.kind() == JsonNode.Kind.OBJECT) return node.get(key);
        if (node.kind() != JsonNode.Kind.ARRAY) return null;
        Long index = pathIndex(key); return index == null ? null : node.get(index);
    }
    private static Long pathIndex(String text) {
        int at = 0;
        while (at < text.length() && " \t\n\r\f\u000b".indexOf(text.charAt(at)) >= 0) at++;
        boolean negative = false;
        if (at < text.length() && (text.charAt(at) == '+' || text.charAt(at) == '-')) negative = text.charAt(at++) == '-';
        if (at == text.length()) return null;
        long value = 0, limit = negative ? 2147483648L : 2147483647L;
        for (; at < text.length(); at++) {
            char digit = text.charAt(at); if (digit < '0' || digit > '9') return null;
            value = value * 10 + digit - '0'; if (value > limit) return null;
        }
        return negative ? -value : value;
    }
    static boolean exists(JsonNode node, String key) {
        if (node.kind() == JsonNode.Kind.OBJECT) return node.get(key) != null;
        if (node.kind() == JsonNode.Kind.STRING) return node.string().equals(key);
        if (node.kind() == JsonNode.Kind.ARRAY) for (JsonNode child : node.elements()) if (child.kind() == JsonNode.Kind.STRING && child.string().equals(key)) return true;
        return false;
    }
    static boolean contains(JsonNode a, JsonNode b, boolean root) {
        if (a.kind() != b.kind()) {
            if (root && a.kind() == JsonNode.Kind.ARRAY && b.kind() != JsonNode.Kind.ARRAY && b.kind() != JsonNode.Kind.OBJECT) for (JsonNode child : a.elements()) if (child.equals(b)) return true;
            return false;
        }
        if (a.kind() == JsonNode.Kind.OBJECT) {
            for (JsonNode.Member member : b.members()) { JsonNode other = a.get(member.key()); if (other == null || !contains(other, member.value(), false)) return false; }
            return true;
        }
        if (a.kind() == JsonNode.Kind.ARRAY) {
            for (JsonNode needle : b.elements()) { boolean found = false; for (JsonNode item : a.elements()) if (contains(item, needle, false)) { found = true; break; } if (!found) return false; }
            return true;
        }
        return a.equals(b);
    }
    static JsonNode delete(JsonNode node, SqlValue key) {
        if (node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT) throw new SqlException("22023", "cannot delete from scalar");
        if (key.type().kind() == SqlType.Kind.ARRAY) { if (key.asArray().dimensions() > 1) throw new SqlException("2202E", "wrong number of array subscripts"); for (SqlValue item : key.asArray().elements()) if (!item.isNull()) node = delete(node, item); return node; }
        if (node.kind() == JsonNode.Kind.OBJECT) {
            if (key.type().isInteger()) throw new SqlException("22023", "cannot delete from object using integer index");
            var members = new ArrayList<JsonNode.Member>(); for (JsonNode.Member member : node.members()) if (!member.key().equals(key.asText())) members.add(member); return JsonNode.object(members);
        }
        long index = key.type().isInteger() ? key.asInteger() : Long.MAX_VALUE; if (index < 0) index += node.elements().size();
        var items = new ArrayList<JsonNode>(); for (int i = 0; i < node.elements().size(); i++) { JsonNode item = node.elements().get(i); if (i != index && !(key.type().kind() == SqlType.Kind.TEXT && item.kind() == JsonNode.Kind.STRING && item.string().equals(key.asText()))) items.add(item); } return JsonNode.array(items);
    }
    static JsonNode deletePath(JsonNode node, SqlArray path, int at) {
        if (at == 0) {
            if (path.dimensions() > 1) throw new SqlException("2202E", "wrong number of array subscripts");
            if (node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT) throw new SqlException("22023", "cannot delete path in scalar");
        }
        if (node.kind() == JsonNode.Kind.OBJECT && node.members().isEmpty() || node.kind() == JsonNode.Kind.ARRAY && node.elements().isEmpty()) return node;
        if (at == path.size()) return node;
        SqlValue step = path.element(at); if (step.isNull()) throw new SqlException("22004", "path element is NULL");
        if (node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT) { if (at == 0) throw new SqlException("22023", "cannot delete from scalar"); return node; }
        SqlValue key = step;
        if (node.kind() == JsonNode.Kind.ARRAY) { try { key = SqlValue.ofInteger(Integer.parseInt(step.asText().stripLeading())); } catch (NumberFormatException failure) { throw new SqlException("22P02", "path element is not an integer"); } }
        if (at == path.size() - 1) return delete(node, key);
        if (node.kind() == JsonNode.Kind.OBJECT) { var members = new ArrayList<JsonNode.Member>(); for (JsonNode.Member member : node.members()) members.add(new JsonNode.Member(member.key(), member.key().equals(key.asText()) ? deletePath(member.value(), path, at + 1) : member.value())); return JsonNode.object(members); }
        long index = key.asInteger(); if (index < 0) index += node.elements().size(); var items = new ArrayList<>(node.elements()); if (index >= 0 && index < items.size()) items.set((int) index, deletePath(items.get((int) index), path, at + 1)); return JsonNode.array(items);
    }
    private static SqlException missing() { return new SqlException("42883", "JSON operator does not exist for argument types"); }
}
