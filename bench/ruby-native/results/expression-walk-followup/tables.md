# Expression-walk measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.518 | 12.120 | 1.033× | 12.033–13.002 | 12.066–12.174 |
| native-prepared | pk_hit | 8.300 | 8.259 | 1.005× | 8.089–8.512 | 8.225–8.294 |
| native-prepared | indexed_join | 262.573 | 253.300 | 1.037× | 260.546–264.601 | 253.290–253.310 |
| native-prepared | three_table_join | 366.803 | 364.443 | 1.006× | 361.558–372.048 | 359.053–369.832 |
| native-prepared | three_table_join_miss | 303.589 | 298.775 | 1.016× | 295.302–311.876 | 297.055–300.494 |
| native-prepared | three_table_join_range | 2847.649 | 2780.844 | 1.024× | 2791.066–2904.232 | 2779.277–2782.412 |
| native-prepared | three_table_computed_key | 399.276 | 400.938 | 0.996× | 392.107–406.445 | 398.729–403.148 |
| native-prepared | four_table_join | 498.800 | 491.034 | 1.016× | 486.808–510.791 | 490.309–491.759 |
| native-prepared | three_table_left_fence | 1323.777 | 1304.314 | 1.015× | 1284.444–1363.110 | 1294.093–1314.536 |
| native-query | scalar | 13.634 | 13.799 | 0.988× | 13.477–13.791 | 13.799–13.800 |
| native-query | pk_hit | 9.697 | 9.671 | 1.003× | 9.695–9.699 | 9.550–9.791 |
| native-query | indexed_join | 255.977 | 258.346 | 0.991× | 254.021–257.934 | 255.650–261.042 |
| native-query | three_table_join | 365.594 | 368.297 | 0.993× | 365.188–366.001 | 361.095–375.500 |
| native-query | three_table_join_miss | 296.862 | 294.254 | 1.009× | 294.533–299.191 | 292.619–295.890 |
| native-query | three_table_join_range | 2781.629 | 2818.482 | 0.987× | 2770.538–2792.720 | 2785.604–2851.360 |
| native-query | three_table_computed_key | 402.664 | 403.783 | 0.997× | 401.353–403.974 | 399.788–407.778 |
| native-query | four_table_join | 493.812 | 499.882 | 0.988× | 489.190–498.435 | 491.169–508.596 |
| native-query | three_table_left_fence | 1314.784 | 1321.429 | 0.995× | 1305.712–1323.856 | 1307.386–1335.472 |
| native-eager | scalar | 15.383 | 15.351 | 1.002× | 15.177–15.589 | 15.144–15.558 |
| native-eager | pk_hit | 9.315 | 9.274 | 1.004× | 9.299–9.332 | 9.260–9.288 |
| native-eager | indexed_join | 221.888 | 223.928 | 0.991× | 219.891–223.886 | 223.177–224.679 |
| native-eager | three_table_join | 326.750 | 335.143 | 0.975× | 326.495–327.006 | 334.813–335.473 |
| native-eager | three_table_join_miss | 273.048 | 280.052 | 0.975× | 271.677–274.418 | 278.366–281.737 |
| native-eager | three_table_join_range | 2682.471 | 2681.123 | 1.001× | 2667.505–2697.437 | 2666.046–2696.199 |
| native-eager | three_table_computed_key | 361.129 | 372.202 | 0.970× | 352.804–369.455 | 370.329–374.075 |
| native-eager | four_table_join | 457.017 | 473.626 | 0.965× | 456.506–457.527 | 464.849–482.403 |
| native-eager | three_table_left_fence | 1228.133 | 1289.446 | 0.952× | 1212.548–1243.718 | 1276.097–1302.794 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.982 | 5.014 | 0.994× | 4.903–5.060 | 4.875–5.152 |
| native-prepared | pk_hit | 3.992 | 4.109 | 0.971× | 3.897–4.086 | 3.991–4.228 |
| native-prepared | indexed_join | 155.123 | 151.595 | 1.023× | 151.989–158.257 | 146.679–156.511 |
| native-prepared | three_table_join | 225.019 | 222.068 | 1.013× | 222.229–227.808 | 215.414–228.722 |
| native-prepared | three_table_join_miss | 197.518 | 196.822 | 1.004× | 196.518–198.519 | 191.626–202.019 |
| native-prepared | three_table_join_range | 1478.798 | 1366.655 | 1.082× | 1458.058–1499.539 | 1336.441–1396.869 |
| native-prepared | three_table_computed_key | 252.684 | 249.434 | 1.013× | 247.798–257.570 | 242.737–256.132 |
| native-prepared | four_table_join | 319.617 | 315.520 | 1.013× | 310.808–328.426 | 306.849–324.192 |
| native-prepared | three_table_left_fence | 722.027 | 711.421 | 1.015× | 713.138–730.916 | 689.578–733.263 |
| native-query | scalar | 5.565 | 5.416 | 1.027× | 5.494–5.636 | 5.274–5.559 |
| native-query | pk_hit | 4.660 | 4.535 | 1.028× | 4.651–4.669 | 4.437–4.633 |
| native-query | indexed_join | 161.045 | 154.192 | 1.044× | 158.689–163.400 | 150.579–157.805 |
| native-query | three_table_join | 239.940 | 223.987 | 1.071× | 238.245–241.636 | 218.892–229.082 |
| native-query | three_table_join_miss | 206.912 | 194.726 | 1.063× | 206.616–207.208 | 190.392–199.060 |
| native-query | three_table_join_range | 1494.729 | 1369.017 | 1.092× | 1476.559–1512.898 | 1344.234–1393.801 |
| native-query | three_table_computed_key | 259.851 | 252.614 | 1.029× | 258.274–261.428 | 251.928–253.299 |
| native-query | four_table_join | 333.363 | 314.756 | 1.059× | 332.749–333.978 | 314.401–315.112 |
| native-query | three_table_left_fence | 743.296 | 701.593 | 1.059× | 739.520–747.073 | 695.900–707.286 |
| native-eager | scalar | 5.602 | 5.551 | 1.009× | 5.582–5.622 | 5.407–5.696 |
| native-eager | pk_hit | 4.121 | 4.064 | 1.014× | 4.108–4.134 | 3.996–4.132 |
| native-eager | indexed_join | 146.608 | 141.442 | 1.037× | 145.983–147.234 | 138.573–144.312 |
| native-eager | three_table_join | 223.655 | 211.952 | 1.055× | 219.113–228.198 | 206.661–217.242 |
| native-eager | three_table_join_miss | 193.999 | 189.156 | 1.026× | 191.839–196.159 | 186.534–191.779 |
| native-eager | three_table_join_range | 1493.220 | 1352.749 | 1.104× | 1478.086–1508.355 | 1335.001–1370.497 |
| native-eager | three_table_computed_key | 243.267 | 234.467 | 1.038× | 241.298–245.235 | 229.817–239.116 |
| native-eager | four_table_join | 317.285 | 305.223 | 1.040× | 316.964–317.606 | 302.461–307.986 |
| native-eager | three_table_left_fence | 736.979 | 715.908 | 1.029× | 729.883–744.076 | 713.898–717.919 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.672 | 5.410 | 1.049× | 5.668–5.676 | 5.274–5.545 |
| native-prepared | pk_hit | 4.156 | 3.961 | 1.049× | 4.084–4.228 | 3.906–4.015 |
| native-prepared | indexed_join | 141.227 | 139.876 | 1.010× | 140.822–141.632 | 138.224–141.528 |
| native-prepared | three_table_join | 246.921 | 242.514 | 1.018× | 220.049–273.793 | 219.353–265.676 |
| native-prepared | three_table_join_miss | 190.585 | 181.880 | 1.048× | 189.701–191.469 | 178.743–185.017 |
| native-prepared | three_table_join_range | 1584.839 | 1586.643 | 0.999× | 1584.118–1585.560 | 1533.145–1640.141 |
| native-prepared | three_table_computed_key | 261.048 | 248.380 | 1.051× | 244.558–277.539 | 244.496–252.263 |
| native-prepared | four_table_join | 328.979 | 311.204 | 1.057× | 327.761–330.197 | 308.565–313.842 |
| native-prepared | three_table_left_fence | 663.385 | 670.665 | 0.989× | 660.640–666.129 | 661.224–680.105 |
| native-query | scalar | 5.815 | 6.074 | 0.957× | 5.788–5.841 | 5.993–6.154 |
| native-query | pk_hit | 4.289 | 4.628 | 0.927× | 4.266–4.311 | 4.580–4.677 |
| native-query | indexed_join | 141.315 | 147.784 | 0.956× | 136.692–145.938 | 144.441–151.127 |
| native-query | three_table_join | 238.577 | 260.052 | 0.917× | 224.720–252.434 | 236.246–283.857 |
| native-query | three_table_join_miss | 195.248 | 192.775 | 1.013× | 189.750–200.746 | 187.561–197.988 |
| native-query | three_table_join_range | 1561.707 | 1610.067 | 0.970× | 1537.546–1585.868 | 1590.208–1629.926 |
| native-query | three_table_computed_key | 252.478 | 260.857 | 0.968× | 250.657–254.299 | 260.397–261.317 |
| native-query | four_table_join | 353.368 | 333.328 | 1.060× | 335.455–371.281 | 326.537–340.120 |
| native-query | three_table_left_fence | 668.384 | 691.434 | 0.967× | 662.424–674.343 | 683.218–699.651 |
| native-eager | scalar | 5.631 | 5.733 | 0.982× | 5.511–5.752 | 5.707–5.758 |
| native-eager | pk_hit | 3.851 | 4.005 | 0.962× | 3.747–3.954 | 3.966–4.044 |
| native-eager | indexed_join | 131.929 | 133.850 | 0.986× | 124.655–139.204 | 133.570–134.130 |
| native-eager | three_table_join | 203.813 | 214.748 | 0.949× | 194.273–213.353 | 213.891–215.605 |
| native-eager | three_table_join_miss | 175.591 | 183.181 | 0.959× | 173.231–177.951 | 176.351–190.011 |
| native-eager | three_table_join_range | 1467.219 | 1496.637 | 0.980× | 1460.083–1474.355 | 1495.840–1497.434 |
| native-eager | three_table_computed_key | 238.723 | 245.990 | 0.970× | 238.086–239.361 | 239.145–252.835 |
| native-eager | four_table_join | 306.028 | 315.498 | 0.970× | 303.648–308.408 | 308.336–322.660 |
| native-eager | three_table_left_fence | 665.820 | 647.363 | 1.029× | 658.027–673.613 | 629.330–665.397 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | three_table_computed_key | +0.416% | no |
| 4.0-none | native-query | scalar | +1.214% | yes |
| 4.0-none | native-query | indexed_join | +0.925% | no |
| 4.0-none | native-query | three_table_join | +0.739% | no |
| 4.0-none | native-query | three_table_join_range | +1.325% | no |
| 4.0-none | native-query | three_table_computed_key | +0.278% | no |
| 4.0-none | native-query | four_table_join | +1.229% | no |
| 4.0-none | native-query | three_table_left_fence | +0.505% | no |
| 4.0-none | native-eager | indexed_join | +0.919% | no |
| 4.0-none | native-eager | three_table_join | +2.569% | yes |
| 4.0-none | native-eager | three_table_join_miss | +2.565% | yes |
| 4.0-none | native-eager | three_table_computed_key | +3.066% | yes |
| 4.0-none | native-eager | four_table_join | +3.634% | yes |
| 4.0-none | native-eager | three_table_left_fence | +4.992% | yes |
| 4.0-yjit | native-prepared | scalar | +0.644% | no |
| 4.0-yjit | native-prepared | pk_hit | +2.950% | no |
| 4.1-zjit | native-prepared | three_table_join_range | +0.114% | no |
| 4.1-zjit | native-prepared | three_table_left_fence | +1.097% | no |
| 4.1-zjit | native-query | scalar | +4.460% | yes |
| 4.1-zjit | native-query | pk_hit | +7.914% | yes |
| 4.1-zjit | native-query | indexed_join | +4.578% | no |
| 4.1-zjit | native-query | three_table_join | +9.001% | no |
| 4.1-zjit | native-query | three_table_join_range | +3.097% | yes |
| 4.1-zjit | native-query | three_table_computed_key | +3.319% | yes |
| 4.1-zjit | native-query | three_table_left_fence | +3.449% | yes |
| 4.1-zjit | native-eager | scalar | +1.800% | no |
| 4.1-zjit | native-eager | pk_hit | +3.999% | yes |
| 4.1-zjit | native-eager | indexed_join | +1.456% | no |
| 4.1-zjit | native-eager | three_table_join | +5.365% | yes |
| 4.1-zjit | native-eager | three_table_join_miss | +4.322% | no |
| 4.1-zjit | native-eager | three_table_join_range | +2.005% | yes |
| 4.1-zjit | native-eager | three_table_computed_key | +3.044% | no |
| 4.1-zjit | native-eager | four_table_join | +3.095% | no |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.814 | 0.782 | 1.416 | 2.066 | 6.160× | 3.401× |
| pk_hit | 3.782 | 1.661 | 2.222 | 3.650 | 2.277× | 1.702× |
| indexed_join | 166.859 | 52.753 | 3.642 | 118.754 | 3.163× | 45.812× |
| three_table_join | 220.259 | 3.529 | 4.310 | 12.272 | 62.411× | 51.101× |
| three_table_join_miss | 187.247 | 1.395 | 2.092 | 3.603 | 134.237× | 89.508× |
| three_table_join_range | 1302.341 | 81.696 | 59.793 | 210.680 | 15.941× | 21.781× |
| three_table_computed_key | 237.508 | 6.125 | 6.179 | 14.545 | 38.775× | 38.439× |
| four_table_join | 305.887 | 4.806 | 3.994 | 11.178 | 63.642× | 76.587× |
| three_table_left_fence | 685.953 | 3.437 | 3.232 | 11.285 | 199.602× | 212.271× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.160 | 4.958 | 5.362 | 5.472 | 0.608× |
| pk_hit | 10.683 | 4.013 | 4.476 | 3.878 | 0.376× |
| indexed_join | 39.051 | 152.123 | 149.912 | 135.432 | 3.895× |
| three_table_join | 138.005 | 226.730 | 223.886 | 205.149 | 1.643× |
| three_table_join_miss | 170.624 | 193.680 | 195.076 | 184.160 | 1.135× |
| three_table_join_range | 236.229 | 1344.207 | 1353.604 | 1331.152 | 5.690× |
| three_table_computed_key | 139.617 | 242.940 | 251.403 | 233.265 | 1.740× |
| four_table_join | 435.298 | 311.610 | 315.348 | 295.133 | 0.716× |
| three_table_left_fence | 51.243 | 701.382 | 706.258 | 686.457 | 13.687× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 648.500 | 572.500 | -11.719% |
| three_table_join | 999.000 | 853.000 | -14.615% |
| three_table_join_miss | 776.000 | 698.000 | -10.052% |
| three_table_join_range | 13961.700 | 10558.200 | -24.377% |
| three_table_computed_key | 993.000 | 887.000 | -10.675% |
| four_table_join | 1304.000 | 1095.000 | -16.028% |
| three_table_left_fence | 6265.900 | 5922.900 | -5.474% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 0.000 | 0.000 |
| scalar | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 0.000 | 0.000 |
| scalar | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 0.000 | 0.000 |
| pk_hit | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 0.000 | 0.000 |
| pk_hit | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 0.000 | 0.000 |
| pk_hit | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 0.000 | 0.000 |
| indexed_join | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 9.000 | 3.000 |
| indexed_join | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 8.000 | 8.000 |
| indexed_join | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 13.000 | 5.000 |
| three_table_join | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 15.000 | 5.000 |
| three_table_join | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 18.000 | 18.000 |
| three_table_join | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 19.000 | 7.000 |
| three_table_join_miss | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 15.000 | 5.000 |
| three_table_join_miss | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 2.000 | 2.000 |
| three_table_join_miss | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 7.000 | 3.000 |
| three_table_join_range | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 30.000 | 16.000 |
| three_table_join_range | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 354.000 | 354.000 |
| three_table_join_range | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 442.000 | 156.000 |
| three_table_computed_key | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 12.000 | 4.000 |
| three_table_computed_key | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 12.000 | 12.000 |
| three_table_computed_key | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 15.000 | 7.000 |
| four_table_join | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 21.000 | 7.000 |
| four_table_join | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 26.000 | 26.000 |
| four_table_join | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 27.000 | 9.000 |
| three_table_left_fence | JedNative::QueryPlans#expression_nodes → JedNative::ExpressionWalks#expression_nodes | 24.000 | 10.000 |
| three_table_left_fence | JedNative::IndexScans#same_key_expression? → JedNative::ExpressionWalks#same_key_expression? | 42.000 | 42.000 |
| three_table_left_fence | JedNative::IndexScans#conjunctions → JedNative::ExpressionWalks#conjunctions | 100.000 | 42.000 |

