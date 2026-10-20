# N-way frontier candidate preparation measurements

Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.

## 4.0-none

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 12.470 | 12.675 | 0.984× | 12.145–12.795 | 12.474–12.876 |
| native-prepared | pk_hit | 8.308 | 8.537 | 0.973× | 8.011–8.605 | 8.508–8.567 |
| native-prepared | indexed_join | 262.445 | 268.256 | 0.978× | 256.376–268.514 | 266.169–270.344 |
| native-prepared | three_table_join | 372.949 | 380.511 | 0.980× | 365.964–379.935 | 379.803–381.220 |
| native-prepared | three_table_join_miss | 303.294 | 314.112 | 0.966× | 300.751–305.836 | 313.352–314.873 |
| native-prepared | three_table_join_range | 2580.722 | 2633.430 | 0.980× | 2521.226–2640.218 | 2614.791–2652.068 |
| native-prepared | three_table_computed_key | 403.111 | 416.094 | 0.969× | 395.086–411.137 | 413.416–418.772 |
| native-prepared | four_table_join | 506.667 | 516.042 | 0.982× | 492.502–520.832 | 515.214–516.871 |
| native-prepared | three_table_left_fence | 756.248 | 771.038 | 0.981× | 746.342–766.153 | 763.487–778.589 |
| native-prepared | join_residual | 1963.864 | 1996.368 | 0.984× | 1941.809–1985.920 | 1985.957–2006.779 |
| native-prepared | join_text_predicate | 311.015 | 316.433 | 0.983× | 305.504–316.526 | 316.200–316.667 |
| native-prepared | join_text_projection | 272.678 | 276.759 | 0.985× | 265.781–279.575 | 275.051–278.467 |
| native-prepared | join_integer_outputs | 304.447 | 310.508 | 0.980× | 300.392–308.502 | 309.078–311.937 |
| native-prepared | five_table_bounded | 12238.901 | 12790.099 | 0.957× | 12082.658–12395.143 | 12375.299–13204.900 |
| native-query | scalar | 13.530 | 13.967 | 0.969× | 13.495–13.565 | 13.845–14.089 |
| native-query | pk_hit | 9.578 | 9.859 | 0.972× | 9.570–9.586 | 9.635–10.084 |
| native-query | indexed_join | 257.162 | 267.023 | 0.963× | 253.738–260.586 | 265.072–268.974 |
| native-query | three_table_join | 368.798 | 374.039 | 0.986× | 361.208–376.388 | 367.840–380.238 |
| native-query | three_table_join_miss | 296.926 | 303.884 | 0.977× | 296.049–297.804 | 295.526–312.242 |
| native-query | three_table_join_range | 2510.593 | 2603.380 | 0.964× | 2500.339–2520.848 | 2531.515–2675.245 |
| native-query | three_table_computed_key | 403.142 | 413.009 | 0.976× | 395.213–411.071 | 404.595–421.423 |
| native-query | four_table_join | 488.920 | 508.891 | 0.961× | 486.397–491.444 | 491.439–526.343 |
| native-query | three_table_left_fence | 741.548 | 755.620 | 0.981× | 735.318–747.779 | 741.501–769.740 |
| native-query | join_residual | 1901.094 | 1960.318 | 0.970× | 1891.007–1911.180 | 1901.797–2018.839 |
| native-query | join_text_predicate | 307.902 | 316.605 | 0.973× | 306.169–309.635 | 305.709–327.501 |
| native-query | join_text_projection | 267.407 | 275.206 | 0.972× | 264.061–270.752 | 265.010–285.402 |
| native-query | join_integer_outputs | 298.565 | 303.493 | 0.984× | 298.510–298.621 | 294.101–312.886 |
| native-query | five_table_bounded | 11964.216 | 12193.250 | 0.981× | 11943.144–11985.288 | 11969.614–12416.885 |
| native-eager | scalar | 15.688 | 16.032 | 0.979× | 15.279–16.096 | 15.936–16.129 |
| native-eager | pk_hit | 9.699 | 9.887 | 0.981× | 9.400–9.998 | 9.752–10.021 |
| native-eager | indexed_join | 228.068 | 233.026 | 0.979× | 220.221–235.914 | 231.245–234.806 |
| native-eager | three_table_join | 340.922 | 347.232 | 0.982× | 332.121–349.723 | 342.651–351.814 |
| native-eager | three_table_join_miss | 283.749 | 286.755 | 0.990× | 281.452–286.047 | 286.483–287.027 |
| native-eager | three_table_join_range | 2500.007 | 2508.939 | 0.996× | 2476.869–2523.145 | 2506.751–2511.127 |
| native-eager | three_table_computed_key | 367.820 | 380.653 | 0.966× | 354.404–381.237 | 374.899–386.407 |
| native-eager | four_table_join | 474.287 | 481.623 | 0.985× | 465.540–483.034 | 474.440–488.806 |
| native-eager | three_table_left_fence | 712.916 | 725.555 | 0.983× | 686.255–739.577 | 724.017–727.093 |
| native-eager | join_residual | 1635.269 | 1642.523 | 0.996× | 1633.061–1637.477 | 1637.438–1647.609 |
| native-eager | join_text_predicate | 274.534 | 280.792 | 0.978× | 269.972–279.096 | 278.708–282.876 |
| native-eager | join_text_projection | 241.571 | 243.252 | 0.993× | 237.646–245.495 | 242.753–243.752 |
| native-eager | join_integer_outputs | 268.589 | 274.603 | 0.978× | 261.666–275.512 | 272.860–276.347 |
| native-eager | five_table_bounded | 12427.924 | 12283.324 | 1.012× | 12356.234–12499.613 | 12218.225–12348.422 |

