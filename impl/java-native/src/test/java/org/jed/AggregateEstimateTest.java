package org.jed;

import java.util.List;

/** Planner-only scope/provenance contracts; the synthetic source must never execute. */
public final class AggregateEstimateTest {
    private static void equal(long expected,long actual) { if (expected!=actual) throw new AssertionError(expected+" != "+actual); }
    public static void main(String[] args) {
        try (Database database=Database.memory(); Session session=database.session()) {
            CostMeter meter=new CostMeter(session,null);
            try (QueryEnvironment environment=new QueryEnvironment((statement,scope) -> {
                SqlParser.Select select=(SqlParser.Select)statement;
                ScalarPlan.Expression output=scope.resolver().resolve(select.items().getFirst().expression(),null);
                return new QueryPlan() {
                    public List<String> names() {return List.of("value");}
                    public List<SqlType> types() {return List.of(output.type());}
                    public Row next(CostMeter ignored) {throw new AssertionError("estimate executed subquery");}
                    public PlanNode explain(boolean verbose) {return new PlanNode("Result","",1,7,List.of(),"buffered");}
                };
            },new SqlValue[0],meter)) {
                environment.estimateCapture(true);
                var bindings=List.of(new ScalarPlan.Binding(List.of("a"),"id",SqlType.I32),new ScalarPlan.Binding(List.of("a"),"v",SqlType.I64));
                var dependency=new RelationPlan.FunctionalKey(List.of(0),List.of(0,1));
                SqlParser.Select query=new SqlParser("SELECT (SELECT a.v), a.id IN (SELECT a.id), (SELECT 1), sum((SELECT a.v)) FROM a GROUP BY a.id HAVING (SELECT a.id)>0").parse();
                AggregatePlan.captureEstimates(query,bindings,List.of(dependency),new SqlValue[0],environment);
                equal(21,environment.hiddenStageEstimate(query.items().get(0).expression(),3));
                equal(21,environment.hiddenStageEstimate(query.items().get(1).expression(),3));
                equal(7,environment.hiddenStageEstimate(query.items().get(2).expression(),3));
                equal(0,environment.hiddenStageEstimate(query.items().get(3).expression(),3));
                SqlParser.Call aggregate=(SqlParser.Call)query.items().get(3).expression();
                equal(35,environment.hiddenEstimate(aggregate.args().getFirst(),5));
                equal(21,environment.hiddenStageEstimate(query.having(),3));
                if (environment.hasInit(query.items().get(2).expression())) throw new AssertionError("estimate registered executable initializer");
                environment.initialize(meter); equal(0,meter.cost());
            }
        }
        System.out.println("AggregateEstimateTest: grouped correlation, rewritten membership provenance, once-only estimates and non-execution passed");
    }
}
