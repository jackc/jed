# JOIN builtin resolution measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.211 | 12.714 | 0.960× | 12.022–12.400 | 12.555–12.873 |
| native-prepared | pk_hit | 8.179 | 8.517 | 0.960× | 8.162–8.196 | 8.516–8.518 |
| native-prepared | indexed_join | 261.821 | 265.358 | 0.987× | 253.523–270.119 | 263.519–267.197 |
| native-prepared | three_table_join | 368.387 | 377.504 | 0.976× | 355.715–381.059 | 375.261–379.748 |
| native-prepared | three_table_join_miss | 309.618 | 311.201 | 0.995× | 300.348–318.887 | 310.622–311.780 |
| native-prepared | three_table_join_range | 2937.350 | 2924.886 | 1.004× | 2918.835–2955.865 | 2911.257–2938.516 |
| native-prepared | three_table_computed_key | 409.652 | 418.011 | 0.980× | 409.493–409.811 | 414.992–421.029 |
| native-prepared | four_table_join | 518.061 | 512.968 | 1.010× | 514.173–521.948 | 512.616–513.320 |
| native-prepared | three_table_left_fence | 769.819 | 766.764 | 1.004× | 769.509–770.130 | 766.731–766.797 |
| native-prepared | join_residual | 3963.059 | 1988.315 | 1.993× | 3945.786–3980.332 | 1981.720–1994.910 |
| native-prepared | join_text_predicate | 2700.082 | 307.215 | 8.789× | 2696.024–2704.140 | 301.366–313.063 |
| native-prepared | join_text_projection | 1830.315 | 273.191 | 6.700× | 1817.409–1843.221 | 270.913–275.470 |
| native-prepared | join_integer_outputs | 2557.241 | 308.059 | 8.301× | 2554.628–2559.854 | 306.555–309.563 |
| native-query | scalar | 14.073 | 13.790 | 1.021× | 14.020–14.126 | 13.308–14.272 |
| native-query | pk_hit | 9.973 | 9.808 | 1.017× | 9.914–10.032 | 9.590–10.027 |
| native-query | indexed_join | 269.900 | 262.387 | 1.029× | 269.056–270.745 | 256.758–268.016 |
| native-query | three_table_join | 377.938 | 376.532 | 1.004× | 374.466–381.410 | 370.358–382.706 |
| native-query | three_table_join_miss | 311.181 | 309.701 | 1.005× | 309.162–313.201 | 307.852–311.551 |
| native-query | three_table_join_range | 2941.546 | 2808.752 | 1.047× | 2940.136–2942.956 | 2807.607–2809.897 |
| native-query | three_table_computed_key | 411.512 | 398.884 | 1.032× | 410.590–412.433 | 398.132–399.637 |
| native-query | four_table_join | 515.793 | 493.431 | 1.045× | 512.506–519.080 | 493.059–493.803 |
| native-query | three_table_left_fence | 769.945 | 744.053 | 1.035× | 768.534–771.355 | 736.575–751.531 |
| native-query | join_residual | 3961.580 | 1921.004 | 2.062× | 3956.767–3966.394 | 1916.810–1925.198 |
| native-query | join_text_predicate | 2716.792 | 305.268 | 8.900× | 2699.454–2734.129 | 302.456–308.080 |
| native-query | join_text_projection | 1842.270 | 262.529 | 7.017× | 1831.592–1852.948 | 262.481–262.577 |
| native-query | join_integer_outputs | 2556.537 | 302.987 | 8.438× | 2549.852–2563.222 | 294.315–311.660 |
| native-eager | scalar | 15.602 | 15.569 | 1.002× | 15.326–15.879 | 15.192–15.947 |
| native-eager | pk_hit | 9.575 | 9.807 | 0.976× | 9.513–9.637 | 9.669–9.944 |
| native-eager | indexed_join | 224.542 | 229.743 | 0.977× | 220.996–228.088 | 224.693–234.793 |
| native-eager | three_table_join | 338.651 | 341.372 | 0.992× | 330.087–347.215 | 336.692–346.051 |
| native-eager | three_table_join_miss | 283.157 | 284.253 | 0.996× | 282.692–283.622 | 279.975–288.530 |
| native-eager | three_table_join_range | 2763.090 | 2777.428 | 0.995× | 2704.155–2822.025 | 2712.088–2842.768 |
| native-eager | three_table_computed_key | 373.098 | 367.693 | 1.015× | 369.743–376.452 | 358.424–376.963 |
| native-eager | four_table_join | 476.299 | 481.709 | 0.989× | 473.246–479.352 | 469.093–494.325 |
| native-eager | three_table_left_fence | 709.203 | 707.462 | 1.002× | 693.821–724.585 | 695.467–719.457 |
| native-eager | join_residual | 3506.949 | 1604.331 | 2.186× | 3411.304–3602.595 | 1559.587–1649.075 |
| native-eager | join_text_predicate | 2603.418 | 272.146 | 9.566× | 2577.120–2629.715 | 264.774–279.517 |
| native-eager | join_text_projection | 1732.619 | 235.563 | 7.355× | 1707.949–1757.288 | 231.783–239.344 |
| native-eager | join_integer_outputs | 2456.418 | 267.981 | 9.166× | 2418.402–2494.433 | 263.313–272.648 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.097 | 4.894 | 1.041× | 5.065–5.130 | 4.875–4.914 |
| native-prepared | pk_hit | 4.071 | 4.001 | 1.018× | 4.065–4.077 | 3.980–4.021 |
| native-prepared | indexed_join | 155.652 | 151.325 | 1.029× | 155.204–156.101 | 150.842–151.809 |
| native-prepared | three_table_join | 222.180 | 222.228 | 1.000× | 219.745–224.616 | 221.328–223.128 |
| native-prepared | three_table_join_miss | 199.191 | 199.095 | 1.000× | 197.722–200.659 | 196.554–201.636 |
| native-prepared | three_table_join_range | 1399.050 | 1361.250 | 1.028× | 1394.316–1403.784 | 1353.946–1368.555 |
| native-prepared | three_table_computed_key | 256.620 | 249.115 | 1.030× | 255.980–257.259 | 247.868–250.361 |
| native-prepared | four_table_join | 324.491 | 316.953 | 1.024× | 323.262–325.721 | 312.789–321.116 |
| native-prepared | three_table_left_fence | 449.910 | 436.862 | 1.030× | 447.440–452.379 | 436.130–437.594 |
| native-prepared | join_residual | 1904.519 | 1052.288 | 1.810× | 1887.325–1921.713 | 1043.940–1060.636 |
| native-prepared | join_text_predicate | 1497.894 | 180.946 | 8.278× | 1483.709–1512.079 | 179.379–182.514 |
| native-prepared | join_text_projection | 956.151 | 151.216 | 6.323× | 955.201–957.101 | 151.040–151.393 |
| native-prepared | join_integer_outputs | 1396.607 | 180.491 | 7.738× | 1395.642–1397.573 | 179.154–181.829 |
| native-query | scalar | 5.502 | 5.362 | 1.026× | 5.332–5.671 | 5.335–5.389 |
| native-query | pk_hit | 4.525 | 4.470 | 1.012× | 4.433–4.616 | 4.447–4.492 |
| native-query | indexed_join | 156.231 | 150.084 | 1.041× | 153.783–158.680 | 148.756–151.411 |
| native-query | three_table_join | 228.571 | 219.575 | 1.041× | 224.976–232.165 | 214.891–224.259 |
| native-query | three_table_join_miss | 202.623 | 193.255 | 1.048× | 196.758–208.488 | 192.798–193.711 |
| native-query | three_table_join_range | 1390.188 | 1349.725 | 1.030× | 1373.233–1407.142 | 1330.561–1368.890 |
| native-query | three_table_computed_key | 250.158 | 242.909 | 1.030× | 243.329–256.988 | 242.123–243.696 |
| native-query | four_table_join | 319.277 | 311.033 | 1.027× | 310.560–327.994 | 307.018–315.048 |
| native-query | three_table_left_fence | 450.979 | 437.289 | 1.031× | 450.129–451.829 | 432.233–442.345 |
| native-query | join_residual | 1924.957 | 1011.000 | 1.904× | 1902.963–1946.951 | 990.663–1031.338 |
| native-query | join_text_predicate | 1502.023 | 179.791 | 8.354× | 1473.178–1530.868 | 179.636–179.946 |
| native-query | join_text_projection | 966.822 | 157.079 | 6.155× | 951.891–981.752 | 156.136–158.023 |
| native-query | join_integer_outputs | 1411.339 | 184.333 | 7.656× | 1394.022–1428.655 | 183.568–185.097 |
| native-eager | scalar | 5.769 | 5.501 | 1.049× | 5.680–5.857 | 5.424–5.577 |
| native-eager | pk_hit | 4.145 | 3.973 | 1.043× | 4.090–4.201 | 3.966–3.981 |
| native-eager | indexed_join | 147.538 | 141.746 | 1.041× | 145.496–149.580 | 141.447–142.045 |
| native-eager | three_table_join | 221.757 | 211.711 | 1.047× | 219.594–223.920 | 208.496–214.926 |
| native-eager | three_table_join_miss | 196.166 | 187.932 | 1.044× | 194.829–197.503 | 186.331–189.532 |
| native-eager | three_table_join_range | 1440.291 | 1366.937 | 1.054× | 1426.139–1454.444 | 1361.244–1372.630 |
| native-eager | three_table_computed_key | 245.041 | 236.631 | 1.036× | 242.342–247.741 | 235.472–237.790 |
| native-eager | four_table_join | 315.792 | 307.817 | 1.026× | 314.406–317.177 | 302.117–313.517 |
| native-eager | three_table_left_fence | 449.424 | 429.032 | 1.048× | 442.676–456.173 | 427.459–430.606 |
| native-eager | join_residual | 2042.597 | 893.567 | 2.286× | 2038.686–2046.509 | 875.331–911.804 |
| native-eager | join_text_predicate | 1519.529 | 168.972 | 8.993× | 1511.070–1527.988 | 167.319–170.625 |
| native-eager | join_text_projection | 978.290 | 144.706 | 6.761× | 976.029–980.552 | 144.249–145.163 |
| native-eager | join_integer_outputs | 1456.171 | 169.401 | 8.596× | 1454.820–1457.522 | 168.939–169.862 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.589 | 5.693 | 0.982× | 5.545–5.633 | 5.670–5.716 |
| native-prepared | pk_hit | 4.049 | 4.008 | 1.010× | 4.037–4.061 | 3.941–4.074 |
| native-prepared | indexed_join | 137.675 | 141.778 | 0.971× | 136.246–139.104 | 138.267–145.290 |
| native-prepared | three_table_join | 251.585 | 259.745 | 0.969× | 249.959–253.211 | 252.393–267.098 |
| native-prepared | three_table_join_miss | 182.835 | 183.869 | 0.994× | 182.438–183.232 | 179.598–188.139 |
| native-prepared | three_table_join_range | 1545.039 | 1583.904 | 0.975× | 1544.191–1545.886 | 1547.769–1620.040 |
| native-prepared | three_table_computed_key | 232.688 | 250.897 | 0.927× | 231.666–233.710 | 244.023–257.771 |
| native-prepared | four_table_join | 325.098 | 364.890 | 0.891× | 297.414–352.781 | 355.464–374.315 |
| native-prepared | three_table_left_fence | 439.531 | 449.352 | 0.978× | 436.409–442.654 | 437.213–461.491 |
| native-prepared | join_residual | 2372.668 | 1057.249 | 2.244× | 2369.621–2375.715 | 1024.899–1089.599 |
| native-prepared | join_text_predicate | 1269.918 | 176.020 | 7.215× | 1267.632–1272.203 | 171.175–180.864 |
| native-prepared | join_text_projection | 851.851 | 157.196 | 5.419× | 847.651–856.051 | 152.570–161.823 |
| native-prepared | join_integer_outputs | 1250.177 | 241.006 | 5.187× | 1232.063–1268.292 | 239.118–242.894 |
| native-query | scalar | 6.105 | 6.097 | 1.001× | 6.072–6.138 | 6.031–6.163 |
| native-query | pk_hit | 4.536 | 4.590 | 0.988× | 4.505–4.567 | 4.534–4.647 |
| native-query | indexed_join | 147.474 | 143.355 | 1.029× | 145.351–149.596 | 142.780–143.931 |
| native-query | three_table_join | 226.909 | 257.727 | 0.880× | 218.158–235.659 | 230.103–285.352 |
| native-query | three_table_join_miss | 193.822 | 195.629 | 0.991× | 188.392–199.252 | 193.100–198.157 |
| native-query | three_table_join_range | 1660.826 | 1648.494 | 1.007× | 1639.849–1681.803 | 1634.834–1662.153 |
| native-query | three_table_computed_key | 256.089 | 253.745 | 1.009× | 247.402–264.777 | 246.869–260.621 |
| native-query | four_table_join | 330.198 | 355.307 | 0.929× | 320.893–339.503 | 337.352–373.262 |
| native-query | three_table_left_fence | 493.573 | 465.176 | 1.061× | 476.738–510.409 | 462.052–468.300 |
| native-query | join_residual | 2456.174 | 1101.593 | 2.230× | 2448.982–2463.366 | 1099.234–1103.953 |
| native-query | join_text_predicate | 1363.185 | 176.210 | 7.736× | 1335.195–1391.175 | 174.776–177.645 |
| native-query | join_text_projection | 906.417 | 159.317 | 5.689× | 883.918–928.916 | 158.195–160.438 |
| native-query | join_integer_outputs | 1278.610 | 185.331 | 6.899× | 1271.198–1286.022 | 176.188–194.474 |
| native-eager | scalar | 6.084 | 5.968 | 1.019× | 5.833–6.335 | 5.767–6.168 |
| native-eager | pk_hit | 3.931 | 3.825 | 1.028× | 3.878–3.984 | 3.762–3.889 |
| native-eager | indexed_join | 131.277 | 135.332 | 0.970× | 131.099–131.455 | 133.262–137.401 |
| native-eager | three_table_join | 201.590 | 207.668 | 0.971× | 197.957–205.223 | 204.275–211.060 |
| native-eager | three_table_join_miss | 181.733 | 183.976 | 0.988× | 173.840–189.626 | 180.948–187.004 |
| native-eager | three_table_join_range | 1523.217 | 1504.479 | 1.012× | 1489.700–1556.734 | 1468.277–1540.681 |
| native-eager | three_table_computed_key | 230.080 | 233.860 | 0.984× | 222.338–237.822 | 232.296–235.424 |
| native-eager | four_table_join | 318.000 | 306.708 | 1.037× | 305.176–330.824 | 305.682–307.735 |
| native-eager | three_table_left_fence | 445.711 | 459.422 | 0.970× | 439.483–451.938 | 458.521–460.323 |
| native-eager | join_residual | 1880.137 | 932.160 | 2.017× | 1878.197–1882.077 | 891.381–972.940 |
| native-eager | join_text_predicate | 1294.125 | 162.912 | 7.944× | 1265.807–1322.442 | 160.123–165.701 |
| native-eager | join_text_projection | 876.947 | 140.664 | 6.234× | 851.688–902.206 | 138.169–143.160 |
| native-eager | join_integer_outputs | 1257.337 | 161.816 | 7.770× | 1243.132–1271.543 | 161.598–162.034 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | scalar | +4.123% | yes |
| 4.0-none | native-prepared | pk_hit | +4.130% | yes |
| 4.0-none | native-prepared | indexed_join | +1.351% | no |
| 4.0-none | native-prepared | three_table_join | +2.475% | no |
| 4.0-none | native-prepared | three_table_join_miss | +0.511% | no |
| 4.0-none | native-prepared | three_table_computed_key | +2.040% | yes |
| 4.0-none | native-eager | pk_hit | +2.416% | yes |
| 4.0-none | native-eager | indexed_join | +2.316% | no |
| 4.0-none | native-eager | three_table_join | +0.803% | no |
| 4.0-none | native-eager | three_table_join_miss | +0.387% | no |
| 4.0-none | native-eager | three_table_join_range | +0.519% | no |
| 4.0-none | native-eager | four_table_join | +1.136% | no |
| 4.0-yjit | native-prepared | three_table_join | +0.021% | no |
| 4.1-zjit | native-prepared | scalar | +1.853% | yes |
| 4.1-zjit | native-prepared | indexed_join | +2.980% | no |
| 4.1-zjit | native-prepared | three_table_join | +3.243% | no |
| 4.1-zjit | native-prepared | three_table_join_miss | +0.565% | no |
| 4.1-zjit | native-prepared | three_table_join_range | +2.516% | yes |
| 4.1-zjit | native-prepared | three_table_computed_key | +7.825% | yes |
| 4.1-zjit | native-prepared | four_table_join | +12.240% | yes |
| 4.1-zjit | native-prepared | three_table_left_fence | +2.234% | no |
| 4.1-zjit | native-query | pk_hit | +1.197% | no |
| 4.1-zjit | native-query | three_table_join | +13.582% | no |
| 4.1-zjit | native-query | three_table_join_miss | +0.932% | no |
| 4.1-zjit | native-query | four_table_join | +7.604% | no |
| 4.1-zjit | native-eager | indexed_join | +3.089% | yes |
| 4.1-zjit | native-eager | three_table_join | +3.015% | no |
| 4.1-zjit | native-eager | three_table_join_miss | +1.234% | no |
| 4.1-zjit | native-eager | three_table_computed_key | +1.643% | no |
| 4.1-zjit | native-eager | three_table_left_fence | +3.076% | yes |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.851 | 0.850 | 1.410 | 2.242 | 5.704× | 3.440× |
| pk_hit | 4.058 | 1.816 | 2.230 | 4.109 | 2.234× | 1.820× |
| indexed_join | 154.585 | 51.334 | 2.482 | 128.108 | 3.011× | 62.280× |
| three_table_join | 228.103 | 5.692 | 3.035 | 12.707 | 40.077× | 75.169× |
| three_table_join_miss | 210.321 | 1.102 | 1.474 | 3.845 | 190.941× | 142.678× |
| three_table_join_range | 1378.474 | 79.128 | 58.179 | 237.673 | 17.421× | 23.694× |
| three_table_computed_key | 247.198 | 6.608 | 5.789 | 15.598 | 37.406× | 42.703× |
| four_table_join | 320.611 | 4.195 | 3.928 | 11.917 | 76.435× | 81.612× |
| three_table_left_fence | 447.977 | 3.387 | 3.082 | 11.123 | 132.273× | 145.343× |
| join_residual | 991.458 | 410.689 | 744.100 | 1563.802 | 2.414× | 1.332× |
| join_text_predicate | 188.481 | 67.169 | 2.525 | 132.020 | 2.806× | 74.646× |
| join_text_projection | 163.305 | 82.024 | 2.403 | 114.560 | 1.991× | 67.970× |
| join_integer_outputs | 180.791 | 105.181 | 2.582 | 191.458 | 1.719× | 70.006× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.143 | 4.995 | 5.616 | 5.807 | 0.613× |
| pk_hit | 10.673 | 4.047 | 4.673 | 4.189 | 0.379× |
| indexed_join | 36.439 | 157.418 | 158.658 | 149.798 | 4.320× |
| three_table_join | 164.755 | 223.816 | 237.706 | 221.622 | 1.358× |
| three_table_join_miss | 166.228 | 201.012 | 203.591 | 198.252 | 1.209× |
| three_table_join_range | 245.691 | 1401.935 | 1415.008 | 1456.569 | 5.706× |
| three_table_computed_key | 130.818 | 256.847 | 254.050 | 248.405 | 1.963× |
| four_table_join | 413.694 | 322.901 | 322.799 | 317.701 | 0.781× |
| three_table_left_fence | 50.878 | 451.853 | 456.539 | 444.960 | 8.881× |
| join_residual | 900.774 | 1070.440 | 1061.139 | 894.745 | 1.188× |
| join_text_predicate | 43.786 | 191.029 | 184.700 | 177.273 | 4.363× |
| join_text_projection | 52.425 | 157.981 | 158.508 | 155.191 | 3.013× |
| join_integer_outputs | 64.443 | 191.114 | 187.053 | 173.598 | 2.966× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10559.800 | 10559.900 | +0.001% |
| three_table_computed_key | 887.000 | 887.000 | +0.000% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2196.800 | +0.000% |
| join_residual | 15784.200 | 5278.200 | -66.560% |
| join_text_predicate | 12738.000 | 618.000 | -95.148% |
| join_text_projection | 8579.000 | 591.000 | -93.111% |
| join_integer_outputs | 11820.000 | 648.000 | -94.518% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::Engine#compile | 1.333 | 1.333 |
| scalar | JedNative::PreparedJoins#prepared_join_root | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| scalar | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#compile | 1.000 | 1.000 |
| pk_hit | JedNative::PreparedJoins#prepared_join_root | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::Engine#compile | 19.667 | 19.667 |
| indexed_join | JedNative::PreparedJoins#prepared_join_root | 19.333 | 19.333 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::JoinPlanner#two_base_join_plan | 0.333 | 0.333 |
| three_table_join | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join | JedNative::PreparedJoins#prepared_join_root | 29.000 | 29.000 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_miss | JedNative::PreparedJoins#prepared_join_root | 29.000 | 29.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join_miss | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_range | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_range | JedNative::PreparedJoins#prepared_join_root | 55.000 | 55.000 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_computed_key | JedNative::Engine#compile | 49.000 | 49.000 |
| three_table_computed_key | JedNative::PreparedJoins#prepared_join_root | 30.000 | 30.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_computed_key | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::Engine#compile | 103.333 | 103.333 |
| four_table_join | JedNative::PreparedJoins#prepared_join_root | 55.333 | 55.333 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| four_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_left_fence | JedNative::Engine#compile | 22.000 | 22.000 |
| three_table_left_fence | JedNative::PreparedJoins#prepared_join_root | 36.000 | 36.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| join_residual | JedNative::Engine#compile | 220.000 | 30.000 |
| join_residual | JedNative::PreparedJoins#prepared_join_root | 0.000 | 19.333 |
| join_residual | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_residual | JedNative::JoinPlanner#two_base_join_plan | 1.000 | 0.333 |
| join_text_predicate | JedNative::Engine#compile | 223.000 | 25.667 |
| join_text_predicate | JedNative::PreparedJoins#prepared_join_root | 0.000 | 21.000 |
| join_text_predicate | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_predicate | JedNative::JoinPlanner#two_base_join_plan | 1.000 | 0.333 |
| join_text_projection | JedNative::Engine#compile | 134.000 | 0.000 |
| join_text_projection | JedNative::PreparedJoins#prepared_join_root | 0.000 | 11.000 |
| join_text_projection | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_projection | JedNative::JoinPlanner#two_base_join_plan | 1.000 | 0.000 |
| join_integer_outputs | JedNative::Engine#compile | 199.000 | 0.000 |
| join_integer_outputs | JedNative::PreparedJoins#prepared_join_root | 0.000 | 12.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_integer_outputs | JedNative::JoinPlanner#two_base_join_plan | 1.000 | 0.000 |