## 4.0-yjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 4.933 | 5.046 | 0.978× | 4.814–5.052 | 5.004–5.087 |
| native-prepared | pk_hit | 4.018 | 4.114 | 0.977× | 3.960–4.076 | 4.107–4.122 |
| native-prepared | indexed_join | 153.522 | 152.760 | 1.005× | 150.150–156.895 | 149.729–155.791 |
| native-prepared | three_table_join | 224.183 | 228.558 | 0.981× | 218.102–230.264 | 227.137–229.978 |
| native-prepared | three_table_join_miss | 194.727 | 198.103 | 0.983× | 189.256–200.197 | 195.529–200.677 |
| native-prepared | three_table_join_range | 1315.706 | 1332.904 | 0.987× | 1286.064–1345.347 | 1318.891–1346.916 |
| native-prepared | three_table_computed_key | 248.757 | 248.228 | 1.002× | 244.607–252.907 | 242.442–254.013 |
| native-prepared | four_table_join | 320.364 | 318.375 | 1.006× | 319.078–321.649 | 307.549–329.202 |
| native-prepared | three_table_left_fence | 444.486 | 444.014 | 1.001× | 435.658–453.313 | 435.490–452.538 |
| native-prepared | join_residual | 1045.047 | 1053.799 | 0.992× | 1028.384–1061.709 | 1037.855–1069.743 |
| native-prepared | join_text_predicate | 181.090 | 184.040 | 0.984× | 177.950–184.229 | 177.704–190.375 |
| native-prepared | join_text_projection | 153.474 | 156.309 | 0.982× | 151.805–155.143 | 148.494–164.123 |
| native-prepared | join_integer_outputs | 181.307 | 180.544 | 1.004× | 180.720–181.894 | 176.729–184.358 |
| native-prepared | five_table_bounded | 6746.464 | 6675.449 | 1.011× | 6599.616–6893.312 | 6499.051–6851.846 |
| native-query | scalar | 5.367 | 5.299 | 1.013× | 5.277–5.457 | 5.242–5.355 |
| native-query | pk_hit | 4.414 | 4.481 | 0.985× | 4.386–4.443 | 4.437–4.525 |
| native-query | indexed_join | 151.285 | 150.646 | 1.004× | 150.756–151.814 | 149.228–152.065 |
| native-query | three_table_join | 223.178 | 219.228 | 1.018× | 220.311–226.045 | 218.227–220.230 |
| native-query | three_table_join_miss | 194.113 | 194.448 | 0.998× | 189.874–198.351 | 193.703–195.192 |
| native-query | three_table_join_range | 1290.271 | 1300.564 | 0.992× | 1273.542–1307.001 | 1291.223–1309.904 |
| native-query | three_table_computed_key | 248.268 | 246.890 | 1.006× | 243.029–253.508 | 245.959–247.821 |
| native-query | four_table_join | 309.747 | 312.594 | 0.991× | 305.139–314.354 | 307.113–318.075 |
| native-query | three_table_left_fence | 436.460 | 437.115 | 0.999× | 432.929–439.991 | 434.139–440.091 |
| native-query | join_residual | 1026.092 | 1029.201 | 0.997× | 1019.174–1033.011 | 1020.650–1037.751 |
| native-query | join_text_predicate | 177.547 | 178.028 | 0.997× | 177.362–177.732 | 175.258–180.798 |
| native-query | join_text_projection | 153.156 | 155.982 | 0.982× | 152.758–153.555 | 154.822–157.141 |
| native-query | join_integer_outputs | 179.043 | 180.028 | 0.995× | 178.867–179.220 | 179.251–180.804 |
| native-query | five_table_bounded | 6643.903 | 6566.726 | 1.012× | 6634.198–6653.608 | 6526.487–6606.964 |
| native-eager | scalar | 5.643 | 5.678 | 0.994× | 5.558–5.729 | 5.544–5.811 |
| native-eager | pk_hit | 4.250 | 4.179 | 1.017× | 4.216–4.284 | 4.136–4.222 |
| native-eager | indexed_join | 147.868 | 146.888 | 1.007× | 146.473–149.264 | 145.877–147.898 |
| native-eager | three_table_join | 213.856 | 215.575 | 0.992× | 208.705–219.007 | 214.536–216.613 |
| native-eager | three_table_join_miss | 192.230 | 193.750 | 0.992× | 188.379–196.080 | 191.962–195.539 |
| native-eager | three_table_join_range | 1362.245 | 1342.113 | 1.015× | 1349.718–1374.773 | 1316.446–1367.779 |
| native-eager | three_table_computed_key | 241.434 | 241.125 | 1.001× | 233.233–249.635 | 233.379–248.872 |
| native-eager | four_table_join | 310.265 | 311.947 | 0.995× | 304.026–316.504 | 305.224–318.671 |
| native-eager | three_table_left_fence | 442.643 | 444.410 | 0.996× | 434.616–450.670 | 431.092–457.728 |
| native-eager | join_residual | 913.116 | 898.892 | 1.016× | 894.629–931.602 | 879.276–918.509 |
| native-eager | join_text_predicate | 173.357 | 174.188 | 0.995× | 169.611–177.104 | 172.102–176.273 |
| native-eager | join_text_projection | 147.776 | 150.548 | 0.982× | 146.045–149.507 | 144.784–156.311 |
| native-eager | join_integer_outputs | 174.262 | 174.253 | 1.000× | 172.022–176.502 | 167.240–181.265 |
| native-eager | five_table_bounded | 7107.551 | 7049.596 | 1.008× | 6977.407–7237.696 | 6928.859–7170.332 |

