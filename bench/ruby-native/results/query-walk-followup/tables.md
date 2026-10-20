# Query-walk measurements

All times are microseconds per complete query. Source comparisons use means of two process medians; each process has five samples.

## main: 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.797 | 12.607 | 1.015× | 12.756–12.838 | 12.509–12.705 |
| native-prepared | pk_hit | 8.428 | 8.451 | 0.997× | 8.358–8.498 | 8.376–8.526 |
| native-prepared | indexed_join | 274.250 | 266.555 | 1.029× | 272.066–276.435 | 265.636–267.475 |
| native-prepared | three_table_join | 384.854 | 372.628 | 1.033× | 379.875–389.834 | 369.293–375.962 |
| native-prepared | three_table_join_miss | 318.098 | 308.823 | 1.030× | 313.560–322.635 | 306.644–311.002 |
| native-prepared | three_table_join_range | 2882.474 | 2867.748 | 1.005× | 2871.692–2893.256 | 2864.069–2871.426 |
| native-prepared | three_table_computed_key | 415.476 | 405.022 | 1.026× | 413.601–417.351 | 402.926–407.117 |
| native-prepared | four_table_join | 531.941 | 509.323 | 1.044× | 528.832–535.049 | 508.363–510.282 |
| native-prepared | three_table_left_fence | 1321.745 | 1346.110 | 0.982× | 1319.988–1323.501 | 1340.171–1352.049 |
| native-query | scalar | 13.744 | 13.668 | 1.006× | 13.478–14.011 | 13.334–14.002 |
| native-query | pk_hit | 9.712 | 9.757 | 0.995× | 9.601–9.823 | 9.543–9.971 |
| native-query | indexed_join | 265.145 | 261.302 | 1.015× | 260.310–269.981 | 255.041–267.562 |
| native-query | three_table_join | 378.095 | 366.244 | 1.032× | 373.920–382.270 | 356.316–376.172 |
| native-query | three_table_join_miss | 307.523 | 299.364 | 1.027× | 305.030–310.016 | 292.079–306.650 |
| native-query | three_table_join_range | 2818.662 | 2830.311 | 0.996× | 2775.412–2861.913 | 2782.166–2878.456 |
| native-query | three_table_computed_key | 412.190 | 400.302 | 1.030× | 405.048–419.332 | 397.697–402.908 |
| native-query | four_table_join | 508.166 | 508.015 | 1.000× | 496.752–519.580 | 504.408–511.622 |
| native-query | three_table_left_fence | 1289.666 | 1317.322 | 0.979× | 1262.846–1316.486 | 1278.115–1356.529 |
| native-eager | scalar | 15.196 | 15.240 | 0.997× | 15.186–15.207 | 15.076–15.404 |
| native-eager | pk_hit | 9.401 | 9.468 | 0.993× | 9.386–9.416 | 9.320–9.617 |
| native-eager | indexed_join | 232.475 | 222.367 | 1.045× | 232.145–232.804 | 221.688–223.047 |
| native-eager | three_table_join | 344.425 | 331.068 | 1.040× | 344.180–344.669 | 327.833–334.304 |
| native-eager | three_table_join_miss | 292.396 | 276.233 | 1.059× | 288.959–295.833 | 273.224–279.241 |
| native-eager | three_table_join_range | 2666.441 | 2684.542 | 0.993× | 2662.310–2670.572 | 2663.991–2705.092 |
| native-eager | three_table_computed_key | 372.784 | 357.246 | 1.043× | 371.540–374.029 | 354.640–359.853 |
| native-eager | four_table_join | 489.695 | 461.579 | 1.061× | 487.801–491.590 | 456.096–467.061 |
| native-eager | three_table_left_fence | 1231.899 | 1239.088 | 0.994× | 1227.370–1236.428 | 1235.094–1243.082 |

