# Indexed JOIN bound preparation measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.431 | 12.554 | 0.990× | 12.134–12.728 | 12.481–12.627 |
| native-prepared | pk_hit | 8.230 | 8.320 | 0.989× | 8.080–8.381 | 8.286–8.353 |
| native-prepared | indexed_join | 260.025 | 272.747 | 0.953× | 252.838–267.213 | 271.767–273.727 |
| native-prepared | three_table_join | 370.812 | 380.601 | 0.974× | 360.169–381.456 | 378.755–382.446 |
| native-prepared | three_table_join_miss | 303.448 | 311.508 | 0.974× | 298.084–308.812 | 308.080–314.937 |
| native-prepared | three_table_join_range | 2872.126 | 2643.521 | 1.086× | 2808.584–2935.669 | 2641.593–2645.448 |
| native-prepared | three_table_computed_key | 405.855 | 415.382 | 0.977× | 397.293–414.417 | 412.828–417.935 |
| native-prepared | four_table_join | 503.180 | 523.554 | 0.961× | 495.033–511.327 | 521.335–525.773 |
| native-prepared | three_table_left_fence | 755.661 | 778.718 | 0.970× | 738.395–772.927 | 776.842–780.595 |
| native-prepared | join_residual | 1965.053 | 1998.301 | 0.983× | 1924.343–2005.763 | 1982.453–2014.148 |
| native-prepared | join_text_predicate | 311.750 | 315.249 | 0.989× | 307.810–315.691 | 309.870–320.628 |
| native-prepared | join_text_projection | 268.863 | 277.355 | 0.969× | 264.813–272.912 | 276.721–277.990 |
| native-prepared | join_integer_outputs | 300.321 | 309.235 | 0.971× | 293.641–307.000 | 306.369–312.101 |
| native-query | scalar | 13.548 | 13.780 | 0.983× | 13.426–13.670 | 13.428–14.132 |
| native-query | pk_hit | 9.644 | 9.871 | 0.977× | 9.559–9.728 | 9.579–10.164 |
| native-query | indexed_join | 257.170 | 267.876 | 0.960× | 257.102–257.237 | 262.077–273.674 |
| native-query | three_table_join | 368.462 | 377.207 | 0.977× | 364.223–372.701 | 367.187–387.226 |
| native-query | three_table_join_miss | 297.973 | 304.485 | 0.979× | 297.598–298.348 | 295.790–313.179 |
| native-query | three_table_join_range | 2827.156 | 2575.859 | 1.098× | 2804.418–2849.894 | 2509.702–2642.016 |
| native-query | three_table_computed_key | 400.622 | 406.601 | 0.985× | 397.427–403.818 | 396.311–416.892 |
| native-query | four_table_join | 493.527 | 504.485 | 0.978× | 491.523–495.530 | 493.146–515.824 |
| native-query | three_table_left_fence | 743.548 | 763.315 | 0.974× | 737.754–749.342 | 752.544–774.086 |
| native-query | join_residual | 1905.254 | 1969.972 | 0.967× | 1892.483–1918.024 | 1947.649–1992.295 |
| native-query | join_text_predicate | 306.139 | 313.473 | 0.977× | 302.928–309.351 | 306.540–320.407 |
| native-query | join_text_projection | 265.617 | 276.638 | 0.960× | 262.518–268.717 | 267.586–285.690 |
| native-query | join_integer_outputs | 296.960 | 304.506 | 0.975× | 296.257–297.663 | 299.669–309.344 |
| native-eager | scalar | 15.349 | 15.692 | 0.978× | 15.329–15.369 | 15.328–16.055 |
| native-eager | pk_hit | 9.577 | 9.573 | 1.000× | 9.490–9.665 | 9.302–9.844 |
| native-eager | indexed_join | 225.835 | 230.583 | 0.979× | 225.272–226.399 | 225.398–235.768 |
| native-eager | three_table_join | 333.900 | 337.099 | 0.991× | 332.525–335.275 | 327.399–346.799 |
| native-eager | three_table_join_miss | 277.495 | 284.480 | 0.975× | 276.634–278.356 | 278.895–290.064 |
| native-eager | three_table_join_range | 2739.374 | 2491.235 | 1.100× | 2714.241–2764.507 | 2442.517–2539.953 |
| native-eager | three_table_computed_key | 359.114 | 368.099 | 0.976× | 354.464–363.763 | 362.613–373.585 |
| native-eager | four_table_join | 466.164 | 474.979 | 0.981× | 462.804–469.524 | 467.272–482.687 |
| native-eager | three_table_left_fence | 696.959 | 717.803 | 0.971× | 696.229–697.689 | 697.527–738.079 |
| native-eager | join_residual | 1615.527 | 1611.016 | 1.003× | 1609.273–1621.781 | 1564.111–1657.920 |
| native-eager | join_text_predicate | 269.774 | 273.710 | 0.986× | 269.655–269.893 | 265.926–281.494 |
| native-eager | join_text_projection | 229.855 | 240.142 | 0.957× | 229.717–229.993 | 238.929–241.354 |
| native-eager | join_integer_outputs | 266.881 | 273.635 | 0.975× | 260.426–273.336 | 270.595–276.675 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.072 | 5.021 | 1.010× | 5.051–5.094 | 4.995–5.048 |
| native-prepared | pk_hit | 4.125 | 4.082 | 1.011× | 4.113–4.137 | 4.002–4.162 |
| native-prepared | indexed_join | 156.483 | 154.542 | 1.013× | 156.023–156.943 | 151.021–158.064 |
| native-prepared | three_table_join | 226.817 | 223.063 | 1.017× | 226.248–227.386 | 222.186–223.939 |
| native-prepared | three_table_join_miss | 201.727 | 198.582 | 1.016× | 201.679–201.775 | 197.239–199.926 |
| native-prepared | three_table_join_range | 1419.999 | 1342.641 | 1.058× | 1414.891–1425.107 | 1337.050–1348.233 |
| native-prepared | three_table_computed_key | 250.190 | 255.416 | 0.980× | 248.824–251.556 | 253.854–256.979 |
| native-prepared | four_table_join | 324.595 | 321.965 | 1.008× | 324.064–325.126 | 320.412–323.518 |
| native-prepared | three_table_left_fence | 452.249 | 453.159 | 0.998× | 447.867–456.631 | 448.927–457.391 |
| native-prepared | join_residual | 1071.071 | 1064.771 | 1.006× | 1067.502–1074.639 | 1055.502–1074.039 |
| native-prepared | join_text_predicate | 184.952 | 186.918 | 0.989× | 184.460–185.444 | 186.514–187.322 |
| native-prepared | join_text_projection | 157.557 | 160.489 | 0.982× | 156.734–158.379 | 158.406–162.572 |
| native-prepared | join_integer_outputs | 188.509 | 184.886 | 1.020× | 186.394–190.623 | 182.234–187.538 |
| native-query | scalar | 5.484 | 5.319 | 1.031× | 5.358–5.610 | 5.271–5.368 |
| native-query | pk_hit | 4.601 | 4.430 | 1.039× | 4.537–4.665 | 4.406–4.454 |
| native-query | indexed_join | 156.906 | 149.749 | 1.048× | 152.833–160.979 | 148.090–151.407 |
| native-query | three_table_join | 229.372 | 219.386 | 1.046× | 226.486–232.258 | 216.791–221.982 |
| native-query | three_table_join_miss | 199.355 | 192.400 | 1.036× | 195.427–203.282 | 189.919–194.881 |
| native-query | three_table_join_range | 1395.664 | 1300.572 | 1.073× | 1380.540–1410.789 | 1288.053–1313.090 |
| native-query | three_table_computed_key | 250.460 | 246.998 | 1.014× | 247.953–252.967 | 246.441–247.555 |
| native-query | four_table_join | 318.350 | 320.206 | 0.994× | 311.825–324.875 | 316.299–324.113 |
| native-query | three_table_left_fence | 448.692 | 440.950 | 1.018× | 440.015–457.370 | 438.414–443.486 |
| native-query | join_residual | 1031.258 | 1028.859 | 1.002× | 1006.322–1056.194 | 1016.355–1041.363 |
| native-query | join_text_predicate | 182.709 | 180.732 | 1.011× | 178.854–186.564 | 180.175–181.289 |
| native-query | join_text_projection | 155.042 | 154.804 | 1.002× | 152.067–158.016 | 154.178–155.431 |
| native-query | join_integer_outputs | 185.316 | 177.546 | 1.044× | 179.079–191.552 | 177.390–177.703 |
| native-eager | scalar | 5.708 | 5.437 | 1.050× | 5.539–5.878 | 5.426–5.448 |
| native-eager | pk_hit | 4.007 | 3.966 | 1.010× | 3.950–4.064 | 3.941–3.990 |
| native-eager | indexed_join | 143.102 | 141.106 | 1.014× | 138.623–147.582 | 140.443–141.770 |
| native-eager | three_table_join | 223.387 | 212.008 | 1.054× | 218.372–228.402 | 210.025–213.991 |
| native-eager | three_table_join_miss | 197.780 | 186.216 | 1.062× | 195.099–200.461 | 186.083–186.348 |
| native-eager | three_table_join_range | 1411.238 | 1334.420 | 1.058× | 1354.518–1467.958 | 1328.652–1340.188 |
| native-eager | three_table_computed_key | 243.576 | 234.923 | 1.037× | 234.510–252.642 | 229.487–240.359 |
| native-eager | four_table_join | 312.509 | 304.382 | 1.027× | 299.552–325.467 | 304.079–304.685 |
| native-eager | three_table_left_fence | 441.580 | 429.542 | 1.028× | 423.372–459.789 | 423.701–435.383 |
| native-eager | join_residual | 892.767 | 908.900 | 0.982× | 884.814–900.720 | 891.951–925.848 |
| native-eager | join_text_predicate | 173.961 | 173.664 | 1.002× | 172.251–175.670 | 172.536–174.792 |
| native-eager | join_text_projection | 143.174 | 143.558 | 0.997× | 142.469–143.879 | 140.404–146.712 |
| native-eager | join_integer_outputs | 170.351 | 171.764 | 0.992× | 167.545–173.158 | 171.350–172.177 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.729 | 5.636 | 1.016× | 5.655–5.803 | 5.544–5.728 |
| native-prepared | pk_hit | 4.128 | 3.947 | 1.046× | 4.126–4.129 | 3.869–4.024 |
| native-prepared | indexed_join | 147.374 | 146.562 | 1.006× | 145.647–149.102 | 140.998–152.127 |
| native-prepared | three_table_join | 222.991 | 243.808 | 0.915× | 215.933–230.049 | 228.710–258.905 |
| native-prepared | three_table_join_miss | 207.399 | 184.068 | 1.127× | 196.960–217.837 | 182.253–185.884 |
| native-prepared | three_table_join_range | 1649.510 | 1472.874 | 1.120× | 1636.736–1662.283 | 1431.217–1514.532 |
| native-prepared | three_table_computed_key | 241.534 | 248.186 | 0.973× | 240.550–242.519 | 243.388–252.983 |
| native-prepared | four_table_join | 318.325 | 318.778 | 0.999× | 310.231–326.420 | 318.297–319.259 |
| native-prepared | three_table_left_fence | 475.173 | 453.427 | 1.048× | 472.575–477.771 | 446.286–460.568 |
| native-prepared | join_residual | 1079.132 | 1111.986 | 0.970× | 1072.068–1086.197 | 1097.420–1126.552 |
| native-prepared | join_text_predicate | 178.052 | 172.936 | 1.030× | 176.624–179.480 | 166.398–179.473 |
| native-prepared | join_text_projection | 162.315 | 148.839 | 1.091× | 162.226–162.404 | 142.001–155.677 |
| native-prepared | join_integer_outputs | 189.785 | 197.921 | 0.959× | 173.092–206.477 | 166.991–228.852 |
| native-query | scalar | 6.031 | 5.991 | 1.007× | 5.828–6.234 | 5.903–6.079 |
| native-query | pk_hit | 4.399 | 4.448 | 0.989× | 4.386–4.412 | 4.266–4.630 |
| native-query | indexed_join | 137.045 | 144.833 | 0.946× | 136.570–137.520 | 144.738–144.928 |
| native-query | three_table_join | 232.433 | 233.254 | 0.996× | 204.778–260.088 | 221.146–245.362 |
| native-query | three_table_join_miss | 187.385 | 184.614 | 1.015× | 182.520–192.250 | 179.773–189.455 |
| native-query | three_table_join_range | 1586.870 | 1519.758 | 1.044× | 1567.114–1606.625 | 1478.737–1560.780 |
| native-query | three_table_computed_key | 247.544 | 271.646 | 0.911× | 237.342–257.746 | 269.278–274.014 |
| native-query | four_table_join | 339.270 | 322.805 | 1.051× | 324.334–354.205 | 305.506–340.104 |
| native-query | three_table_left_fence | 447.016 | 473.332 | 0.944× | 442.014–452.019 | 451.724–494.940 |
| native-query | join_residual | 1027.607 | 1065.429 | 0.965× | 1017.680–1037.535 | 1061.751–1069.107 |
| native-query | join_text_predicate | 169.737 | 174.168 | 0.975× | 169.486–169.987 | 169.484–178.852 |
| native-query | join_text_projection | 150.625 | 149.431 | 1.008× | 150.017–151.233 | 146.012–152.849 |
| native-query | join_integer_outputs | 223.000 | 209.974 | 1.062× | 218.444–227.557 | 191.746–228.202 |
| native-eager | scalar | 5.826 | 5.865 | 0.993× | 5.728–5.925 | 5.846–5.884 |
| native-eager | pk_hit | 3.841 | 3.830 | 1.003× | 3.778–3.905 | 3.829–3.831 |
| native-eager | indexed_join | 133.374 | 130.086 | 1.025× | 132.974–133.773 | 128.934–131.237 |
| native-eager | three_table_join | 211.423 | 200.132 | 1.056× | 210.405–212.440 | 196.883–203.381 |
| native-eager | three_table_join_miss | 187.466 | 182.799 | 1.026× | 176.839–198.093 | 178.136–187.461 |
| native-eager | three_table_join_range | 1479.162 | 1383.911 | 1.069× | 1472.639–1485.684 | 1378.820–1389.001 |
| native-eager | three_table_computed_key | 235.805 | 255.431 | 0.923× | 235.333–236.276 | 225.245–285.617 |
| native-eager | four_table_join | 303.878 | 320.653 | 0.948× | 302.050–305.706 | 290.496–350.810 |
| native-eager | three_table_left_fence | 421.675 | 434.497 | 0.970× | 421.256–422.094 | 428.223–440.770 |
| native-eager | join_residual | 926.541 | 914.722 | 1.013× | 926.021–927.062 | 892.628–936.815 |
| native-eager | join_text_predicate | 161.756 | 164.542 | 0.983× | 157.415–166.098 | 162.201–166.882 |
| native-eager | join_text_projection | 134.404 | 136.836 | 0.982× | 131.804–137.004 | 132.513–141.158 |
| native-eager | join_integer_outputs | 157.147 | 177.484 | 0.885× | 156.854–157.439 | 165.470–189.497 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | scalar | +0.990% | no |
| 4.0-none | native-prepared | pk_hit | +1.086% | no |
| 4.0-none | native-prepared | indexed_join | +4.893% | yes |
| 4.0-none | native-prepared | three_table_join | +2.640% | no |
| 4.0-none | native-prepared | three_table_join_miss | +2.656% | no |
| 4.0-none | native-prepared | three_table_computed_key | +2.347% | no |
| 4.0-none | native-prepared | four_table_join | +4.049% | yes |
| 4.0-none | native-prepared | three_table_left_fence | +3.051% | yes |
| 4.0-none | native-prepared | join_residual | +1.692% | no |
| 4.0-none | native-prepared | join_text_predicate | +1.122% | no |
| 4.0-none | native-prepared | join_text_projection | +3.159% | yes |
| 4.0-none | native-prepared | join_integer_outputs | +2.968% | no |
| 4.0-none | native-query | scalar | +1.711% | no |
| 4.0-none | native-query | pk_hit | +2.359% | no |
| 4.0-none | native-query | indexed_join | +4.163% | yes |
| 4.0-none | native-query | three_table_join | +2.373% | no |
| 4.0-none | native-query | three_table_join_miss | +2.185% | no |
| 4.0-none | native-query | three_table_computed_key | +1.492% | no |
| 4.0-none | native-query | four_table_join | +2.220% | no |
| 4.0-none | native-query | three_table_left_fence | +2.658% | yes |
| 4.0-none | native-query | join_residual | +3.397% | yes |
| 4.0-none | native-query | join_text_predicate | +2.396% | no |
| 4.0-none | native-query | join_text_projection | +4.149% | no |
| 4.0-none | native-query | join_integer_outputs | +2.541% | yes |
| 4.0-none | native-eager | scalar | +2.234% | no |
| 4.0-none | native-eager | indexed_join | +2.102% | no |
| 4.0-none | native-eager | three_table_join | +0.958% | no |
| 4.0-none | native-eager | three_table_join_miss | +2.517% | yes |
| 4.0-none | native-eager | three_table_computed_key | +2.502% | no |
| 4.0-none | native-eager | four_table_join | +1.891% | no |
| 4.0-none | native-eager | three_table_left_fence | +2.991% | no |
| 4.0-none | native-eager | join_text_predicate | +1.459% | no |
| 4.0-none | native-eager | join_text_projection | +4.475% | yes |
| 4.0-none | native-eager | join_integer_outputs | +2.531% | no |
| 4.0-yjit | native-prepared | three_table_computed_key | +2.089% | yes |
| 4.0-yjit | native-prepared | three_table_left_fence | +0.201% | no |
| 4.0-yjit | native-prepared | join_text_predicate | +1.063% | yes |
| 4.0-yjit | native-prepared | join_text_projection | +1.861% | yes |
| 4.0-yjit | native-query | four_table_join | +0.583% | no |
| 4.0-yjit | native-eager | join_residual | +1.807% | no |
| 4.0-yjit | native-eager | join_text_projection | +0.268% | no |
| 4.0-yjit | native-eager | join_integer_outputs | +0.829% | no |
| 4.1-zjit | native-prepared | three_table_join | +9.335% | no |
| 4.1-zjit | native-prepared | three_table_computed_key | +2.754% | yes |
| 4.1-zjit | native-prepared | four_table_join | +0.142% | no |
| 4.1-zjit | native-prepared | join_residual | +3.044% | yes |
| 4.1-zjit | native-prepared | join_integer_outputs | +4.287% | no |
| 4.1-zjit | native-query | pk_hit | +1.117% | no |
| 4.1-zjit | native-query | indexed_join | +5.683% | yes |
| 4.1-zjit | native-query | three_table_join | +0.353% | no |
| 4.1-zjit | native-query | three_table_computed_key | +9.736% | yes |
| 4.1-zjit | native-query | three_table_left_fence | +5.887% | no |
| 4.1-zjit | native-query | join_residual | +3.681% | yes |
| 4.1-zjit | native-query | join_text_predicate | +2.611% | no |
| 4.1-zjit | native-eager | scalar | +0.665% | no |
| 4.1-zjit | native-eager | three_table_computed_key | +8.323% | no |
| 4.1-zjit | native-eager | four_table_join | +5.520% | no |
| 4.1-zjit | native-eager | three_table_left_fence | +3.041% | yes |
| 4.1-zjit | native-eager | join_text_predicate | +1.722% | no |
| 4.1-zjit | native-eager | join_text_projection | +1.809% | no |
| 4.1-zjit | native-eager | join_integer_outputs | +12.941% | yes |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.646 | 0.840 | 1.435 | 2.108 | 5.529× | 3.236× |
| pk_hit | 3.885 | 1.683 | 2.240 | 3.915 | 2.308× | 1.735× |
| indexed_join | 162.985 | 51.877 | 5.249 | 122.808 | 3.142× | 31.051× |
| three_table_join | 228.945 | 3.910 | 3.072 | 12.670 | 58.554× | 74.515× |
| three_table_join_miss | 191.239 | 1.058 | 1.487 | 3.605 | 180.713× | 128.625× |
| three_table_join_range | 1293.865 | 79.567 | 58.261 | 212.986 | 16.261× | 22.208× |
| three_table_computed_key | 244.500 | 6.578 | 5.905 | 16.114 | 37.169× | 41.403× |
| four_table_join | 309.542 | 4.478 | 3.911 | 11.485 | 69.122× | 79.155× |
| three_table_left_fence | 443.505 | 3.457 | 3.076 | 10.330 | 128.305× | 144.192× |
| join_residual | 979.886 | 387.440 | 739.627 | 1508.608 | 2.529× | 1.325× |
| join_text_predicate | 178.057 | 64.103 | 2.519 | 129.752 | 2.778× | 70.673× |
| join_text_projection | 157.255 | 73.868 | 2.386 | 136.836 | 2.129× | 65.894× |
| join_integer_outputs | 180.127 | 102.554 | 2.581 | 177.188 | 1.756× | 69.791× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 7.767 | 5.004 | 5.268 | 5.406 | 0.644× |
| pk_hit | 10.159 | 4.096 | 4.413 | 4.094 | 0.403× |
| indexed_join | 34.958 | 159.795 | 149.394 | 141.663 | 4.571× |
| three_table_join | 150.797 | 232.515 | 219.388 | 212.696 | 1.542× |
| three_table_join_miss | 165.217 | 204.065 | 190.062 | 187.657 | 1.235× |
| three_table_join_range | 232.228 | 1351.624 | 1288.086 | 1347.465 | 5.820× |
| three_table_computed_key | 134.555 | 255.455 | 260.168 | 235.076 | 1.899× |
| four_table_join | 399.345 | 323.545 | 315.392 | 304.155 | 0.810× |
| three_table_left_fence | 47.703 | 451.254 | 434.998 | 442.042 | 9.460× |
| join_residual | 860.996 | 1053.701 | 1052.265 | 915.077 | 1.224× |
| join_text_predicate | 41.533 | 190.078 | 179.767 | 168.268 | 4.577× |
| join_text_projection | 50.113 | 161.206 | 154.095 | 142.917 | 3.217× |
| join_integer_outputs | 60.817 | 189.039 | 182.106 | 169.988 | 3.108× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 10559.900 | 9453.100 | -10.481% |
| three_table_computed_key | 887.000 | 887.000 | +0.000% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2196.800 | +0.000% |
| join_residual | 5278.200 | 5278.200 | +0.000% |
| join_text_predicate | 618.000 | 618.000 | +0.000% |
| join_text_projection | 591.000 | 591.000 | +0.000% |
| join_integer_outputs | 648.000 | 648.000 | +0.000% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::IndexScans#key_comparisons | 0.000 | 0.000 |
| scalar | JedNative::IndexNestedLoops#join_index_relation | 0.000 | 0.000 |
| scalar | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| scalar | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| scalar | JedNative::ExpressionWalks#same_key_expression? | 0.000 | 0.000 |
| scalar | JedNative::ExpressionWalks#append_conjunctions | 0.000 | 0.000 |
| scalar | JedNative::Engine#compile | 1.333 | 1.333 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| scalar | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::IndexScans#key_comparisons | 0.667 | 0.667 |
| pk_hit | JedNative::IndexNestedLoops#join_index_relation | 0.000 | 0.000 |
| pk_hit | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| pk_hit | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| pk_hit | JedNative::ExpressionWalks#same_key_expression? | 1.333 | 1.333 |
| pk_hit | JedNative::ExpressionWalks#append_conjunctions | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#compile | 1.000 | 1.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::IndexScans#key_comparisons | 9.333 | 9.333 |
| indexed_join | JedNative::IndexNestedLoops#join_index_relation | 1.000 | 1.000 |
| indexed_join | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| indexed_join | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| indexed_join | JedNative::ExpressionWalks#same_key_expression? | 20.000 | 20.000 |
| indexed_join | JedNative::ExpressionWalks#append_conjunctions | 55.333 | 55.333 |
| indexed_join | JedNative::Engine#compile | 19.667 | 19.667 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::JoinPlanner#two_base_join_plan | 0.333 | 0.333 |
| three_table_join | JedNative::IndexScans#key_comparisons | 15.667 | 15.667 |
| three_table_join | JedNative::IndexNestedLoops#join_index_relation | 2.000 | 2.000 |
| three_table_join | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| three_table_join | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| three_table_join | JedNative::ExpressionWalks#same_key_expression? | 44.667 | 44.667 |
| three_table_join | JedNative::ExpressionWalks#append_conjunctions | 87.000 | 87.000 |
| three_table_join | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_miss | JedNative::IndexScans#key_comparisons | 11.667 | 11.667 |
| three_table_join_miss | JedNative::IndexNestedLoops#join_index_relation | 0.000 | 0.000 |
| three_table_join_miss | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| three_table_join_miss | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| three_table_join_miss | JedNative::ExpressionWalks#same_key_expression? | 28.667 | 28.667 |
| three_table_join_miss | JedNative::ExpressionWalks#append_conjunctions | 75.000 | 75.000 |
| three_table_join_miss | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join_miss | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_join_range | JedNative::IndexScans#key_comparisons | 101.000 | 38.333 |
| three_table_join_range | JedNative::IndexNestedLoops#join_index_relation | 34.000 | 34.000 |
| three_table_join_range | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 1.333 |
| three_table_join_range | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 65.333 |
| three_table_join_range | JedNative::ExpressionWalks#same_key_expression? | 354.000 | 103.333 |
| three_table_join_range | JedNative::ExpressionWalks#append_conjunctions | 419.000 | 325.000 |
| three_table_join_range | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_computed_key | JedNative::IndexScans#key_comparisons | 14.667 | 14.667 |
| three_table_computed_key | JedNative::IndexNestedLoops#join_index_relation | 1.000 | 1.000 |
| three_table_computed_key | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| three_table_computed_key | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| three_table_computed_key | JedNative::ExpressionWalks#same_key_expression? | 38.667 | 38.667 |
| three_table_computed_key | JedNative::ExpressionWalks#append_conjunctions | 60.333 | 60.333 |
| three_table_computed_key | JedNative::Engine#compile | 49.000 | 49.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_computed_key | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| four_table_join | JedNative::IndexScans#key_comparisons | 36.333 | 36.333 |
| four_table_join | JedNative::IndexNestedLoops#join_index_relation | 3.000 | 3.000 |
| four_table_join | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| four_table_join | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| four_table_join | JedNative::ExpressionWalks#same_key_expression? | 100.667 | 100.667 |
| four_table_join | JedNative::ExpressionWalks#append_conjunctions | 233.000 | 233.000 |
| four_table_join | JedNative::Engine#compile | 103.333 | 103.333 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| four_table_join | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| three_table_left_fence | JedNative::IndexScans#key_comparisons | 19.000 | 19.000 |
| three_table_left_fence | JedNative::IndexNestedLoops#join_index_relation | 2.000 | 2.000 |
| three_table_left_fence | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| three_table_left_fence | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| three_table_left_fence | JedNative::ExpressionWalks#same_key_expression? | 42.000 | 42.000 |
| three_table_left_fence | JedNative::ExpressionWalks#append_conjunctions | 87.000 | 87.000 |
| three_table_left_fence | JedNative::Engine#compile | 22.000 | 22.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| join_residual | JedNative::IndexScans#key_comparisons | 16.667 | 16.667 |
| join_residual | JedNative::IndexNestedLoops#join_index_relation | 0.000 | 0.000 |
| join_residual | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| join_residual | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| join_residual | JedNative::ExpressionWalks#same_key_expression? | 49.333 | 49.333 |
| join_residual | JedNative::ExpressionWalks#append_conjunctions | 67.000 | 67.000 |
| join_residual | JedNative::Engine#compile | 30.000 | 30.000 |
| join_residual | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_residual | JedNative::JoinPlanner#two_base_join_plan | 0.333 | 0.333 |
| join_text_predicate | JedNative::IndexScans#key_comparisons | 18.000 | 18.000 |
| join_text_predicate | JedNative::IndexNestedLoops#join_index_relation | 1.000 | 1.000 |
| join_text_predicate | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| join_text_predicate | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| join_text_predicate | JedNative::ExpressionWalks#same_key_expression? | 60.667 | 60.667 |
| join_text_predicate | JedNative::ExpressionWalks#append_conjunctions | 173.333 | 173.333 |
| join_text_predicate | JedNative::Engine#compile | 25.667 | 25.667 |
| join_text_predicate | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_predicate | JedNative::JoinPlanner#two_base_join_plan | 0.333 | 0.333 |
| join_text_projection | JedNative::IndexScans#key_comparisons | 3.000 | 3.000 |
| join_text_projection | JedNative::IndexNestedLoops#join_index_relation | 1.000 | 1.000 |
| join_text_projection | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| join_text_projection | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| join_text_projection | JedNative::ExpressionWalks#same_key_expression? | 12.000 | 12.000 |
| join_text_projection | JedNative::ExpressionWalks#append_conjunctions | 13.000 | 13.000 |
| join_text_projection | JedNative::Engine#compile | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_projection | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
| join_integer_outputs | JedNative::IndexScans#key_comparisons | 3.000 | 3.000 |
| join_integer_outputs | JedNative::IndexNestedLoops#join_index_relation | 1.000 | 1.000 |
| join_integer_outputs | JedNative::IndexNestedLoops#prepare_join_bound_structure | 0.000 | 0.000 |
| join_integer_outputs | JedNative::IndexNestedLoops#join_bound_comparisons | 0.000 | 0.000 |
| join_integer_outputs | JedNative::ExpressionWalks#same_key_expression? | 14.000 | 14.000 |
| join_integer_outputs | JedNative::ExpressionWalks#append_conjunctions | 19.000 | 19.000 |
| join_integer_outputs | JedNative::Engine#compile | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_integer_outputs | JedNative::JoinPlanner#two_base_join_plan | 0.000 | 0.000 |