## 4.1-zjit

| API | Workload | Before | Final | Speedup | Before process range | Final process range |
|---|---|---:|---:|---:|---|---|
| native-prepared | scalar | 5.730 | 5.593 | 1.025× | 5.689–5.771 | 5.548–5.637 |
| native-prepared | pk_hit | 4.199 | 4.014 | 1.046× | 4.184–4.215 | 3.959–4.068 |
| native-prepared | indexed_join | 144.942 | 145.063 | 0.999× | 144.531–145.354 | 144.084–146.043 |
| native-prepared | three_table_join | 250.390 | 249.871 | 1.002× | 227.801–272.978 | 229.717–270.026 |
| native-prepared | three_table_join_miss | 189.429 | 185.931 | 1.019× | 186.483–192.374 | 180.482–191.380 |
| native-prepared | three_table_join_range | 1504.319 | 1491.866 | 1.008× | 1475.510–1533.128 | 1449.796–1533.937 |
| native-prepared | three_table_computed_key | 244.970 | 251.236 | 0.975× | 241.427–248.514 | 249.949–252.523 |
| native-prepared | four_table_join | 320.253 | 334.117 | 0.959× | 319.492–321.015 | 329.806–338.427 |
| native-prepared | three_table_left_fence | 455.057 | 467.728 | 0.973× | 450.239–459.875 | 463.169–472.286 |
| native-prepared | join_residual | 1071.381 | 1091.018 | 0.982× | 1040.631–1102.132 | 1072.038–1109.999 |
| native-prepared | join_text_predicate | 172.992 | 174.535 | 0.991× | 170.093–175.891 | 173.085–175.985 |
| native-prepared | join_text_projection | 142.912 | 153.560 | 0.931× | 140.486–145.338 | 147.328–159.792 |
| native-prepared | join_integer_outputs | 193.955 | 197.988 | 0.980× | 181.127–206.783 | 172.529–223.446 |
| native-prepared | five_table_bounded | 6596.228 | 6302.552 | 1.047× | 6351.659–6840.798 | 6185.561–6419.544 |
| native-query | scalar | 5.978 | 6.016 | 0.994× | 5.839–6.117 | 5.969–6.062 |
| native-query | pk_hit | 4.471 | 4.459 | 1.003× | 4.347–4.595 | 4.435–4.482 |
| native-query | indexed_join | 142.111 | 147.046 | 0.966× | 141.626–142.597 | 146.045–148.048 |
| native-query | three_table_join | 221.006 | 230.281 | 0.960× | 213.515–228.498 | 226.992–233.570 |
| native-query | three_table_join_miss | 213.521 | 181.921 | 1.174× | 198.172–228.870 | 181.099–182.743 |
| native-query | three_table_join_range | 1486.305 | 1453.921 | 1.022× | 1458.744–1513.866 | 1450.598–1457.243 |
| native-query | three_table_computed_key | 249.457 | 254.632 | 0.980× | 247.294–251.620 | 249.057–260.206 |
| native-query | four_table_join | 351.695 | 317.983 | 1.106× | 337.746–365.644 | 307.502–328.464 |
| native-query | three_table_left_fence | 463.648 | 460.018 | 1.008× | 454.865–472.431 | 458.301–461.736 |
| native-query | join_residual | 1103.962 | 1061.210 | 1.040× | 1100.656–1107.269 | 1042.576–1079.843 |
| native-query | join_text_predicate | 175.806 | 171.139 | 1.027× | 175.698–175.914 | 170.191–172.088 |
| native-query | join_text_projection | 165.861 | 157.927 | 1.050× | 161.888–169.834 | 156.112–159.743 |
| native-query | join_integer_outputs | 191.883 | 204.508 | 0.938× | 175.526–208.241 | 173.873–235.143 |
| native-query | five_table_bounded | 6740.248 | 6618.112 | 1.018× | 6484.592–6995.905 | 6603.748–6632.477 |
| native-eager | scalar | 6.092 | 6.232 | 0.978× | 6.070–6.114 | 6.048–6.415 |
| native-eager | pk_hit | 3.877 | 3.897 | 0.995× | 3.754–3.999 | 3.880–3.913 |
| native-eager | indexed_join | 128.968 | 133.574 | 0.966× | 126.716–131.219 | 131.702–135.445 |
| native-eager | three_table_join | 216.161 | 219.553 | 0.985× | 215.146–217.176 | 217.833–221.273 |
| native-eager | three_table_join_miss | 181.265 | 184.202 | 0.984× | 178.674–183.856 | 179.735–188.668 |
| native-eager | three_table_join_range | 1434.532 | 1454.485 | 0.986× | 1396.555–1472.509 | 1449.557–1459.413 |
| native-eager | three_table_computed_key | 227.678 | 243.840 | 0.934× | 224.143–231.213 | 238.396–249.284 |
| native-eager | four_table_join | 303.054 | 309.022 | 0.981× | 301.021–305.086 | 303.743–314.301 |
| native-eager | three_table_left_fence | 471.907 | 449.950 | 1.049× | 444.510–499.304 | 439.883–460.016 |
| native-eager | join_residual | 938.558 | 925.195 | 1.014× | 931.930–945.187 | 913.600–936.790 |
| native-eager | join_text_predicate | 162.785 | 163.344 | 0.997× | 161.146–164.424 | 159.306–167.382 |
| native-eager | join_text_projection | 145.143 | 154.785 | 0.938× | 136.049–154.237 | 152.976–156.593 |
| native-eager | join_integer_outputs | 160.732 | 161.766 | 0.994× | 155.705–165.758 | 155.886–167.646 |
| native-eager | five_table_bounded | 6466.292 | 6351.979 | 1.018× | 6368.929–6563.655 | 6245.018–6458.939 |

