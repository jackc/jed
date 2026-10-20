# Work-estimation measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.595 | 12.490 | 1.008× | 12.460–12.729 | 12.271–12.709 |
| native-prepared | pk_hit | 8.571 | 8.229 | 1.041× | 8.517–8.624 | 8.086–8.373 |
| native-prepared | indexed_join | 269.083 | 261.448 | 1.029× | 268.046–270.119 | 256.487–266.409 |
| native-prepared | three_table_join | 376.242 | 368.623 | 1.021× | 375.226–377.257 | 363.538–373.708 |
| native-prepared | three_table_join_miss | 310.333 | 303.524 | 1.022× | 309.733–310.933 | 299.623–307.424 |
| native-prepared | three_table_join_range | 2928.385 | 2816.162 | 1.040× | 2925.253–2931.517 | 2775.168–2857.156 |
| native-prepared | three_table_computed_key | 411.822 | 406.956 | 1.012× | 410.150–413.494 | 399.965–413.946 |
| native-prepared | four_table_join | 514.699 | 509.007 | 1.011× | 513.070–516.327 | 504.483–513.530 |
| native-prepared | three_table_left_fence | 762.104 | 742.353 | 1.027× | 762.073–762.134 | 737.604–747.103 |
| native-query | scalar | 14.029 | 13.648 | 1.028× | 14.004–14.054 | 13.468–13.827 |
| native-query | pk_hit | 9.919 | 9.849 | 1.007× | 9.877–9.961 | 9.622–10.077 |
| native-query | indexed_join | 264.122 | 261.545 | 1.010× | 260.811–267.433 | 256.162–266.928 |
| native-query | three_table_join | 374.079 | 370.555 | 1.010× | 369.501–378.658 | 361.350–379.760 |
| native-query | three_table_join_miss | 305.432 | 300.687 | 1.016× | 304.990–305.874 | 295.041–306.333 |
| native-query | three_table_join_range | 2929.765 | 2830.973 | 1.035× | 2929.163–2930.368 | 2789.930–2872.015 |
| native-query | three_table_computed_key | 409.377 | 406.851 | 1.006× | 404.763–413.992 | 402.157–411.545 |
| native-query | four_table_join | 509.078 | 502.237 | 1.014× | 505.388–512.769 | 486.169–518.305 |
| native-query | three_table_left_fence | 764.602 | 732.836 | 1.043× | 764.600–764.603 | 714.513–751.159 |
| native-eager | scalar | 15.686 | 15.227 | 1.030× | 15.377–15.994 | 15.127–15.327 |
| native-eager | pk_hit | 9.596 | 9.534 | 1.006× | 9.405–9.788 | 9.472–9.596 |
| native-eager | indexed_join | 228.766 | 223.085 | 1.025× | 223.433–234.099 | 222.428–223.742 |
| native-eager | three_table_join | 337.857 | 330.394 | 1.023× | 331.938–343.775 | 328.679–332.109 |
| native-eager | three_table_join_miss | 279.707 | 275.134 | 1.017× | 274.370–285.044 | 273.375–276.893 |
| native-eager | three_table_join_range | 2752.964 | 2703.152 | 1.018× | 2693.679–2812.250 | 2701.622–2704.681 |
| native-eager | three_table_computed_key | 366.584 | 358.194 | 1.023× | 361.199–371.968 | 358.010–358.379 |
| native-eager | four_table_join | 473.236 | 463.477 | 1.021× | 461.110–485.361 | 459.767–467.187 |
| native-eager | three_table_left_fence | 706.877 | 672.635 | 1.051× | 694.608–719.146 | 667.502–677.767 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.962 | 4.981 | 0.996× | 4.852–5.072 | 4.848–5.113 |
| native-prepared | pk_hit | 4.058 | 4.136 | 0.981× | 4.042–4.073 | 4.008–4.263 |
| native-prepared | indexed_join | 153.313 | 153.524 | 0.999× | 151.334–155.292 | 149.831–157.217 |
| native-prepared | three_table_join | 219.838 | 227.312 | 0.967× | 218.172–221.504 | 220.032–234.592 |
| native-prepared | three_table_join_miss | 198.119 | 200.519 | 0.988× | 194.295–201.944 | 191.245–209.792 |
| native-prepared | three_table_join_range | 1387.001 | 1384.586 | 1.002× | 1371.304–1402.699 | 1342.347–1426.824 |
| native-prepared | three_table_computed_key | 246.936 | 251.339 | 0.982× | 242.081–251.791 | 243.227–259.450 |
| native-prepared | four_table_join | 315.803 | 326.849 | 0.966× | 312.236–319.370 | 322.012–331.686 |
| native-prepared | three_table_left_fence | 445.700 | 445.401 | 1.001× | 437.174–454.227 | 434.799–456.002 |
| native-query | scalar | 5.536 | 5.570 | 0.994× | 5.509–5.562 | 5.539–5.601 |
| native-query | pk_hit | 4.674 | 4.677 | 0.999× | 4.661–4.687 | 4.653–4.701 |
| native-query | indexed_join | 159.059 | 158.616 | 1.003× | 158.422–159.697 | 157.945–159.286 |
| native-query | three_table_join | 233.740 | 231.512 | 1.010× | 232.513–234.968 | 231.402–231.622 |
| native-query | three_table_join_miss | 200.618 | 202.989 | 0.988× | 199.679–201.556 | 202.139–203.840 |
| native-query | three_table_join_range | 1418.659 | 1405.073 | 1.010× | 1405.991–1431.328 | 1396.398–1413.748 |
| native-query | three_table_computed_key | 257.334 | 256.301 | 1.004× | 253.980–260.687 | 254.875–257.727 |
| native-query | four_table_join | 324.295 | 325.414 | 0.997× | 319.239–329.351 | 323.488–327.339 |
| native-query | three_table_left_fence | 455.972 | 450.821 | 1.011× | 452.707–459.236 | 447.553–454.090 |
| native-eager | scalar | 5.733 | 5.707 | 1.005× | 5.726–5.740 | 5.693–5.721 |
| native-eager | pk_hit | 4.199 | 4.220 | 0.995× | 4.109–4.290 | 4.187–4.253 |
| native-eager | indexed_join | 146.080 | 146.504 | 0.997× | 144.628–147.531 | 146.331–146.677 |
| native-eager | three_table_join | 221.220 | 220.933 | 1.001× | 217.960–224.479 | 219.865–222.001 |
| native-eager | three_table_join_miss | 199.579 | 193.267 | 1.033× | 199.574–199.584 | 191.760–194.773 |
| native-eager | three_table_join_range | 1409.977 | 1411.289 | 0.999× | 1403.922–1416.032 | 1402.586–1419.991 |
| native-eager | three_table_computed_key | 242.327 | 239.768 | 1.011× | 240.550–244.104 | 239.430–240.106 |
| native-eager | four_table_join | 313.707 | 314.971 | 0.996× | 312.664–314.751 | 312.141–317.800 |
| native-eager | three_table_left_fence | 443.577 | 439.178 | 1.010× | 441.454–445.699 | 437.762–440.593 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.533 | 5.593 | 0.989× | 5.498–5.568 | 5.558–5.629 |
| native-prepared | pk_hit | 4.076 | 4.123 | 0.989× | 4.049–4.103 | 4.001–4.245 |
| native-prepared | indexed_join | 142.631 | 142.084 | 1.004× | 139.831–145.431 | 140.104–144.064 |
| native-prepared | three_table_join | 217.654 | 224.778 | 0.968× | 210.361–224.946 | 215.387–234.169 |
| native-prepared | three_table_join_miss | 193.153 | 191.320 | 1.010× | 190.861–195.444 | 189.039–193.601 |
| native-prepared | three_table_join_range | 1621.549 | 1624.761 | 0.998× | 1589.287–1653.812 | 1602.073–1647.449 |
| native-prepared | three_table_computed_key | 251.731 | 303.222 | 0.830× | 245.906–257.557 | 262.061–344.384 |
| native-prepared | four_table_join | 358.095 | 324.037 | 1.105× | 333.739–382.451 | 318.333–329.741 |
| native-prepared | three_table_left_fence | 460.181 | 464.548 | 0.991× | 453.261–467.101 | 463.302–465.794 |
| native-query | scalar | 5.754 | 5.809 | 0.991× | 5.751–5.758 | 5.673–5.945 |
| native-query | pk_hit | 4.276 | 4.421 | 0.967× | 4.249–4.304 | 4.305–4.538 |
| native-query | indexed_join | 136.366 | 144.836 | 0.942× | 136.068–136.664 | 138.302–151.370 |
| native-query | three_table_join | 237.100 | 229.737 | 1.032× | 222.460–251.741 | 205.544–253.930 |
| native-query | three_table_join_miss | 188.657 | 194.206 | 0.971× | 187.356–189.959 | 189.394–199.018 |
| native-query | three_table_join_range | 1586.040 | 1586.539 | 1.000× | 1542.150–1629.930 | 1534.567–1638.512 |
| native-query | three_table_computed_key | 255.250 | 253.858 | 1.005× | 254.582–255.917 | 253.291–254.425 |
| native-query | four_table_join | 323.426 | 330.633 | 0.978× | 320.621–326.230 | 325.408–335.858 |
| native-query | three_table_left_fence | 454.845 | 447.069 | 1.017× | 442.225–467.466 | 436.942–457.196 |
| native-eager | scalar | 5.501 | 5.634 | 0.976× | 5.311–5.691 | 5.508–5.761 |
| native-eager | pk_hit | 3.732 | 3.823 | 0.976× | 3.709–3.755 | 3.738–3.909 |
| native-eager | indexed_join | 132.656 | 131.317 | 1.010× | 130.603–134.710 | 129.273–133.362 |
| native-eager | three_table_join | 227.507 | 217.053 | 1.048× | 206.339–248.675 | 213.791–220.316 |
| native-eager | three_table_join_miss | 179.767 | 172.676 | 1.041× | 169.639–189.895 | 169.333–176.019 |
| native-eager | three_table_join_range | 1510.986 | 1478.252 | 1.022× | 1455.346–1566.625 | 1478.229–1478.274 |
| native-eager | three_table_computed_key | 233.689 | 228.222 | 1.024× | 229.796–237.583 | 227.410–229.036 |
| native-eager | four_table_join | 295.590 | 289.346 | 1.022× | 286.619–304.561 | 279.546–299.145 |
| native-eager | three_table_left_fence | 420.092 | 437.187 | 0.961× | 419.806–420.378 | 426.998–447.377 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-yjit | native-prepared | scalar | +0.379% | no |
| 4.0-yjit | native-prepared | pk_hit | +1.922% | no |
| 4.0-yjit | native-prepared | indexed_join | +0.137% | no |
| 4.0-yjit | native-prepared | three_table_join | +3.400% | no |
| 4.0-yjit | native-prepared | three_table_join_miss | +1.211% | no |
| 4.0-yjit | native-prepared | three_table_computed_key | +1.783% | no |
| 4.0-yjit | native-prepared | four_table_join | +3.498% | yes |
| 4.0-yjit | native-query | scalar | +0.614% | no |
| 4.0-yjit | native-query | pk_hit | +0.068% | no |
| 4.0-yjit | native-query | three_table_join_miss | +1.182% | yes |
| 4.0-yjit | native-query | four_table_join | +0.345% | no |
| 4.0-yjit | native-eager | pk_hit | +0.490% | no |
| 4.0-yjit | native-eager | indexed_join | +0.290% | no |
| 4.0-yjit | native-eager | three_table_join_range | +0.093% | no |
| 4.0-yjit | native-eager | four_table_join | +0.403% | no |
| 4.1-zjit | native-prepared | scalar | +1.096% | no |
| 4.1-zjit | native-prepared | pk_hit | +1.152% | no |
| 4.1-zjit | native-prepared | three_table_join | +3.273% | no |
| 4.1-zjit | native-prepared | three_table_join_range | +0.198% | no |
| 4.1-zjit | native-prepared | three_table_computed_key | +20.455% | yes |
| 4.1-zjit | native-prepared | three_table_left_fence | +0.949% | no |
| 4.1-zjit | native-query | scalar | +0.948% | no |
| 4.1-zjit | native-query | pk_hit | +3.390% | yes |
| 4.1-zjit | native-query | indexed_join | +6.211% | yes |
| 4.1-zjit | native-query | three_table_join_miss | +2.941% | no |
| 4.1-zjit | native-query | three_table_join_range | +0.031% | no |
| 4.1-zjit | native-query | four_table_join | +2.229% | no |
| 4.1-zjit | native-eager | scalar | +2.417% | no |
| 4.1-zjit | native-eager | pk_hit | +2.446% | no |
| 4.1-zjit | native-eager | three_table_left_fence | +4.069% | yes |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.648 | 0.820 | 1.410 | 2.186 | 5.665× | 3.298× |
| pk_hit | 3.921 | 1.648 | 2.184 | 3.732 | 2.379× | 1.795× |
| indexed_join | 162.562 | 55.497 | 2.499 | 122.176 | 2.929× | 65.057× |
| three_table_join | 225.112 | 3.180 | 3.095 | 12.278 | 70.799× | 72.727× |
| three_table_join_miss | 194.631 | 1.054 | 1.488 | 3.599 | 184.633× | 130.765× |
| three_table_join_range | 1297.010 | 76.548 | 59.008 | 229.805 | 16.944× | 21.980× |
| three_table_computed_key | 242.768 | 6.896 | 5.817 | 15.606 | 35.203× | 41.737× |
| four_table_join | 301.619 | 4.706 | 3.905 | 11.867 | 64.099× | 77.233× |
| three_table_left_fence | 418.531 | 3.206 | 3.116 | 10.638 | 130.561× | 134.328× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.048 | 4.915 | 5.360 | 6.450 | 0.611× |
| pk_hit | 10.651 | 4.197 | 4.582 | 4.026 | 0.394× |
| indexed_join | 38.243 | 157.831 | 154.954 | 139.379 | 4.127× |
| three_table_join | 142.636 | 235.215 | 219.599 | 211.006 | 1.649× |
| three_table_join_miss | 173.086 | 206.559 | 201.384 | 187.344 | 1.193× |
| three_table_join_range | 236.183 | 1428.283 | 1375.346 | 1352.008 | 6.047× |
| three_table_computed_key | 138.611 | 260.129 | 245.802 | 227.419 | 1.877× |
| four_table_join | 441.734 | 326.065 | 312.972 | 305.616 | 0.738× |
| three_table_left_fence | 51.567 | 440.392 | 439.120 | 420.771 | 8.540× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10558.300 | 10410.300 | -1.402% |
| three_table_computed_key | 887.000 | 887.000 | +0.000% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2136.800 | -2.731% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| scalar | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| scalar | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| scalar | JedNative::Engine#compile | 0.000 | 0.000 |
| pk_hit | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| pk_hit | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| pk_hit | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#compile | 0.000 | 0.000 |
| indexed_join | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| indexed_join | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| indexed_join | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| three_table_join | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| three_table_join | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join_miss | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| three_table_join_miss | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| three_table_join_miss | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join_range | JedNative::QueryPlans#expression_work | 231.000 | 1.000 |
| three_table_join_range | JedNative::AccessPlanning#expression_work | 231.000 | 1.000 |
| three_table_join_range | JedNative::DeepStack#expression_work | 231.000 | 231.000 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_computed_key | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| three_table_computed_key | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| three_table_computed_key | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_computed_key | JedNative::Engine#compile | 0.000 | 0.000 |
| four_table_join | JedNative::QueryPlans#expression_work | 0.000 | 0.000 |
| four_table_join | JedNative::AccessPlanning#expression_work | 0.000 | 0.000 |
| four_table_join | JedNative::DeepStack#expression_work | 0.000 | 0.000 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_left_fence | JedNative::QueryPlans#expression_work | 93.000 | 0.000 |
| three_table_left_fence | JedNative::AccessPlanning#expression_work | 93.000 | 0.000 |
| three_table_left_fence | JedNative::DeepStack#expression_work | 93.000 | 93.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::Engine#compile | 0.000 | 0.000 |
