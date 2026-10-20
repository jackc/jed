# Join-predicate measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.345 | 12.318 | 1.002× | 12.010–12.680 | 11.983–12.653 |
| native-prepared | pk_hit | 8.202 | 8.265 | 0.992× | 7.884–8.520 | 7.981–8.548 |
| native-prepared | indexed_join | 261.723 | 260.737 | 1.004× | 253.042–270.405 | 254.678–266.797 |
| native-prepared | three_table_join | 372.236 | 369.829 | 1.007× | 361.584–382.887 | 357.335–382.322 |
| native-prepared | three_table_join_miss | 299.348 | 303.175 | 0.987× | 290.104–308.592 | 292.361–313.989 |
| native-prepared | three_table_join_range | 2867.410 | 2874.174 | 0.998× | 2800.806–2934.013 | 2821.278–2927.069 |
| native-prepared | three_table_computed_key | 414.677 | 422.523 | 0.981× | 405.590–423.764 | 417.617–427.429 |
| native-prepared | four_table_join | 502.197 | 504.463 | 0.996× | 489.451–514.942 | 497.161–511.765 |
| native-prepared | three_table_left_fence | 762.043 | 752.563 | 1.013× | 746.099–777.988 | 737.198–767.928 |
| native-prepared | join_residual | 5302.435 | 3903.480 | 1.358× | 5137.096–5467.774 | 3827.776–3979.183 |
| native-query | scalar | 13.943 | 13.719 | 1.016× | 13.813–14.073 | 13.527–13.910 |
| native-query | pk_hit | 9.971 | 9.648 | 1.033× | 9.927–10.015 | 9.383–9.913 |
| native-query | indexed_join | 265.342 | 262.694 | 1.010× | 264.795–265.888 | 257.800–267.588 |
| native-query | three_table_join | 376.801 | 368.680 | 1.022× | 375.413–378.190 | 360.362–376.999 |
| native-query | three_table_join_miss | 304.154 | 307.828 | 0.988× | 303.737–304.571 | 296.488–319.168 |
| native-query | three_table_join_range | 2938.813 | 2867.946 | 1.025× | 2938.664–2938.962 | 2821.277–2914.615 |
| native-query | three_table_computed_key | 409.170 | 423.125 | 0.967× | 408.048–410.293 | 411.514–434.735 |
| native-query | four_table_join | 513.338 | 497.668 | 1.031× | 508.875–517.800 | 487.726–507.611 |
| native-query | three_table_left_fence | 767.625 | 772.038 | 0.994× | 765.633–769.617 | 763.920–780.155 |
| native-query | join_residual | 5328.608 | 3961.080 | 1.345× | 5328.493–5328.722 | 3952.381–3969.778 |
| native-eager | scalar | 15.459 | 15.825 | 0.977× | 15.447–15.472 | 15.594–16.056 |
| native-eager | pk_hit | 9.523 | 9.964 | 0.956× | 9.462–9.584 | 9.828–10.100 |
| native-eager | indexed_join | 231.470 | 235.445 | 0.983× | 227.661–235.279 | 234.065–236.825 |
| native-eager | three_table_join | 335.315 | 349.646 | 0.959× | 333.774–336.856 | 341.506–357.787 |
| native-eager | three_table_join_miss | 277.432 | 288.920 | 0.960× | 277.338–277.524 | 287.138–290.702 |
| native-eager | three_table_join_range | 2745.153 | 2827.629 | 0.971× | 2729.515–2760.790 | 2813.983–2841.275 |
| native-eager | three_table_computed_key | 365.977 | 391.356 | 0.935× | 364.130–367.824 | 386.415–396.298 |
| native-eager | four_table_join | 466.444 | 481.690 | 0.968× | 464.290–468.598 | 479.191–484.188 |
| native-eager | three_table_left_fence | 702.293 | 726.323 | 0.967× | 696.866–707.721 | 724.378–728.267 |
| native-eager | join_residual | 4710.374 | 3581.569 | 1.315× | 4703.210–4717.538 | 3577.389–3585.748 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.072 | 4.993 | 1.016× | 5.033–5.111 | 4.967–5.018 |
| native-prepared | pk_hit | 4.098 | 4.105 | 0.998× | 4.096–4.100 | 4.087–4.124 |
| native-prepared | indexed_join | 154.697 | 154.341 | 1.002× | 152.621–156.773 | 154.010–154.673 |
| native-prepared | three_table_join | 227.370 | 225.322 | 1.009× | 227.277–227.462 | 220.764–229.880 |
| native-prepared | three_table_join_miss | 198.621 | 199.656 | 0.995× | 196.563–200.679 | 196.300–203.012 |
| native-prepared | three_table_join_range | 1396.880 | 1424.415 | 0.981× | 1390.893–1402.866 | 1403.792–1445.039 |
| native-prepared | three_table_computed_key | 254.169 | 268.158 | 0.948× | 252.422–255.916 | 266.484–269.832 |
| native-prepared | four_table_join | 323.304 | 322.975 | 1.001× | 321.354–325.255 | 321.925–324.025 |
| native-prepared | three_table_left_fence | 446.898 | 447.900 | 0.998× | 446.674–447.121 | 447.473–448.327 |
| native-prepared | join_residual | 2280.515 | 1910.134 | 1.194× | 2277.430–2283.601 | 1898.940–1921.329 |
| native-query | scalar | 5.387 | 5.639 | 0.955× | 5.329–5.444 | 5.598–5.680 |
| native-query | pk_hit | 4.516 | 4.628 | 0.976× | 4.501–4.531 | 4.611–4.645 |
| native-query | indexed_join | 151.910 | 156.203 | 0.973× | 150.101–153.719 | 154.424–157.982 |
| native-query | three_table_join | 222.110 | 226.268 | 0.982× | 221.555–222.665 | 225.195–227.341 |
| native-query | three_table_join_miss | 197.525 | 200.153 | 0.987× | 196.150–198.900 | 198.085–202.222 |
| native-query | three_table_join_range | 1358.381 | 1409.221 | 0.964× | 1346.693–1370.070 | 1408.497–1409.945 |
| native-query | three_table_computed_key | 244.666 | 266.049 | 0.920× | 244.442–244.891 | 264.705–267.393 |
| native-query | four_table_join | 316.418 | 330.698 | 0.957× | 310.627–322.210 | 325.977–335.418 |
| native-query | three_table_left_fence | 438.794 | 455.423 | 0.963× | 438.562–439.026 | 454.369–456.478 |
| native-query | join_residual | 2213.482 | 1907.356 | 1.160× | 2209.904–2217.059 | 1904.051–1910.661 |
| native-eager | scalar | 5.492 | 5.829 | 0.942× | 5.445–5.539 | 5.787–5.872 |
| native-eager | pk_hit | 3.996 | 4.114 | 0.971× | 3.982–4.009 | 4.084–4.144 |
| native-eager | indexed_join | 138.906 | 144.574 | 0.961× | 138.638–139.174 | 142.414–146.733 |
| native-eager | three_table_join | 209.398 | 218.398 | 0.959× | 207.213–211.583 | 217.873–218.923 |
| native-eager | three_table_join_miss | 185.846 | 194.724 | 0.954× | 184.793–186.899 | 192.458–196.991 |
| native-eager | three_table_join_range | 1356.259 | 1431.026 | 0.948× | 1349.464–1363.055 | 1418.783–1443.270 |
| native-eager | three_table_computed_key | 232.296 | 256.353 | 0.906× | 231.483–233.109 | 253.091–259.616 |
| native-eager | four_table_join | 300.297 | 319.830 | 0.939× | 300.131–300.463 | 313.999–325.660 |
| native-eager | three_table_left_fence | 429.279 | 449.599 | 0.955× | 427.383–431.174 | 444.687–454.512 |
| native-eager | join_residual | 2264.105 | 1993.974 | 1.135× | 2252.010–2276.199 | 1973.733–2014.215 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.634 | 5.691 | 0.990× | 5.624–5.645 | 5.680–5.702 |
| native-prepared | pk_hit | 3.998 | 4.084 | 0.979× | 3.990–4.006 | 3.980–4.187 |
| native-prepared | indexed_join | 143.529 | 137.954 | 1.040× | 140.541–146.517 | 134.633–141.275 |
| native-prepared | three_table_join | 215.503 | 286.530 | 0.752× | 213.354–217.652 | 282.269–290.791 |
| native-prepared | three_table_join_miss | 194.949 | 186.058 | 1.048× | 194.648–195.249 | 184.286–187.831 |
| native-prepared | three_table_join_range | 1627.938 | 1577.671 | 1.032× | 1612.840–1643.037 | 1550.826–1604.517 |
| native-prepared | three_table_computed_key | 267.253 | 257.940 | 1.036× | 260.609–273.896 | 244.467–271.413 |
| native-prepared | four_table_join | 330.322 | 335.288 | 0.985× | 310.718–349.927 | 319.302–351.273 |
| native-prepared | three_table_left_fence | 455.936 | 460.297 | 0.991× | 441.805–470.066 | 445.732–474.862 |
| native-prepared | join_residual | 3631.461 | 2447.512 | 1.484× | 3604.833–3658.089 | 2403.122–2491.901 |
| native-query | scalar | 6.005 | 6.029 | 0.996× | 5.908–6.102 | 5.812–6.247 |
| native-query | pk_hit | 4.462 | 4.432 | 1.007× | 4.387–4.537 | 4.282–4.583 |
| native-query | indexed_join | 143.316 | 143.469 | 0.999× | 137.230–149.401 | 142.874–144.063 |
| native-query | three_table_join | 217.806 | 246.741 | 0.883× | 206.888–228.725 | 222.953–270.529 |
| native-query | three_table_join_miss | 200.126 | 187.958 | 1.065× | 198.978–201.275 | 183.840–192.076 |
| native-query | three_table_join_range | 1586.250 | 1594.080 | 0.995× | 1548.092–1624.407 | 1585.827–1602.333 |
| native-query | three_table_computed_key | 248.533 | 271.181 | 0.916× | 234.091–262.974 | 267.498–274.863 |
| native-query | four_table_join | 322.645 | 323.466 | 0.997× | 322.507–322.782 | 311.109–335.822 |
| native-query | three_table_left_fence | 473.220 | 464.994 | 1.018× | 463.060–483.380 | 461.171–468.817 |
| native-query | join_residual | 3618.550 | 2523.228 | 1.434× | 3500.823–3736.277 | 2495.426–2551.031 |
| native-eager | scalar | 5.627 | 5.545 | 1.015× | 5.457–5.798 | 5.542–5.547 |
| native-eager | pk_hit | 3.797 | 3.774 | 1.006× | 3.741–3.854 | 3.767–3.782 |
| native-eager | indexed_join | 132.703 | 128.416 | 1.033× | 131.640–133.766 | 126.142–130.691 |
| native-eager | three_table_join | 200.755 | 203.771 | 0.985× | 195.160–206.351 | 194.301–213.242 |
| native-eager | three_table_join_miss | 175.737 | 175.343 | 1.002× | 169.351–182.123 | 172.259–178.428 |
| native-eager | three_table_join_range | 1488.298 | 1536.354 | 0.969× | 1459.241–1517.355 | 1525.377–1547.331 |
| native-eager | three_table_computed_key | 232.917 | 233.627 | 0.997× | 230.740–235.095 | 227.436–239.818 |
| native-eager | four_table_join | 302.415 | 322.191 | 0.939× | 300.042–304.787 | 304.242–340.141 |
| native-eager | three_table_left_fence | 436.968 | 432.392 | 1.011× | 420.958–452.979 | 426.713–438.072 |
| native-eager | join_residual | 2817.684 | 1897.988 | 1.485× | 2770.212–2865.157 | 1859.810–1936.166 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | pk_hit | +0.765% | no |
| 4.0-none | native-prepared | three_table_join_miss | +1.278% | no |
| 4.0-none | native-prepared | three_table_join_range | +0.236% | no |
| 4.0-none | native-prepared | three_table_computed_key | +1.892% | no |
| 4.0-none | native-prepared | four_table_join | +0.451% | no |
| 4.0-none | native-query | three_table_join_miss | +1.208% | no |
| 4.0-none | native-query | three_table_computed_key | +3.410% | yes |
| 4.0-none | native-query | three_table_left_fence | +0.575% | no |
| 4.0-none | native-eager | scalar | +2.365% | yes |
| 4.0-none | native-eager | pk_hit | +4.632% | yes |
| 4.0-none | native-eager | indexed_join | +1.717% | no |
| 4.0-none | native-eager | three_table_join | +4.274% | yes |
| 4.0-none | native-eager | three_table_join_miss | +4.141% | yes |
| 4.0-none | native-eager | three_table_join_range | +3.004% | yes |
| 4.0-none | native-eager | three_table_computed_key | +6.935% | yes |
| 4.0-none | native-eager | four_table_join | +3.268% | yes |
| 4.0-none | native-eager | three_table_left_fence | +3.422% | yes |
| 4.0-yjit | native-prepared | pk_hit | +0.178% | no |
| 4.0-yjit | native-prepared | three_table_join_miss | +0.521% | no |
| 4.0-yjit | native-prepared | three_table_join_range | +1.971% | yes |
| 4.0-yjit | native-prepared | three_table_computed_key | +5.504% | yes |
| 4.0-yjit | native-prepared | three_table_left_fence | +0.224% | yes |
| 4.0-yjit | native-query | scalar | +4.684% | yes |
| 4.0-yjit | native-query | pk_hit | +2.481% | yes |
| 4.0-yjit | native-query | indexed_join | +2.826% | yes |
| 4.0-yjit | native-query | three_table_join | +1.872% | yes |
| 4.0-yjit | native-query | three_table_join_miss | +1.330% | no |
| 4.0-yjit | native-query | three_table_join_range | +3.743% | yes |
| 4.0-yjit | native-query | three_table_computed_key | +8.740% | yes |
| 4.0-yjit | native-query | four_table_join | +4.513% | yes |
| 4.0-yjit | native-query | three_table_left_fence | +3.790% | yes |
| 4.0-yjit | native-eager | scalar | +6.142% | yes |
| 4.0-yjit | native-eager | pk_hit | +2.956% | yes |
| 4.0-yjit | native-eager | indexed_join | +4.080% | yes |
| 4.0-yjit | native-eager | three_table_join | +4.298% | yes |
| 4.0-yjit | native-eager | three_table_join_miss | +4.777% | yes |
| 4.0-yjit | native-eager | three_table_join_range | +5.513% | yes |
| 4.0-yjit | native-eager | three_table_computed_key | +10.356% | yes |
| 4.0-yjit | native-eager | four_table_join | +6.504% | yes |
| 4.0-yjit | native-eager | three_table_left_fence | +4.734% | yes |
| 4.1-zjit | native-prepared | scalar | +1.000% | yes |
| 4.1-zjit | native-prepared | pk_hit | +2.149% | no |
| 4.1-zjit | native-prepared | three_table_join | +32.959% | yes |
| 4.1-zjit | native-prepared | four_table_join | +1.503% | no |
| 4.1-zjit | native-prepared | three_table_left_fence | +0.957% | no |
| 4.1-zjit | native-query | scalar | +0.404% | no |
| 4.1-zjit | native-query | indexed_join | +0.107% | no |
| 4.1-zjit | native-query | three_table_join | +13.285% | no |
| 4.1-zjit | native-query | three_table_join_range | +0.494% | no |
| 4.1-zjit | native-query | three_table_computed_key | +9.113% | yes |
| 4.1-zjit | native-query | four_table_join | +0.255% | no |
| 4.1-zjit | native-eager | three_table_join | +1.502% | no |
| 4.1-zjit | native-eager | three_table_join_range | +3.229% | yes |
| 4.1-zjit | native-eager | three_table_computed_key | +0.305% | no |
| 4.1-zjit | native-eager | four_table_join | +6.540% | no |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.905 | 0.872 | 1.414 | 2.148 | 5.624× | 3.468× |
| pk_hit | 4.052 | 1.573 | 2.235 | 3.748 | 2.576× | 1.813× |
| indexed_join | 153.993 | 55.525 | 2.564 | 120.182 | 2.773× | 60.055× |
| three_table_join | 228.418 | 3.729 | 3.219 | 12.275 | 61.258× | 70.950× |
| three_table_join_miss | 202.418 | 1.265 | 1.507 | 3.757 | 160.033× | 134.278× |
| three_table_join_range | 1341.410 | 75.808 | 61.653 | 232.060 | 17.695× | 21.757× |
| three_table_computed_key | 258.923 | 8.106 | 5.996 | 16.341 | 31.941× | 43.186× |
| four_table_join | 310.285 | 4.120 | 4.057 | 11.920 | 75.308× | 76.484× |
| three_table_left_fence | 442.129 | 3.453 | 3.192 | 11.128 | 128.038× | 138.498× |
| join_residual | 1757.319 | 393.886 | 768.032 | 1633.398 | 4.461× | 2.288× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 7.791 | 4.830 | 5.594 | 5.610 | 0.620× |
| pk_hit | 10.308 | 3.946 | 4.595 | 3.991 | 0.383× |
| indexed_join | 43.578 | 148.667 | 160.284 | 144.003 | 3.412× |
| three_table_join | 177.286 | 218.844 | 231.912 | 218.870 | 1.234× |
| three_table_join_miss | 174.804 | 195.682 | 203.765 | 187.438 | 1.119× |
| three_table_join_range | 234.811 | 1385.417 | 1438.707 | 1377.488 | 5.900× |
| three_table_computed_key | 127.349 | 262.778 | 269.508 | 249.660 | 2.063× |
| four_table_join | 430.788 | 314.195 | 329.192 | 302.713 | 0.729× |
| three_table_left_fence | 51.072 | 440.420 | 457.021 | 431.634 | 8.624× |
| join_residual | 854.054 | 1835.281 | 1915.503 | 1943.596 | 2.149× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10558.200 | 10651.100 | +0.880% |
| three_table_computed_key | 887.000 | 999.000 | +12.627% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2196.800 | +0.000% |
| join_residual | 15592.200 | 15784.200 | +1.231% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| scalar | JedNative::Engine#evaluate | 0.000 | 0.000 |
| scalar | JedNative::Engine#join | 0.000 | 0.000 |
| scalar | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#evaluate | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#join | 0.000 | 0.000 |
| pk_hit | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| indexed_join | JedNative::Engine#evaluate | 1.000 | 1.000 |
| indexed_join | JedNative::Engine#join | 1.000 | 1.000 |
| indexed_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_join | JedNative::Engine#evaluate | 1.000 | 1.000 |
| three_table_join | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_join_miss | JedNative::Engine#evaluate | 0.000 | 0.000 |
| three_table_join_miss | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join_miss | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_range | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 1.333 |
| three_table_join_range | JedNative::Engine#evaluate | 17.000 | 17.000 |
| three_table_join_range | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join_range | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_computed_key | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 1.000 |
| three_table_computed_key | JedNative::Engine#evaluate | 1.000 | 1.000 |
| three_table_computed_key | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_computed_key | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| four_table_join | JedNative::Engine#evaluate | 1.000 | 1.000 |
| four_table_join | JedNative::Engine#join | 3.000 | 3.000 |
| four_table_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_left_fence | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_left_fence | JedNative::Engine#evaluate | 1.000 | 1.000 |
| three_table_left_fence | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_left_fence | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| join_residual | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 1.000 |
| join_residual | JedNative::Engine#evaluate | 0.000 | 0.000 |
| join_residual | JedNative::Engine#join | 1.000 | 1.000 |
| join_residual | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