## Slower observations

| Runtime | API | Workload | Elapsed change | Both finals above both befores |
|---|---|---|---:|---|
| 4.0-none | native-prepared | scalar | +1.645% | no |
| 4.0-none | native-prepared | pk_hit | +2.760% | no |
| 4.0-none | native-prepared | indexed_join | +2.214% | no |
| 4.0-none | native-prepared | three_table_join | +2.028% | no |
| 4.0-none | native-prepared | three_table_join_miss | +3.567% | yes |
| 4.0-none | native-prepared | three_table_join_range | +2.042% | no |
| 4.0-none | native-prepared | three_table_computed_key | +3.221% | yes |
| 4.0-none | native-prepared | four_table_join | +1.850% | no |
| 4.0-none | native-prepared | three_table_left_fence | +1.956% | no |
| 4.0-none | native-prepared | join_residual | +1.655% | yes |
| 4.0-none | native-prepared | join_text_predicate | +1.742% | no |
| 4.0-none | native-prepared | join_text_projection | +1.497% | no |
| 4.0-none | native-prepared | join_integer_outputs | +1.991% | yes |
| 4.0-none | native-prepared | five_table_bounded | +4.504% | no |
| 4.0-none | native-query | scalar | +3.229% | yes |
| 4.0-none | native-query | pk_hit | +2.933% | yes |
| 4.0-none | native-query | indexed_join | +3.835% | yes |
| 4.0-none | native-query | three_table_join | +1.421% | no |
| 4.0-none | native-query | three_table_join_miss | +2.343% | no |
| 4.0-none | native-query | three_table_join_range | +3.696% | yes |
| 4.0-none | native-query | three_table_computed_key | +2.448% | no |
| 4.0-none | native-query | four_table_join | +4.085% | no |
| 4.0-none | native-query | three_table_left_fence | +1.898% | no |
| 4.0-none | native-query | join_residual | +3.115% | no |
| 4.0-none | native-query | join_text_predicate | +2.827% | no |
| 4.0-none | native-query | join_text_projection | +2.917% | no |
| 4.0-none | native-query | join_integer_outputs | +1.651% | no |
| 4.0-none | native-query | five_table_bounded | +1.914% | no |
| 4.0-none | native-eager | scalar | +2.196% | no |
| 4.0-none | native-eager | pk_hit | +1.938% | no |
| 4.0-none | native-eager | indexed_join | +2.174% | no |
| 4.0-none | native-eager | three_table_join | +1.851% | no |
| 4.0-none | native-eager | three_table_join_miss | +1.059% | yes |
| 4.0-none | native-eager | three_table_join_range | +0.357% | no |
| 4.0-none | native-eager | three_table_computed_key | +3.489% | no |
| 4.0-none | native-eager | four_table_join | +1.547% | no |
| 4.0-none | native-eager | three_table_left_fence | +1.773% | no |
| 4.0-none | native-eager | join_residual | +0.444% | no |
| 4.0-none | native-eager | join_text_predicate | +2.280% | no |
| 4.0-none | native-eager | join_text_projection | +0.696% | no |
| 4.0-none | native-eager | join_integer_outputs | +2.239% | no |
| 4.0-yjit | native-prepared | scalar | +2.281% | no |
| 4.0-yjit | native-prepared | pk_hit | +2.398% | yes |
| 4.0-yjit | native-prepared | three_table_join | +1.951% | no |
| 4.0-yjit | native-prepared | three_table_join_miss | +1.734% | no |
| 4.0-yjit | native-prepared | three_table_join_range | +1.307% | no |
| 4.0-yjit | native-prepared | join_residual | +0.837% | no |
| 4.0-yjit | native-prepared | join_text_predicate | +1.629% | no |
| 4.0-yjit | native-prepared | join_text_projection | +1.847% | no |
| 4.0-yjit | native-query | pk_hit | +1.503% | no |
| 4.0-yjit | native-query | three_table_join_miss | +0.173% | no |
| 4.0-yjit | native-query | three_table_join_range | +0.798% | no |
| 4.0-yjit | native-query | four_table_join | +0.919% | no |
| 4.0-yjit | native-query | three_table_left_fence | +0.150% | no |
| 4.0-yjit | native-query | join_residual | +0.303% | no |
| 4.0-yjit | native-query | join_text_predicate | +0.271% | no |
| 4.0-yjit | native-query | join_text_projection | +1.845% | yes |
| 4.0-yjit | native-query | join_integer_outputs | +0.550% | yes |
| 4.0-yjit | native-eager | scalar | +0.605% | no |
| 4.0-yjit | native-eager | three_table_join | +0.804% | no |
| 4.0-yjit | native-eager | three_table_join_miss | +0.791% | no |
| 4.0-yjit | native-eager | four_table_join | +0.542% | no |
| 4.0-yjit | native-eager | three_table_left_fence | +0.399% | no |
| 4.0-yjit | native-eager | join_text_predicate | +0.479% | no |
| 4.0-yjit | native-eager | join_text_projection | +1.876% | no |
| 4.1-zjit | native-prepared | indexed_join | +0.084% | no |
| 4.1-zjit | native-prepared | three_table_computed_key | +2.558% | yes |
| 4.1-zjit | native-prepared | four_table_join | +4.329% | yes |
| 4.1-zjit | native-prepared | three_table_left_fence | +2.784% | yes |
| 4.1-zjit | native-prepared | join_residual | +1.833% | no |
| 4.1-zjit | native-prepared | join_text_predicate | +0.892% | no |
| 4.1-zjit | native-prepared | join_text_projection | +7.451% | yes |
| 4.1-zjit | native-prepared | join_integer_outputs | +2.079% | no |
| 4.1-zjit | native-query | scalar | +0.626% | no |
| 4.1-zjit | native-query | indexed_join | +3.472% | yes |
| 4.1-zjit | native-query | three_table_join | +4.196% | no |
| 4.1-zjit | native-query | three_table_computed_key | +2.074% | no |
| 4.1-zjit | native-query | join_integer_outputs | +6.580% | no |
| 4.1-zjit | native-eager | scalar | +2.291% | no |
| 4.1-zjit | native-eager | pk_hit | +0.518% | no |
| 4.1-zjit | native-eager | indexed_join | +3.572% | yes |
| 4.1-zjit | native-eager | three_table_join | +1.569% | yes |
| 4.1-zjit | native-eager | three_table_join_miss | +1.620% | no |
| 4.1-zjit | native-eager | three_table_join_range | +1.391% | no |
| 4.1-zjit | native-eager | three_table_computed_key | +7.099% | yes |
| 4.1-zjit | native-eager | four_table_join | +1.969% | no |
| 4.1-zjit | native-eager | join_text_predicate | +0.343% | no |
| 4.1-zjit | native-eager | join_text_projection | +6.643% | no |
| 4.1-zjit | native-eager | join_integer_outputs | +0.643% | no |

