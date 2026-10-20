# Outer-join measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.039 | 12.272 | 0.981× | 12.029–12.048 | 12.128–12.415 |
| native-prepared | pk_hit | 8.116 | 8.063 | 1.007× | 7.983–8.248 | 7.906–8.219 |
| native-prepared | indexed_join | 255.098 | 259.950 | 0.981× | 254.179–256.018 | 256.281–263.620 |
| native-prepared | three_table_join | 358.976 | 368.257 | 0.975× | 357.723–360.229 | 363.233–373.282 |
| native-prepared | three_table_join_miss | 295.842 | 304.051 | 0.973× | 295.353–296.331 | 299.077–309.025 |
| native-prepared | three_table_join_range | 2794.677 | 2865.992 | 0.975× | 2791.346–2798.007 | 2827.203–2904.780 |
| native-prepared | three_table_computed_key | 396.631 | 403.891 | 0.982× | 391.396–401.867 | 396.069–411.714 |
| native-prepared | four_table_join | 498.347 | 499.088 | 0.999× | 497.925–498.768 | 492.984–505.191 |
| native-prepared | three_table_left_fence | 1303.038 | 748.820 | 1.740× | 1291.546–1314.530 | 735.414–762.227 |
| native-query | scalar | 13.916 | 13.294 | 1.047× | 13.493–14.338 | 13.258–13.330 |
| native-query | pk_hit | 9.814 | 9.567 | 1.026× | 9.629–10.000 | 9.564–9.570 |
| native-query | indexed_join | 261.989 | 255.976 | 1.023× | 258.958–265.021 | 254.492–257.460 |
| native-query | three_table_join | 376.540 | 361.377 | 1.042× | 375.338–377.742 | 357.582–365.171 |
| native-query | three_table_join_miss | 299.346 | 293.543 | 1.020× | 293.611–305.080 | 292.698–294.388 |
| native-query | three_table_join_range | 2862.399 | 2823.510 | 1.014× | 2810.459–2914.339 | 2820.381–2826.638 |
| native-query | three_table_computed_key | 404.551 | 395.954 | 1.022× | 397.036–412.065 | 393.285–398.623 |
| native-query | four_table_join | 502.707 | 489.418 | 1.027× | 496.606–508.808 | 487.525–491.311 |
| native-query | three_table_left_fence | 1338.970 | 757.833 | 1.767× | 1321.749–1356.190 | 738.326–777.340 |
| native-eager | scalar | 15.332 | 15.905 | 0.964× | 15.187–15.476 | 15.716–16.093 |
| native-eager | pk_hit | 9.460 | 9.815 | 0.964× | 9.395–9.525 | 9.646–9.984 |
| native-eager | indexed_join | 224.149 | 233.079 | 0.962× | 223.928–224.370 | 230.527–235.631 |
| native-eager | three_table_join | 329.775 | 346.367 | 0.952× | 325.685–333.865 | 346.215–346.518 |
| native-eager | three_table_join_miss | 276.820 | 285.991 | 0.968× | 272.822–280.818 | 284.198–287.784 |
| native-eager | three_table_join_range | 2669.899 | 2807.437 | 0.951× | 2662.491–2677.306 | 2800.306–2814.567 |
| native-eager | three_table_computed_key | 357.961 | 372.263 | 0.962× | 357.463–358.459 | 367.446–377.080 |
| native-eager | four_table_join | 459.484 | 477.600 | 0.962× | 457.956–461.012 | 477.269–477.931 |
| native-eager | three_table_left_fence | 1246.380 | 720.637 | 1.730× | 1228.223–1264.538 | 716.007–725.266 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.942 | 5.108 | 0.968× | 4.874–5.010 | 5.073–5.142 |
| native-prepared | pk_hit | 3.984 | 4.110 | 0.969× | 3.961–4.007 | 4.105–4.115 |
| native-prepared | indexed_join | 150.267 | 158.525 | 0.948× | 150.025–150.509 | 157.505–159.545 |
| native-prepared | three_table_join | 220.511 | 233.155 | 0.946× | 218.885–222.136 | 230.359–235.950 |
| native-prepared | three_table_join_miss | 196.078 | 204.807 | 0.957× | 195.224–196.932 | 204.702–204.912 |
| native-prepared | three_table_join_range | 1348.725 | 1416.781 | 0.952× | 1337.429–1360.020 | 1403.753–1429.808 |
| native-prepared | three_table_computed_key | 246.860 | 251.560 | 0.981× | 246.134–247.587 | 250.146–252.974 |
| native-prepared | four_table_join | 310.630 | 328.113 | 0.947× | 309.785–311.476 | 328.110–328.116 |
| native-prepared | three_table_left_fence | 707.771 | 454.562 | 1.557× | 703.003–712.539 | 453.973–455.151 |
| native-query | scalar | 5.459 | 5.359 | 1.019× | 5.395–5.522 | 5.291–5.427 |
| native-query | pk_hit | 4.503 | 4.542 | 0.991× | 4.500–4.506 | 4.441–4.644 |
| native-query | indexed_join | 150.721 | 149.304 | 1.009× | 150.513–150.929 | 147.371–151.236 |
| native-query | three_table_join | 224.209 | 221.507 | 1.012× | 221.299–227.119 | 217.866–225.148 |
| native-query | three_table_join_miss | 195.707 | 190.385 | 1.028× | 192.420–198.993 | 189.652–191.118 |
| native-query | three_table_join_range | 1351.110 | 1343.458 | 1.006× | 1344.968–1357.253 | 1329.701–1357.216 |
| native-query | three_table_computed_key | 243.017 | 244.892 | 0.992× | 241.999–244.035 | 242.991–246.792 |
| native-query | four_table_join | 311.664 | 314.224 | 0.992× | 309.538–313.790 | 312.676–315.773 |
| native-query | three_table_left_fence | 714.663 | 436.082 | 1.639× | 708.012–721.314 | 432.491–439.673 |
| native-eager | scalar | 5.373 | 5.396 | 0.996× | 5.319–5.427 | 5.327–5.465 |
| native-eager | pk_hit | 3.978 | 3.918 | 1.015× | 3.889–4.068 | 3.895–3.941 |
| native-eager | indexed_join | 141.465 | 137.525 | 1.029× | 134.053–148.877 | 136.150–138.901 |
| native-eager | three_table_join | 219.154 | 208.718 | 1.050× | 209.056–229.252 | 206.816–210.621 |
| native-eager | three_table_join_miss | 186.260 | 182.671 | 1.020× | 173.926–198.595 | 180.919–184.423 |
| native-eager | three_table_join_range | 1377.719 | 1334.341 | 1.033× | 1331.187–1424.251 | 1327.566–1341.117 |
| native-eager | three_table_computed_key | 238.137 | 228.802 | 1.041× | 231.940–244.334 | 228.451–229.154 |
| native-eager | four_table_join | 309.529 | 299.545 | 1.033× | 302.275–316.784 | 297.191–301.899 |
| native-eager | three_table_left_fence | 689.759 | 416.681 | 1.655× | 675.815–703.703 | 415.650–417.712 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.395 | 5.656 | 0.954× | 5.299–5.490 | 5.582–5.731 |
| native-prepared | pk_hit | 4.031 | 4.071 | 0.990× | 3.984–4.079 | 4.061–4.080 |
| native-prepared | indexed_join | 137.435 | 146.118 | 0.941× | 135.715–139.155 | 145.496–146.741 |
| native-prepared | three_table_join | 272.690 | 238.458 | 1.144× | 271.185–274.194 | 228.726–248.191 |
| native-prepared | three_table_join_miss | 185.313 | 185.982 | 0.996× | 184.095–186.531 | 185.138–186.826 |
| native-prepared | three_table_join_range | 1616.027 | 1615.673 | 1.000× | 1603.623–1628.430 | 1583.430–1647.915 |
| native-prepared | three_table_computed_key | 245.657 | 240.383 | 1.022× | 232.732–258.581 | 240.256–240.511 |
| native-prepared | four_table_join | 310.758 | 378.774 | 0.820× | 299.190–322.325 | 366.973–390.576 |
| native-prepared | three_table_left_fence | 672.519 | 467.129 | 1.440× | 668.748–676.289 | 459.409–474.848 |
| native-query | scalar | 5.872 | 6.048 | 0.971× | 5.721–6.023 | 5.907–6.189 |
| native-query | pk_hit | 4.481 | 4.529 | 0.989× | 4.437–4.524 | 4.415–4.642 |
| native-query | indexed_join | 142.705 | 142.323 | 1.003× | 142.326–143.083 | 141.804–142.842 |
| native-query | three_table_join | 240.860 | 258.491 | 0.932× | 213.122–268.599 | 234.260–282.721 |
| native-query | three_table_join_miss | 188.324 | 184.538 | 1.021× | 181.938–194.710 | 178.456–190.620 |
| native-query | three_table_join_range | 1587.027 | 1606.882 | 0.988× | 1572.319–1601.736 | 1594.282–1619.483 |
| native-query | three_table_computed_key | 246.100 | 245.426 | 1.003× | 243.755–248.445 | 230.737–260.116 |
| native-query | four_table_join | 337.157 | 319.022 | 1.057× | 301.907–372.407 | 304.084–333.960 |
| native-query | three_table_left_fence | 665.583 | 472.514 | 1.409× | 650.882–680.283 | 466.357–478.670 |
| native-eager | scalar | 5.799 | 5.637 | 1.029× | 5.755–5.843 | 5.440–5.833 |
| native-eager | pk_hit | 3.913 | 3.853 | 1.016× | 3.868–3.957 | 3.743–3.962 |
| native-eager | indexed_join | 135.401 | 133.783 | 1.012× | 134.382–136.420 | 129.737–137.829 |
| native-eager | three_table_join | 213.910 | 212.051 | 1.009× | 211.780–216.040 | 206.805–217.298 |
| native-eager | three_table_join_miss | 182.491 | 180.568 | 1.011× | 178.258–186.725 | 178.450–182.685 |
| native-eager | three_table_join_range | 1492.111 | 1509.423 | 0.989× | 1476.895–1507.327 | 1503.964–1514.881 |
| native-eager | three_table_computed_key | 237.325 | 231.214 | 1.026× | 234.265–240.384 | 230.412–232.016 |
| native-eager | four_table_join | 308.428 | 316.691 | 0.974× | 306.971–309.885 | 309.384–323.998 |
| native-eager | three_table_left_fence | 639.495 | 456.319 | 1.401× | 636.495–642.494 | 442.675–469.963 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | scalar | +1.935% | yes |
| 4.0-none | native-prepared | indexed_join | +1.902% | yes |
| 4.0-none | native-prepared | three_table_join | +2.585% | yes |
| 4.0-none | native-prepared | three_table_join_miss | +2.775% | yes |
| 4.0-none | native-prepared | three_table_join_range | +2.552% | yes |
| 4.0-none | native-prepared | three_table_computed_key | +1.830% | no |
| 4.0-none | native-prepared | four_table_join | +0.149% | no |
| 4.0-none | native-eager | scalar | +3.737% | yes |
| 4.0-none | native-eager | pk_hit | +3.752% | yes |
| 4.0-none | native-eager | indexed_join | +3.984% | yes |
| 4.0-none | native-eager | three_table_join | +5.031% | yes |
| 4.0-none | native-eager | three_table_join_miss | +3.313% | yes |
| 4.0-none | native-eager | three_table_join_range | +5.151% | yes |
| 4.0-none | native-eager | three_table_computed_key | +3.995% | yes |
| 4.0-none | native-eager | four_table_join | +3.943% | yes |
| 4.0-yjit | native-prepared | scalar | +3.353% | yes |
| 4.0-yjit | native-prepared | pk_hit | +3.158% | yes |
| 4.0-yjit | native-prepared | indexed_join | +5.495% | yes |
| 4.0-yjit | native-prepared | three_table_join | +5.734% | yes |
| 4.0-yjit | native-prepared | three_table_join_miss | +4.452% | yes |
| 4.0-yjit | native-prepared | three_table_join_range | +5.046% | yes |
| 4.0-yjit | native-prepared | three_table_computed_key | +1.904% | yes |
| 4.0-yjit | native-prepared | four_table_join | +5.628% | yes |
| 4.0-yjit | native-query | pk_hit | +0.872% | no |
| 4.0-yjit | native-query | three_table_computed_key | +0.771% | no |
| 4.0-yjit | native-query | four_table_join | +0.821% | no |
| 4.0-yjit | native-eager | scalar | +0.438% | no |
| 4.1-zjit | native-prepared | scalar | +4.855% | yes |
| 4.1-zjit | native-prepared | pk_hit | +0.971% | no |
| 4.1-zjit | native-prepared | indexed_join | +6.319% | yes |
| 4.1-zjit | native-prepared | three_table_join_miss | +0.361% | no |
| 4.1-zjit | native-prepared | four_table_join | +21.887% | yes |
| 4.1-zjit | native-query | scalar | +2.998% | no |
| 4.1-zjit | native-query | pk_hit | +1.072% | no |
| 4.1-zjit | native-query | three_table_join | +7.320% | no |
| 4.1-zjit | native-query | three_table_join_range | +1.251% | no |
| 4.1-zjit | native-eager | three_table_join_range | +1.160% | no |
| 4.1-zjit | native-eager | four_table_join | +2.679% | no |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.846 | 0.729 | 1.411 | 1.757 | 6.652× | 3.436× |
| pk_hit | 4.105 | 1.527 | 1.991 | 3.338 | 2.689× | 2.062× |
| indexed_join | 153.878 | 48.479 | 2.639 | 108.138 | 3.174× | 58.314× |
| three_table_join | 226.142 | 3.361 | 3.255 | 12.172 | 67.292× | 69.477× |
| three_table_join_miss | 197.976 | 1.153 | 1.548 | 3.360 | 171.743× | 127.871× |
| three_table_join_range | 1374.012 | 74.505 | 62.146 | 216.968 | 18.442× | 22.110× |
| three_table_computed_key | 252.673 | 8.788 | 6.142 | 15.311 | 28.751× | 41.137× |
| four_table_join | 325.719 | 4.195 | 4.125 | 11.296 | 77.638× | 78.965× |
| three_table_left_fence | 440.050 | 3.274 | 3.263 | 10.773 | 134.395× | 134.879× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 7.674 | 4.869 | 5.404 | 5.506 | 0.635× |
| pk_hit | 10.150 | 3.957 | 4.437 | 3.989 | 0.390× |
| indexed_join | 35.159 | 148.066 | 152.417 | 140.863 | 4.211× |
| three_table_join | 134.728 | 219.571 | 224.207 | 210.837 | 1.630× |
| three_table_join_miss | 155.007 | 198.590 | 198.504 | 186.086 | 1.281× |
| three_table_join_range | 224.404 | 1322.435 | 1347.632 | 1388.410 | 5.893× |
| three_table_computed_key | 134.900 | 245.928 | 248.884 | 237.439 | 1.823× |
| four_table_join | 415.082 | 315.334 | 312.069 | 301.917 | 0.760× |
| three_table_left_fence | 48.145 | 427.147 | 435.991 | 432.699 | 8.872× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10558.200 | 10558.300 | +0.001% |
| three_table_computed_key | 887.000 | 887.000 | +0.000% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 5922.900 | 2196.800 | -62.910% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::Engine#compile | 0.000 | 0.000 |
| scalar | JedNative::PreparedJoins#prepared_join_root | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| scalar | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#compile | 0.000 | 0.000 |
| pk_hit | JedNative::PreparedJoins#prepared_join_root | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::Engine#compile | 0.000 | 0.000 |
| indexed_join | JedNative::PreparedJoins#prepared_join_root | 12.000 | 12.000 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join | JedNative::PreparedJoins#prepared_join_root | 16.000 | 16.000 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join_miss | JedNative::PreparedJoins#prepared_join_root | 16.000 | 16.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_range | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_join_range | JedNative::PreparedJoins#prepared_join_root | 55.000 | 55.000 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_computed_key | JedNative::Engine#compile | 0.000 | 0.000 |
| three_table_computed_key | JedNative::PreparedJoins#prepared_join_root | 18.000 | 18.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_computed_key | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::Engine#compile | 0.000 | 0.000 |
| four_table_join | JedNative::PreparedJoins#prepared_join_root | 21.000 | 21.000 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_left_fence | JedNative::Engine#compile | 110.000 | 0.000 |
| three_table_left_fence | JedNative::PreparedJoins#prepared_join_root | 0.000 | 36.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
