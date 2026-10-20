# Join admission measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.252 | 12.334 | 0.993× | 12.252–12.252 | 12.173–12.494 |
| native-prepared | pk_hit | 8.168 | 8.282 | 0.986× | 7.999–8.337 | 8.210–8.355 |
| native-prepared | indexed_join | 257.609 | 260.228 | 0.990× | 254.655–260.562 | 254.221–266.235 |
| native-prepared | three_table_join | 363.521 | 367.630 | 0.989× | 359.254–367.788 | 365.213–370.048 |
| native-prepared | three_table_join_miss | 298.827 | 301.399 | 0.991× | 298.296–299.357 | 298.087–304.711 |
| native-prepared | three_table_join_range | 2835.561 | 2867.242 | 0.989× | 2816.799–2854.324 | 2821.450–2913.034 |
| native-prepared | three_table_computed_key | 411.454 | 400.680 | 1.027× | 408.876–414.033 | 396.839–404.522 |
| native-prepared | four_table_join | 489.971 | 504.101 | 0.972× | 489.300–490.643 | 487.954–520.248 |
| native-prepared | three_table_left_fence | 744.283 | 750.188 | 0.992× | 741.497–747.069 | 740.058–760.318 |
| native-prepared | join_residual | 3797.897 | 3879.394 | 0.979× | 3789.399–3806.396 | 3775.732–3983.056 |
| native-query | scalar | 13.671 | 13.509 | 1.012× | 13.344–13.997 | 13.324–13.695 |
| native-query | pk_hit | 9.971 | 9.610 | 1.038× | 9.706–10.235 | 9.589–9.631 |
| native-query | indexed_join | 262.296 | 261.289 | 1.004× | 257.586–267.005 | 260.441–262.137 |
| native-query | three_table_join | 371.268 | 370.360 | 1.002× | 363.336–379.199 | 367.421–373.299 |
| native-query | three_table_join_miss | 304.740 | 301.376 | 1.011× | 297.878–311.603 | 297.622–305.130 |
| native-query | three_table_join_range | 2948.597 | 2834.233 | 1.040× | 2940.227–2956.966 | 2820.807–2847.659 |
| native-query | three_table_computed_key | 433.657 | 398.648 | 1.088× | 432.416–434.899 | 396.385–400.911 |
| native-query | four_table_join | 519.535 | 494.214 | 1.051× | 513.806–525.263 | 493.792–494.636 |
| native-query | three_table_left_fence | 772.520 | 751.110 | 1.029× | 768.203–776.837 | 742.542–759.679 |
| native-query | join_residual | 3951.337 | 3791.529 | 1.042× | 3950.879–3951.796 | 3774.146–3808.911 |
| native-eager | scalar | 15.696 | 15.201 | 1.033× | 15.447–15.946 | 15.178–15.223 |
| native-eager | pk_hit | 9.755 | 9.506 | 1.026× | 9.506–10.004 | 9.422–9.590 |
| native-eager | indexed_join | 230.061 | 224.137 | 1.026× | 225.470–234.652 | 222.664–225.610 |
| native-eager | three_table_join | 341.742 | 333.876 | 1.024× | 335.169–348.316 | 330.832–336.920 |
| native-eager | three_table_join_miss | 282.291 | 277.228 | 1.018× | 277.878–286.704 | 274.638–279.818 |
| native-eager | three_table_join_range | 2791.272 | 2727.584 | 1.023× | 2752.200–2830.345 | 2720.694–2734.473 |
| native-eager | three_table_computed_key | 382.546 | 358.614 | 1.067× | 375.570–389.523 | 356.496–360.733 |
| native-eager | four_table_join | 473.876 | 465.315 | 1.018× | 464.658–483.093 | 459.131–471.499 |
| native-eager | three_table_left_fence | 713.824 | 693.001 | 1.030× | 699.954–727.695 | 689.423–696.578 |
| native-eager | join_residual | 3506.501 | 3409.050 | 1.029× | 3467.098–3545.903 | 3396.694–3421.406 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.905 | 5.015 | 0.978× | 4.832–4.979 | 5.010–5.020 |
| native-prepared | pk_hit | 4.019 | 4.144 | 0.970× | 3.944–4.094 | 4.130–4.158 |
| native-prepared | indexed_join | 152.730 | 153.684 | 0.994× | 149.783–155.678 | 153.219–154.149 |
| native-prepared | three_table_join | 224.725 | 227.596 | 0.987× | 218.529–230.922 | 226.721–228.471 |
| native-prepared | three_table_join_miss | 195.806 | 200.010 | 0.979× | 191.726–199.887 | 198.830–201.189 |
| native-prepared | three_table_join_range | 1408.662 | 1416.384 | 0.995× | 1382.317–1435.006 | 1406.482–1426.287 |
| native-prepared | three_table_computed_key | 262.908 | 251.032 | 1.047× | 258.160–267.655 | 250.710–251.355 |
| native-prepared | four_table_join | 319.669 | 324.738 | 0.984× | 312.048–327.289 | 323.600–325.877 |
| native-prepared | three_table_left_fence | 441.574 | 452.855 | 0.975× | 431.936–451.212 | 452.157–453.553 |
| native-prepared | join_residual | 1887.083 | 1895.282 | 0.996× | 1840.923–1933.244 | 1893.997–1896.568 |
| native-query | scalar | 5.576 | 5.609 | 0.994× | 5.531–5.621 | 5.595–5.622 |
| native-query | pk_hit | 4.623 | 4.661 | 0.992× | 4.594–4.653 | 4.625–4.697 |
| native-query | indexed_join | 157.832 | 159.429 | 0.990× | 157.296–158.369 | 156.280–162.578 |
| native-query | three_table_join | 233.895 | 230.722 | 1.014× | 232.922–234.868 | 229.370–232.074 |
| native-query | three_table_join_miss | 199.354 | 205.831 | 0.969× | 194.434–204.274 | 203.234–208.427 |
| native-query | three_table_join_range | 1425.160 | 1388.214 | 1.027× | 1424.882–1425.438 | 1387.764–1388.664 |
| native-query | three_table_computed_key | 271.720 | 254.412 | 1.068× | 268.551–274.890 | 254.228–254.597 |
| native-query | four_table_join | 326.704 | 325.370 | 1.004× | 326.637–326.771 | 324.359–326.382 |
| native-query | three_table_left_fence | 455.957 | 463.216 | 0.984× | 454.521–457.394 | 450.014–476.417 |
| native-query | join_residual | 1872.551 | 1927.326 | 0.972× | 1842.050–1903.053 | 1921.333–1933.320 |
| native-eager | scalar | 5.668 | 5.573 | 1.017× | 5.591–5.745 | 5.426–5.719 |
| native-eager | pk_hit | 4.087 | 4.052 | 1.009× | 4.023–4.151 | 3.970–4.133 |
| native-eager | indexed_join | 143.246 | 141.189 | 1.015× | 142.567–143.925 | 138.467–143.911 |
| native-eager | three_table_join | 213.507 | 214.661 | 0.995× | 213.426–213.589 | 210.577–218.746 |
| native-eager | three_table_join_miss | 188.746 | 189.110 | 0.998× | 187.606–189.885 | 186.299–191.920 |
| native-eager | three_table_join_range | 1387.691 | 1397.996 | 0.993× | 1383.517–1391.865 | 1366.498–1429.493 |
| native-eager | three_table_computed_key | 250.605 | 240.266 | 1.043× | 244.975–256.235 | 236.057–244.475 |
| native-eager | four_table_join | 311.635 | 310.785 | 1.003× | 306.656–316.613 | 306.085–315.485 |
| native-eager | three_table_left_fence | 436.490 | 432.290 | 1.010× | 434.401–438.579 | 425.652–438.929 |
| native-eager | join_residual | 1904.267 | 1933.109 | 0.985× | 1899.804–1908.730 | 1891.607–1974.611 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.553 | 5.545 | 1.001× | 5.362–5.745 | 5.415–5.675 |
| native-prepared | pk_hit | 4.066 | 3.976 | 1.023× | 3.957–4.176 | 3.888–4.065 |
| native-prepared | indexed_join | 138.232 | 143.832 | 0.961× | 134.819–141.644 | 138.967–148.697 |
| native-prepared | three_table_join | 259.448 | 224.164 | 1.157× | 251.280–267.616 | 216.905–231.424 |
| native-prepared | three_table_join_miss | 179.593 | 189.984 | 0.945× | 179.314–179.872 | 187.009–192.959 |
| native-prepared | three_table_join_range | 1527.492 | 1581.481 | 0.966× | 1517.511–1537.474 | 1532.643–1630.319 |
| native-prepared | three_table_computed_key | 242.327 | 242.890 | 0.998× | 241.680–242.974 | 239.345–246.435 |
| native-prepared | four_table_join | 300.867 | 341.542 | 0.881× | 300.745–300.988 | 329.844–353.239 |
| native-prepared | three_table_left_fence | 452.292 | 456.420 | 0.991× | 444.162–460.423 | 444.370–468.470 |
| native-prepared | join_residual | 2402.706 | 2390.696 | 1.005× | 2386.204–2419.209 | 2353.130–2428.261 |
| native-query | scalar | 5.834 | 6.111 | 0.955× | 5.722–5.945 | 6.018–6.203 |
| native-query | pk_hit | 4.342 | 4.594 | 0.945× | 4.254–4.430 | 4.542–4.647 |
| native-query | indexed_join | 144.646 | 141.082 | 1.025× | 142.457–146.836 | 136.456–145.708 |
| native-query | three_table_join | 210.531 | 242.377 | 0.869× | 202.656–218.406 | 219.999–264.756 |
| native-query | three_table_join_miss | 186.005 | 192.298 | 0.967× | 182.364–189.645 | 191.170–193.425 |
| native-query | three_table_join_range | 1551.450 | 1583.135 | 0.980× | 1515.975–1586.926 | 1546.456–1619.814 |
| native-query | three_table_computed_key | 263.542 | 253.815 | 1.038× | 262.442–264.642 | 248.973–258.656 |
| native-query | four_table_join | 324.143 | 324.154 | 1.000× | 320.689–327.598 | 317.288–331.020 |
| native-query | three_table_left_fence | 440.773 | 457.270 | 0.964× | 439.995–441.551 | 445.194–469.347 |
| native-query | join_residual | 2411.076 | 2395.165 | 1.007× | 2372.812–2449.340 | 2352.143–2438.188 |
| native-eager | scalar | 5.645 | 5.574 | 1.013× | 5.643–5.647 | 5.519–5.629 |
| native-eager | pk_hit | 3.765 | 3.720 | 1.012× | 3.727–3.803 | 3.702–3.738 |
| native-eager | indexed_join | 133.034 | 128.134 | 1.038× | 132.061–134.008 | 124.831–131.438 |
| native-eager | three_table_join | 205.571 | 202.034 | 1.018× | 205.039–206.104 | 193.033–211.036 |
| native-eager | three_table_join_miss | 177.558 | 173.859 | 1.021× | 171.168–183.948 | 169.384–178.333 |
| native-eager | three_table_join_range | 1528.107 | 1484.851 | 1.029× | 1499.561–1556.652 | 1469.884–1499.819 |
| native-eager | three_table_computed_key | 230.925 | 261.782 | 0.882× | 226.669–235.180 | 235.435–288.130 |
| native-eager | four_table_join | 324.451 | 309.435 | 1.049× | 295.542–353.360 | 303.619–315.252 |
| native-eager | three_table_left_fence | 439.468 | 419.468 | 1.048× | 429.234–449.701 | 418.042–420.893 |
| native-eager | join_residual | 1905.081 | 1854.409 | 1.027× | 1857.864–1952.298 | 1822.220–1886.598 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | scalar | +0.669% | no |
| 4.0-none | native-prepared | pk_hit | +1.398% | no |
| 4.0-none | native-prepared | indexed_join | +1.017% | no |
| 4.0-none | native-prepared | three_table_join | +1.130% | no |
| 4.0-none | native-prepared | three_table_join_miss | +0.861% | no |
| 4.0-none | native-prepared | three_table_join_range | +1.117% | no |
| 4.0-none | native-prepared | four_table_join | +2.884% | no |
| 4.0-none | native-prepared | three_table_left_fence | +0.793% | no |
| 4.0-none | native-prepared | join_residual | +2.146% | no |
| 4.0-yjit | native-prepared | scalar | +2.231% | yes |
| 4.0-yjit | native-prepared | pk_hit | +3.119% | yes |
| 4.0-yjit | native-prepared | indexed_join | +0.624% | no |
| 4.0-yjit | native-prepared | three_table_join | +1.277% | no |
| 4.0-yjit | native-prepared | three_table_join_miss | +2.147% | no |
| 4.0-yjit | native-prepared | three_table_join_range | +0.548% | no |
| 4.0-yjit | native-prepared | four_table_join | +1.586% | no |
| 4.0-yjit | native-prepared | three_table_left_fence | +2.555% | yes |
| 4.0-yjit | native-prepared | join_residual | +0.434% | no |
| 4.0-yjit | native-query | scalar | +0.591% | no |
| 4.0-yjit | native-query | pk_hit | +0.823% | no |
| 4.0-yjit | native-query | indexed_join | +1.012% | no |
| 4.0-yjit | native-query | three_table_join_miss | +3.249% | no |
| 4.0-yjit | native-query | three_table_left_fence | +1.592% | no |
| 4.0-yjit | native-query | join_residual | +2.925% | yes |
| 4.0-yjit | native-eager | three_table_join | +0.540% | no |
| 4.0-yjit | native-eager | three_table_join_miss | +0.193% | no |
| 4.0-yjit | native-eager | three_table_join_range | +0.743% | no |
| 4.0-yjit | native-eager | join_residual | +1.515% | no |
| 4.1-zjit | native-prepared | indexed_join | +4.051% | no |
| 4.1-zjit | native-prepared | three_table_join_miss | +5.786% | yes |
| 4.1-zjit | native-prepared | three_table_join_range | +3.534% | no |
| 4.1-zjit | native-prepared | three_table_computed_key | +0.232% | no |
| 4.1-zjit | native-prepared | four_table_join | +13.519% | yes |
| 4.1-zjit | native-prepared | three_table_left_fence | +0.912% | no |
| 4.1-zjit | native-query | scalar | +4.746% | yes |
| 4.1-zjit | native-query | pk_hit | +5.812% | yes |
| 4.1-zjit | native-query | three_table_join | +15.127% | yes |
| 4.1-zjit | native-query | three_table_join_miss | +3.383% | yes |
| 4.1-zjit | native-query | three_table_join_range | +2.042% | no |
| 4.1-zjit | native-query | four_table_join | +0.003% | no |
| 4.1-zjit | native-query | three_table_left_fence | +3.743% | yes |
| 4.1-zjit | native-eager | three_table_computed_key | +13.363% | yes |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.609 | 0.834 | 1.439 | 2.326 | 5.523× | 3.204× |
| pk_hit | 3.860 | 1.721 | 2.245 | 4.120 | 2.243× | 1.719× |
| indexed_join | 149.321 | 50.357 | 10.933 | 120.115 | 2.965× | 13.657× |
| three_table_join | 219.250 | 3.291 | 5.666 | 12.754 | 66.616× | 38.692× |
| three_table_join_miss | 193.255 | 1.169 | 1.501 | 3.575 | 165.260× | 128.725× |
| three_table_join_range | 1325.507 | 87.501 | 59.565 | 240.708 | 15.149× | 22.253× |
| three_table_computed_key | 242.658 | 6.300 | 5.811 | 16.552 | 38.516× | 41.760× |
| four_table_join | 304.895 | 3.069 | 3.890 | 12.028 | 99.334× | 78.377× |
| three_table_left_fence | 426.761 | 2.181 | 3.091 | 11.030 | 195.708× | 138.066× |
| join_residual | 1781.009 | 400.023 | 739.813 | 1564.693 | 4.452× | 2.407× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.117 | 5.119 | 5.594 | 5.739 | 0.631× |
| pk_hit | 10.711 | 4.057 | 4.604 | 4.176 | 0.379× |
| indexed_join | 43.258 | 154.508 | 157.322 | 145.269 | 3.572× |
| three_table_join | 184.330 | 221.662 | 228.001 | 218.331 | 1.203× |
| three_table_join_miss | 182.681 | 200.907 | 199.274 | 192.339 | 1.100× |
| three_table_join_range | 242.315 | 1389.657 | 1398.442 | 1411.290 | 5.735× |
| three_table_computed_key | 132.112 | 249.747 | 251.648 | 238.036 | 1.890× |
| four_table_join | 418.814 | 320.367 | 333.183 | 310.777 | 0.765× |
| three_table_left_fence | 49.841 | 444.505 | 455.858 | 443.656 | 8.918× |
| join_residual | 885.755 | 1892.018 | 1905.299 | 1967.944 | 2.136× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10651.050 | 10559.900 | -0.856% |
| three_table_computed_key | 999.000 | 887.000 | -11.211% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2196.800 | +0.000% |
| join_residual | 15784.200 | 15784.200 | +0.000% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| scalar | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| scalar | JedNative::FastEvaluation#evaluate | 0.000 | 0.000 |
| scalar | JedNative::Engine#join | 0.000 | 0.000 |
| scalar | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| pk_hit | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| pk_hit | JedNative::FastEvaluation#evaluate | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#join | 0.000 | 0.000 |
| pk_hit | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| indexed_join | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| indexed_join | JedNative::FastEvaluation#evaluate | 17.000 | 17.000 |
| indexed_join | JedNative::Engine#join | 1.000 | 1.000 |
| indexed_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_join | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| three_table_join | JedNative::FastEvaluation#evaluate | 28.000 | 28.000 |
| three_table_join | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_join_miss | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| three_table_join_miss | JedNative::FastEvaluation#evaluate | 1.000 | 1.000 |
| three_table_join_miss | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join_miss | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_range | JedNative::CompiledPlans#native_compiled_join_predicate | 1.333 | 1.333 |
| three_table_join_range | #<Class:JedNative::CompiledExpression>#prepare | 1.333 | 0.000 |
| three_table_join_range | JedNative::FastEvaluation#evaluate | 358.000 | 460.000 |
| three_table_join_range | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_join_range | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_computed_key | JedNative::CompiledPlans#native_compiled_join_predicate | 1.000 | 0.000 |
| three_table_computed_key | #<Class:JedNative::CompiledExpression>#prepare | 1.000 | 0.000 |
| three_table_computed_key | JedNative::FastEvaluation#evaluate | 49.000 | 99.000 |
| three_table_computed_key | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_computed_key | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| four_table_join | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| four_table_join | JedNative::FastEvaluation#evaluate | 39.000 | 39.000 |
| four_table_join | JedNative::Engine#join | 3.000 | 3.000 |
| four_table_join | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_left_fence | JedNative::CompiledPlans#native_compiled_join_predicate | 0.000 | 0.000 |
| three_table_left_fence | #<Class:JedNative::CompiledExpression>#prepare | 0.000 | 0.000 |
| three_table_left_fence | JedNative::FastEvaluation#evaluate | 27.000 | 27.000 |
| three_table_left_fence | JedNative::Engine#join | 2.000 | 2.000 |
| three_table_left_fence | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| join_residual | JedNative::CompiledPlans#native_compiled_join_predicate | 1.000 | 1.000 |
| join_residual | #<Class:JedNative::CompiledExpression>#prepare | 1.000 | 1.000 |
| join_residual | JedNative::FastEvaluation#evaluate | 272.000 | 272.000 |
| join_residual | JedNative::Engine#join | 1.000 | 1.000 |
| join_residual | JedNative::SpillSelect#spill_join | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
