package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Index-key framing can locate a suffix without materializing recursive values. */
public final class IndexKeyReaderTest {
    public static void main(String[] args) {
        SqlType record = SqlType.composite("pair", List.of(new SqlType.Field("s", SqlType.TEXT), new SqlType.Field("n", SqlType.I32)));
        List<SqlValue> values = new ArrayList<>(List.of(SqlValue.ofInteger(SqlType.I16, -4), SqlValue.ofInteger(SqlType.I32, 42), SqlValue.ofInteger(SqlType.I64, 900),
            SqlValue.ofBoolean(true), SqlValue.ofText("aé中b"), SqlValue.ofBytes(new byte[]{0,1,-1}), SqlValue.ofDecimal(Decimal.parse("-123.45")), SqlValue.ofDecimal(Decimal.parse("0")),
            SqlValue.ofDecimal(Decimal.parse("234.56")), SqlValue.ofJsonb(JsonNode.object(List.of(new JsonNode.Member("a", JsonNode.array(List.of(JsonNode.string("qéx"), JsonNode.number(Decimal.parse("-4")))))))),
            SqlValue.ofComposite(record, List.of(SqlValue.ofText("x"), SqlValue.nullValue(SqlType.I32)))));
        values.add(SqlValue.ofRange(SqlType.range(SqlType.I32), SqlRange.create(SqlType.I32, SqlValue.ofInteger(SqlType.I32, -1), SqlValue.ofInteger(SqlType.I32, 8), true, false)));
        values.add(SqlValue.ofArray(SqlType.array(SqlType.TEXT), new SqlArray(new int[]{3}, new int[]{-2}, List.of(SqlValue.ofText("x"), SqlValue.nullValue(SqlType.TEXT), SqlValue.ofText("")))));
        byte[] suffix = new byte[]{17,0,-1,42};
        for (boolean descending : new boolean[]{false,true}) for (boolean nullsFirst : new boolean[]{false,true}) {
            List<Engine.IndexKey> keys = new ArrayList<>(); Encoding.Sink bytes = new Encoding.Sink();
            for (SqlValue value : values) { var key = new Engine.IndexKey(0,null,value.type(),descending,nullsFirst); keys.add(key); bytes.raw(Indexes.slot(value,key)); }
            var nullable = new Engine.IndexKey(0,null,SqlType.TEXT,descending,nullsFirst); keys.add(nullable); bytes.raw(Indexes.slot(SqlValue.nullValue(SqlType.TEXT),nullable)); bytes.raw(suffix);
            check(Arrays.equals(IndexKeyReader.primaryKey(bytes.bytes(),keys),suffix),"recursive suffix differs");
        }
        // Canonical UCA keys have three big-endian weight levels, then escaped original text.
        for (boolean descending : new boolean[]{false,true}) {
            byte[] collated = new byte[]{0,0,5,0,0,0,6,0,0,0,7,0,0,97,0,1};
            if(descending)for(int i=0;i<collated.length;i++)collated[i]=(byte)~collated[i];
            byte[] full=Arrays.copyOf(collated,collated.length+suffix.length);System.arraycopy(suffix,0,full,collated.length,suffix.length);
            check(Arrays.equals(IndexKeyReader.primaryKey(full,List.of(new Engine.IndexKey(0,null,SqlType.TEXT,descending,descending,"test"))),suffix),"collated suffix differs");
        }
        for(byte[] invalid:List.of(new byte[]{0},new byte[]{2,0},new byte[]{0,0,2,3})) {
            try { IndexKeyReader.primaryKey(invalid,List.of(new Engine.IndexKey(0,null,SqlType.TEXT,false,false)));throw new AssertionError("malformed key accepted"); }
            catch(SqlException failure){check(failure.sqlState().equals("XX001"),"malformed key classification");}
        }
        System.out.println("IndexKeyReaderTest passed (recursive, descending, null, collated and malformed key framing)");
    }
    private static void check(boolean condition,String message){if(!condition)throw new AssertionError(message);}
}