## Fresh cross-core observations

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.662 | 0.808 | 1.393 | 2.165 | 5.770× | 3.347× |
| pk_hit | 3.912 | 1.614 | 2.220 | 3.919 | 2.424× | 1.762× |
| indexed_join | 175.859 | 54.963 | 2.471 | 126.706 | 3.200× | 71.158× |
| three_table_join | 234.395 | 4.091 | 3.112 | 13.552 | 57.301× | 75.315× |
| three_table_join_miss | 203.683 | 1.111 | 1.474 | 3.732 | 183.407× | 138.226× |
| three_table_join_range | 1337.553 | 82.799 | 61.177 | 241.627 | 16.154× | 21.864× |
| three_table_computed_key | 262.763 | 6.476 | 5.796 | 15.725 | 40.574× | 45.334× |
| four_table_join | 323.290 | 4.027 | 3.855 | 12.051 | 80.275× | 83.859× |
| three_table_left_fence | 461.464 | 3.239 | 3.078 | 11.470 | 142.480× | 149.911× |
| join_residual | 1032.369 | 397.018 | 746.446 | 1558.923 | 2.600× | 1.383× |
| join_text_predicate | 183.997 | 70.834 | 2.530 | 146.258 | 2.598× | 72.718× |
| join_text_projection | 154.294 | 82.324 | 2.441 | 147.013 | 1.874× | 63.220× |
| join_integer_outputs | 193.169 | 98.712 | 2.585 | 196.412 | 1.957× | 74.720× |
| five_table_bounded | 6539.444 | 22.138 | 17.499 | 60.361 | 295.391× | 373.713× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 7.829 | 4.804 | 5.636 | 5.802 | 0.614× |
| pk_hit | 10.254 | 4.011 | 4.621 | 4.189 | 0.391× |
| indexed_join | 41.402 | 149.586 | 155.399 | 141.792 | 3.613× |
| three_table_join | 182.665 | 221.136 | 230.754 | 208.997 | 1.211× |
| three_table_join_miss | 176.400 | 192.781 | 203.399 | 195.356 | 1.093× |
| three_table_join_range | 229.452 | 1302.370 | 1336.433 | 1347.934 | 5.676× |
| three_table_computed_key | 133.857 | 256.669 | 253.796 | 245.332 | 1.917× |
| four_table_join | 411.190 | 315.474 | 324.641 | 317.958 | 0.767× |
| three_table_left_fence | 48.882 | 435.501 | 459.485 | 439.785 | 8.909× |
| join_residual | 859.274 | 1033.463 | 1064.132 | 928.464 | 1.203× |
| join_text_predicate | 40.731 | 178.678 | 196.065 | 174.025 | 4.387× |
| join_text_projection | 50.035 | 150.460 | 158.077 | 156.567 | 3.007× |
| join_integer_outputs | 60.737 | 178.398 | 186.057 | 181.025 | 2.937× |
| five_table_bounded | 1538.440 | 6539.263 | 6785.605 | 7206.276 | 4.251× |