## main: 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.829 | 5.080 | 0.951× | 4.790–4.869 | 5.024–5.135 |
| native-prepared | pk_hit | 3.956 | 4.123 | 0.959× | 3.946–3.966 | 4.095–4.151 |
| native-prepared | indexed_join | 158.068 | 157.223 | 1.005× | 156.468–159.668 | 156.632–157.814 |
| native-prepared | three_table_join | 231.214 | 231.280 | 1.000× | 228.737–233.691 | 229.607–232.954 |
| native-prepared | three_table_join_miss | 206.303 | 202.110 | 1.021× | 206.206–206.400 | 201.764–202.457 |
| native-prepared | three_table_join_range | 1443.150 | 1499.134 | 0.963× | 1439.174–1447.127 | 1491.909–1506.359 |
| native-prepared | three_table_computed_key | 260.727 | 257.040 | 1.014× | 257.763–263.691 | 255.441–258.639 |
| native-prepared | four_table_join | 330.952 | 330.391 | 1.002× | 330.189–331.714 | 328.054–332.728 |
| native-prepared | three_table_left_fence | 711.801 | 747.814 | 0.952× | 703.057–720.544 | 740.629–754.999 |
| native-query | scalar | 5.471 | 5.476 | 0.999× | 5.268–5.673 | 5.381–5.571 |
| native-query | pk_hit | 4.521 | 4.581 | 0.987× | 4.400–4.642 | 4.493–4.668 |
| native-query | indexed_join | 159.783 | 157.661 | 1.013× | 157.361–162.205 | 155.492–159.830 |
| native-query | three_table_join | 240.372 | 230.310 | 1.044× | 233.260–247.485 | 224.253–236.366 |
| native-query | three_table_join_miss | 207.469 | 200.709 | 1.034× | 203.063–211.874 | 197.146–204.271 |
| native-query | three_table_join_range | 1471.463 | 1484.416 | 0.991× | 1438.074–1504.852 | 1452.890–1515.943 |
| native-query | three_table_computed_key | 262.179 | 255.445 | 1.026× | 257.645–266.713 | 250.591–260.300 |
| native-query | four_table_join | 333.208 | 327.511 | 1.017× | 330.092–336.323 | 320.929–334.093 |
| native-query | three_table_left_fence | 710.598 | 738.439 | 0.962× | 699.708–721.488 | 719.043–757.835 |
| native-eager | scalar | 5.421 | 5.412 | 1.002× | 5.380–5.463 | 5.395–5.430 |
| native-eager | pk_hit | 3.955 | 3.977 | 0.994× | 3.953–3.956 | 3.952–4.002 |
| native-eager | indexed_join | 149.882 | 141.911 | 1.056× | 149.000–150.764 | 141.336–142.487 |
| native-eager | three_table_join | 224.291 | 216.497 | 1.036× | 223.052–225.530 | 214.442–218.553 |
| native-eager | three_table_join_miss | 200.760 | 189.773 | 1.058× | 199.179–202.341 | 187.445–192.101 |
| native-eager | three_table_join_range | 1414.521 | 1443.897 | 0.980× | 1407.889–1421.153 | 1440.456–1447.337 |
| native-eager | three_table_computed_key | 248.501 | 232.353 | 1.069× | 242.496–254.506 | 230.081–234.626 |
| native-eager | four_table_join | 327.789 | 300.907 | 1.089× | 322.008–333.571 | 300.243–301.572 |
| native-eager | three_table_left_fence | 701.693 | 710.765 | 0.987× | 696.632–706.754 | 703.364–718.166 |