## Direct helper allocations (objects/invocation; no elapsed-time samples)

| Runtime | Helper | Before | Final | Final disabled control |
|---|---|---:|---:|---:|
| 4.0-none | nodes_tree | 68.000 | 1.000 | 68.000 |
| 4.0-none | nodes_nested_array | 150.000 | 1.000 | 150.000 |
| 4.0-none | conjunctions | 49.000 | 1.000 | 49.000 |
| 4.0-none | key_equal | 75.000 | 0.000 | 75.000 |
| 4.0-none | key_unequal | 74.000 | 0.000 | 74.000 |
| 4.0-none | key_typemod | 2.000 | 0.000 | 2.000 |
| 4.0-none | key_native_body_type | 0.000 | 0.000 | 0.000 |
| 4.0-none | key_nil_args | 4.000 | 0.000 | 4.000 |
| 4.0-yjit | nodes_tree | 68.000 | 1.000 | 68.000 |
| 4.0-yjit | nodes_nested_array | 150.000 | 1.000 | 150.000 |
| 4.0-yjit | conjunctions | 49.000 | 1.000 | 49.000 |
| 4.0-yjit | key_equal | 75.000 | 0.000 | 75.000 |
| 4.0-yjit | key_unequal | 74.000 | 0.000 | 74.000 |
| 4.0-yjit | key_typemod | 2.000 | 0.000 | 2.000 |
| 4.0-yjit | key_native_body_type | 0.000 | 0.000 | 0.000 |
| 4.0-yjit | key_nil_args | 4.000 | 0.000 | 4.000 |
| 4.1-zjit | nodes_tree | 68.000 | 1.000 | 68.000 |
| 4.1-zjit | nodes_nested_array | 150.000 | 1.000 | 150.000 |
| 4.1-zjit | conjunctions | 49.000 | 1.000 | 49.000 |
| 4.1-zjit | key_equal | 75.000 | 0.000 | 75.000 |
| 4.1-zjit | key_unequal | 74.000 | 0.000 | 74.000 |
| 4.1-zjit | key_typemod | 2.000 | 0.000 | 2.000 |
| 4.1-zjit | key_native_body_type | 0.000 | 0.000 | 0.000 |
| 4.1-zjit | key_nil_args | 4.000 | 0.000 | 4.000 |