## Profile allocations

| Workload | Before objects/query | Final objects/query | Change |
|---|---:|---:|---:|---:|
| scalar | 39.000 | 39.000 | +0.000% |
| pk_hit | 31.000 | 31.000 | +0.000% |
| indexed_join | 572.500 | 572.500 | +0.000% |
| three_table_join | 853.000 | 853.000 | +0.000% |
| three_table_join_miss | 698.000 | 698.000 | +0.000% |
| three_table_join_range | 9453.100 | 9309.200 | -1.522% |
| three_table_computed_key | 887.000 | 887.000 | +0.000% |
| four_table_join | 1095.000 | 1095.000 | +0.000% |
| three_table_left_fence | 2196.800 | 2174.800 | -1.001% |
| join_residual | 5278.200 | 5278.200 | +0.000% |
| join_text_predicate | 618.000 | 618.000 | +0.000% |
| join_text_projection | 591.000 | 591.000 | +0.000% |
| join_integer_outputs | 648.000 | 648.000 | +0.000% |
| five_table_bounded | 50981.100 | 49546.950 | -2.813% |

## Profile calls (counts, not CPU-time percentages)

| Workload | Method | Before | Final |
|---|---|---:|---:|
| scalar | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| scalar | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| scalar | JedNative::Engine#compile | 1.333 | 1.333 |
| scalar | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| scalar | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| pk_hit | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| pk_hit | JedNative::Engine#compile | 1.000 | 1.000 |
| pk_hit | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| pk_hit | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| indexed_join | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| indexed_join | JedNative::Engine#compile | 19.667 | 19.667 |
| indexed_join | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| indexed_join | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| three_table_join | JedNative::NWayJoins#nway_extensions | 3.000 | 3.000 |
| three_table_join | JedNative::NWayJoins#nway_retain | 15.000 | 10.667 |
| three_table_join | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 7.333 |
| three_table_join | JedNative::NWayJoins#nway_step_cost | 13.000 | 13.000 |
| three_table_join | JedNative::NWayJoins#nway_extension_inputs | 3.000 | 3.000 |
| three_table_join | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_extensions | 3.000 | 3.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_retain | 15.000 | 10.667 |
| three_table_join_miss | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 7.333 |
| three_table_join_miss | JedNative::NWayJoins#nway_step_cost | 13.000 | 13.000 |
| three_table_join_miss | JedNative::NWayJoins#nway_extension_inputs | 3.000 | 3.000 |
| three_table_join_miss | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_miss | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_join_miss | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::NWayJoins#nway_extensions | 9.000 | 9.000 |
| three_table_join_range | JedNative::NWayJoins#nway_retain | 45.000 | 27.000 |
| three_table_join_range | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 22.000 |
| three_table_join_range | JedNative::NWayJoins#nway_step_cost | 39.000 | 39.000 |
| three_table_join_range | JedNative::NWayJoins#nway_extension_inputs | 9.000 | 9.000 |
| three_table_join_range | JedNative::Engine#compile | 37.667 | 37.667 |
| three_table_join_range | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_join_range | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_extensions | 3.000 | 3.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_retain | 11.333 | 7.667 |
| three_table_computed_key | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 4.667 |
| three_table_computed_key | JedNative::NWayJoins#nway_step_cost | 9.333 | 9.333 |
| three_table_computed_key | JedNative::NWayJoins#nway_extension_inputs | 3.000 | 3.000 |
| three_table_computed_key | JedNative::Engine#compile | 49.000 | 49.000 |
| three_table_computed_key | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| three_table_computed_key | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| four_table_join | JedNative::NWayJoins#nway_extensions | 9.333 | 9.333 |
| four_table_join | JedNative::NWayJoins#nway_retain | 41.333 | 27.667 |
| four_table_join | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 24.333 |
| four_table_join | JedNative::NWayJoins#nway_step_cost | 38.667 | 38.667 |
| four_table_join | JedNative::NWayJoins#nway_extension_inputs | 9.333 | 9.333 |
| four_table_join | JedNative::Engine#compile | 103.333 | 103.333 |
| four_table_join | JedNative::NWayJoins#nway_join_plan | 0.333 | 0.333 |
| four_table_join | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_extensions | 3.000 | 3.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_retain | 14.000 | 11.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 5.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_step_cost | 11.000 | 11.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_extension_inputs | 3.000 | 3.000 |
| three_table_left_fence | JedNative::Engine#compile | 22.000 | 22.000 |
| three_table_left_fence | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| three_table_left_fence | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
| join_residual | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| join_residual | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| join_residual | JedNative::Engine#compile | 30.000 | 30.000 |
| join_residual | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_residual | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_predicate | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| join_text_predicate | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| join_text_predicate | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| join_text_predicate | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| join_text_predicate | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| join_text_predicate | JedNative::Engine#compile | 25.667 | 25.667 |
| join_text_predicate | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_predicate | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| join_text_projection | JedNative::Engine#compile | 0.000 | 0.000 |
| join_text_projection | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_text_projection | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_extensions | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_retain | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_step_cost | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_extension_inputs | 0.000 | 0.000 |
| join_integer_outputs | JedNative::Engine#compile | 0.000 | 0.000 |
| join_integer_outputs | JedNative::NWayJoins#nway_join_plan | 0.000 | 0.000 |
| join_integer_outputs | JedNative::PreparedJoins#nway_join_plan | 0.000 | 0.000 |
| five_table_bounded | JedNative::NWayJoins#nway_extensions | 75.000 | 75.000 |
| five_table_bounded | JedNative::NWayJoins#nway_retain | 316.000 | 140.000 |
| five_table_bounded | JedNative::NWayJoins#nway_strictly_dominated? | 0.000 | 209.000 |
| five_table_bounded | JedNative::NWayJoins#nway_step_cost | 306.000 | 306.000 |
| five_table_bounded | JedNative::NWayJoins#nway_extension_inputs | 75.000 | 75.000 |
| five_table_bounded | JedNative::Engine#compile | 668.000 | 668.000 |
| five_table_bounded | JedNative::NWayJoins#nway_join_plan | 1.000 | 1.000 |
| five_table_bounded | JedNative::PreparedJoins#nway_join_plan | 1.000 | 1.000 |