## main: 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.468 | 5.440 | 1.005× | 5.450–5.486 | 5.273–5.608 |
| native-prepared | pk_hit | 3.939 | 4.134 | 0.953× | 3.882–3.996 | 4.063–4.204 |
| native-prepared | indexed_join | 149.232 | 140.866 | 1.059× | 147.531–150.933 | 139.125–142.607 |
| native-prepared | three_table_join | 227.452 | 286.436 | 0.794× | 224.545–230.360 | 269.358–303.514 |
| native-prepared | three_table_join_miss | 191.013 | 181.549 | 1.052× | 188.879–193.148 | 179.235–183.863 |
| native-prepared | three_table_join_range | 1552.537 | 1569.022 | 0.989× | 1548.089–1556.986 | 1550.428–1587.616 |
| native-prepared | three_table_computed_key | 262.536 | 274.168 | 0.958× | 262.463–262.608 | 256.142–292.194 |
| native-prepared | four_table_join | 335.355 | 319.959 | 1.048× | 330.644–340.065 | 317.129–322.788 |
| native-prepared | three_table_left_fence | 650.191 | 661.001 | 0.984× | 645.666–654.715 | 634.783–687.218 |
| native-query | scalar | 5.935 | 5.940 | 0.999× | 5.876–5.994 | 5.710–6.171 |
| native-query | pk_hit | 4.464 | 4.471 | 0.999× | 4.348–4.581 | 4.400–4.541 |
| native-query | indexed_join | 159.368 | 141.701 | 1.125× | 155.223–163.514 | 140.825–142.576 |
| native-query | three_table_join | 239.726 | 240.042 | 0.999× | 238.682–240.770 | 212.524–267.560 |
| native-query | three_table_join_miss | 205.870 | 183.584 | 1.121× | 205.805–205.936 | 178.692–188.477 |
| native-query | three_table_join_range | 1604.163 | 1582.798 | 1.013× | 1584.672–1623.654 | 1576.879–1588.716 |
| native-query | three_table_computed_key | 259.071 | 245.682 | 1.054× | 258.164–259.979 | 230.989–260.376 |
| native-query | four_table_join | 351.484 | 318.011 | 1.105× | 346.464–356.504 | 298.223–337.798 |
| native-query | three_table_left_fence | 678.535 | 677.098 | 1.002× | 667.477–689.592 | 664.445–689.751 |
| native-eager | scalar | 5.717 | 5.700 | 1.003× | 5.704–5.729 | 5.524–5.876 |
| native-eager | pk_hit | 3.982 | 3.893 | 1.023× | 3.961–4.003 | 3.818–3.968 |
| native-eager | indexed_join | 147.210 | 133.737 | 1.101× | 143.752–150.668 | 132.029–135.446 |
| native-eager | three_table_join | 229.934 | 205.340 | 1.120× | 229.202–230.666 | 204.867–205.812 |
| native-eager | three_table_join_miss | 191.078 | 179.798 | 1.063× | 190.879–191.278 | 171.237–188.360 |
| native-eager | three_table_join_range | 1504.330 | 1469.795 | 1.023× | 1495.112–1513.547 | 1423.766–1515.825 |
| native-eager | three_table_computed_key | 245.762 | 230.715 | 1.065× | 243.714–247.811 | 220.096–241.334 |
| native-eager | four_table_join | 331.417 | 304.489 | 1.088× | 328.432–334.402 | 287.005–321.974 |
| native-eager | three_table_left_fence | 643.071 | 634.217 | 1.014× | 639.780–646.362 | 621.862–646.573 |

## supplemental: 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | positive_window | 150.739 | 151.508 | 0.995× | 149.697–151.782 | 149.713–153.304 |
| native-prepared | ordinary_aggregate | 228.709 | 230.937 | 0.990× | 228.470–228.948 | 229.904–231.970 |
| native-prepared | oversized_syntax | 50.807 | 49.456 | 1.027× | 50.042–51.572 | 48.387–50.525 |
| native-query | positive_window | 149.509 | 154.557 | 0.967× | 148.798–150.219 | 154.339–154.776 |
| native-query | ordinary_aggregate | 227.617 | 234.203 | 0.972× | 227.094–228.139 | 234.099–234.307 |
| native-query | oversized_syntax | 49.688 | 50.717 | 0.980× | 47.800–51.576 | 50.165–51.269 |
| native-eager | positive_window | 132.665 | 130.841 | 1.014× | 131.789–133.541 | 129.947–131.735 |
| native-eager | ordinary_aggregate | 204.926 | 202.300 | 1.013× | 202.204–207.648 | 200.106–204.494 |
| native-eager | oversized_syntax | 20.643 | 21.552 | 0.958× | 20.172–21.114 | 21.500–21.605 |

## fresh: 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-query | fresh_sql | 153.674 | 158.312 | 0.971× | 151.596–155.752 | 152.769–163.855 |
| native-query | fresh_join | 1226.680 | 1251.055 | 0.981× | 1201.475–1251.884 | 1241.172–1260.938 |
| native-eager | fresh_sql | 122.537 | 116.511 | 1.052× | 122.485–122.589 | 115.519–117.504 |
| native-eager | fresh_join | 1242.088 | 1269.455 | 0.978× | 1208.687–1275.489 | 1213.929–1324.981 |

## Slower observations

| Group | Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---|---:|---|
| main | 4.0-none | native-prepared | pk_hit | +0.268% | no |
| main | 4.0-none | native-prepared | three_table_left_fence | +1.843% | yes |
| main | 4.0-none | native-query | pk_hit | +0.469% | no |
| main | 4.0-none | native-query | three_table_join_range | +0.413% | no |
| main | 4.0-none | native-query | three_table_left_fence | +2.144% | no |
| main | 4.0-none | native-eager | scalar | +0.286% | no |
| main | 4.0-none | native-eager | pk_hit | +0.717% | no |
| main | 4.0-none | native-eager | three_table_join_range | +0.679% | no |
| main | 4.0-none | native-eager | three_table_left_fence | +0.584% | no |
| main | 4.0-yjit | native-prepared | scalar | +5.188% | yes |
| main | 4.0-yjit | native-prepared | pk_hit | +4.224% | yes |
| main | 4.0-yjit | native-prepared | three_table_join | +0.029% | no |
| main | 4.0-yjit | native-prepared | three_table_join_range | +3.879% | yes |
| main | 4.0-yjit | native-prepared | three_table_left_fence | +5.059% | yes |
| main | 4.0-yjit | native-query | scalar | +0.090% | no |
| main | 4.0-yjit | native-query | pk_hit | +1.312% | no |
| main | 4.0-yjit | native-query | three_table_join_range | +0.880% | no |
| main | 4.0-yjit | native-query | three_table_left_fence | +3.918% | no |
| main | 4.0-yjit | native-eager | pk_hit | +0.567% | no |
| main | 4.0-yjit | native-eager | three_table_join_range | +2.077% | yes |
| main | 4.0-yjit | native-eager | three_table_left_fence | +1.293% | no |
| main | 4.1-zjit | native-prepared | pk_hit | +4.946% | yes |
| main | 4.1-zjit | native-prepared | three_table_join | +25.932% | yes |
| main | 4.1-zjit | native-prepared | three_table_join_range | +1.062% | no |
| main | 4.1-zjit | native-prepared | three_table_computed_key | +4.431% | no |
| main | 4.1-zjit | native-prepared | three_table_left_fence | +1.663% | no |
| main | 4.1-zjit | native-query | scalar | +0.089% | no |
| main | 4.1-zjit | native-query | pk_hit | +0.142% | no |
| main | 4.1-zjit | native-query | three_table_join | +0.132% | no |
| supplemental | 4.0-yjit | native-prepared | positive_window | +0.510% | no |
| supplemental | 4.0-yjit | native-prepared | ordinary_aggregate | +0.974% | yes |
| supplemental | 4.0-yjit | native-query | positive_window | +3.377% | yes |
| supplemental | 4.0-yjit | native-query | ordinary_aggregate | +2.893% | yes |
| supplemental | 4.0-yjit | native-query | oversized_syntax | +2.071% | no |
| supplemental | 4.0-yjit | native-eager | oversized_syntax | +4.406% | yes |
| fresh | 4.0-yjit | native-query | fresh_sql | +3.018% | no |
| fresh | 4.0-yjit | native-query | fresh_join | +1.987% | no |
| fresh | 4.0-yjit | native-eager | fresh_join | +2.203% | no |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.614 | 0.803 | 1.419 | 2.114 | 5.747× | 3.250× |
| pk_hit | 3.879 | 1.576 | 2.217 | 3.820 | 2.462× | 1.749× |
| indexed_join | 148.104 | 52.670 | 12.931 | 120.463 | 2.812× | 11.453× |
| three_table_join | 219.555 | 3.475 | 4.286 | 12.509 | 63.178× | 51.225× |
| three_table_join_miss | 194.247 | 1.080 | 2.099 | 3.715 | 179.775× | 92.558× |
| three_table_join_range | 1418.973 | 82.869 | 57.157 | 215.282 | 17.123× | 24.826× |
| three_table_computed_key | 249.239 | 6.613 | 5.803 | 15.638 | 37.692× | 42.952× |
| four_table_join | 308.427 | 4.124 | 3.861 | 11.197 | 74.795× | 79.876× |
| three_table_left_fence | 682.050 | 3.296 | 3.096 | 11.092 | 206.904× | 220.307× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.001 | 4.933 | 5.595 | 5.371 | 0.617× |
| pk_hit | 10.507 | 3.965 | 4.683 | 3.934 | 0.377× |
| indexed_join | 37.797 | 151.263 | 158.251 | 140.217 | 4.002× |
| three_table_join | 137.304 | 226.540 | 230.692 | 209.581 | 1.650× |
| three_table_join_miss | 169.620 | 198.916 | 200.284 | 187.893 | 1.173× |
| three_table_join_range | 231.378 | 1438.281 | 1498.916 | 1437.341 | 6.216× |
| three_table_computed_key | 125.265 | 245.463 | 259.411 | 234.712 | 1.960× |
| four_table_join | 422.647 | 318.612 | 328.843 | 302.514 | 0.754× |
| three_table_left_fence | 49.748 | 718.243 | 746.093 | 693.448 | 14.437× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 844.500 | 648.500 | -23.209% |
| three_table_join | 1297.000 | 999.000 | -22.976% |
| three_table_join_miss | 1074.000 | 776.000 | -27.747% |
| three_table_join_range | 14259.600 | 13961.700 | -2.089% |
| three_table_computed_key | 1309.000 | 993.000 | -24.141% |
| four_table_join | 1708.000 | 1304.000 | -23.653% |
| three_table_left_fence | 6485.900 | 6265.900 | -3.392% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 0.000 | 0.000 |
| scalar | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 0.000 | 0.000 |
| scalar | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 0.000 | 0.000 |
| scalar | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 0.000 | 0.000 |
| scalar | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 0.000 | 0.000 |
| scalar | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 0.000 |
| pk_hit | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 1.000 | 1.000 |
| pk_hit | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 0.000 | 0.000 |
| pk_hit | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 0.000 | 0.000 |
| pk_hit | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 0.000 | 0.000 |
| pk_hit | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 0.000 |
| indexed_join | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 108.000 | 20.000 |
| indexed_join | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| indexed_join | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 91.000 | 20.000 |
| indexed_join | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 134.000 | 27.000 |
| indexed_join | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 4.000 | 0.000 |
| indexed_join | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 4.000 |
| three_table_join | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 178.000 | 34.000 |
| three_table_join | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| three_table_join | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 137.000 | 29.000 |
| three_table_join | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 191.000 | 37.000 |
| three_table_join | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 6.000 | 0.000 |
| three_table_join | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 6.000 |
| three_table_join_miss | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 178.000 | 34.000 |
| three_table_join_miss | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| three_table_join_miss | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 137.000 | 29.000 |
| three_table_join_miss | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 191.000 | 37.000 |
| three_table_join_miss | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 6.000 | 0.000 |
| three_table_join_miss | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 6.000 |
| three_table_join_range | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 178.000 | 34.000 |
| three_table_join_range | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| three_table_join_range | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 137.000 | 29.000 |
| three_table_join_range | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 191.000 | 37.000 |
| three_table_join_range | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 6.000 | 0.000 |
| three_table_join_range | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 6.000 |
| three_table_computed_key | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 206.000 | 38.000 |
| three_table_computed_key | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| three_table_computed_key | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 151.000 | 31.000 |
| three_table_computed_key | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 205.000 | 39.000 |
| three_table_computed_key | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 6.000 | 0.000 |
| three_table_computed_key | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 6.000 |
| four_table_join | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 252.000 | 48.000 |
| four_table_join | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| four_table_join | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 185.000 | 38.000 |
| four_table_join | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 256.000 | 48.000 |
| four_table_join | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 8.000 | 0.000 |
| four_table_join | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 8.000 |
| three_table_left_fence | JedNative::AggregateExecution#local_aggregate? → JedNative::QueryWalks#local_aggregate? | 182.000 | 34.000 |
| three_table_left_fence | JedNative::Engine#contains_aggregate? → JedNative::QueryWalks#contains_aggregate? | 4.000 | 4.000 |
| three_table_left_fence | JedNative::WindowExecution#window_nodes → JedNative::QueryWalks#window_nodes | 139.000 | 29.000 |
| three_table_left_fence | JedNative::OpaqueDefinitions#validate_opaque_query_tree → JedNative::QueryWalks#validate_opaque_query_tree | 193.000 | 37.000 |
| three_table_left_fence | JedNative::QueryColumns#query_source_columns → JedNative::QueryColumns#query_source_columns | 6.000 | 6.000 |
| three_table_left_fence | JedNative::PreparedJoins#query_source_columns → JedNative::PreparedJoins#query_source_columns | 0.000 | 6.000 |

## Adaptive ZJIT prepared three-table follow-up

These separate runs retain the original 94 paired cells above. Only the target's warmup changes in the long condition.

| Condition | Before | Final | Speedup | Elapsed change |
|---|---:|---:|---:|---:|
| primary warmup 100 | 227.452 | 286.436 | 0.794× | +25.932% |
| adaptive short | 236.788 | 208.019 | 1.138× | -12.150% |
| adaptive long | 229.322 | 213.889 | 1.072× | -6.729% |
