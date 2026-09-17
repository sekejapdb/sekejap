# Phase 2 multimodel benchmark report

**Evidence state:** INCOMPLETE — candidate evidence only, not public acceptance

- Raw report SHA-256: `7d587bd42d4b087e63194bcc83aab75a91d3931e475612c29d1f2ee277df6f1e`
- Benchmark binary SHA-256: `5034ad305932ecf60b28a1c9a61be5a08935cd56f2beeb09807ca24addb8e71e`
- Driver format: `phase2-multimodel-driver-v2`
- Capture complete: `true`
- Workload complete: `false`
- Captured arms: 36
- Driver scheduled arms: 36
- Completed workloads: 24
- Typed resource refusals: 12
- Nonzero process exits: 0

All numeric summaries below exclude refused arms. A dash means no complete comparable measurement; it never means zero. Sampled disk peaks are lower bounds because polling can miss transients and unlinked SQLite temporary files. The in-process final sample is taken while the benchmark process and live database files still exist; the after-close final tree is measured by the driver after process exit. RSS includes the benchmark harness, deterministic generator and any in-process correctness work.

## N=100,000, dimension=32, reader=`batch`

one snapshot held across the first 256-row update commit of each CRUD round, then released

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 4.598 [4.546–4.603] | 4.294 [4.211–4.990] | 2.989 [2.331–3.578] |
| Graph load | s | 11.597 [10.393–12.244] | 10.984 [10.004–11.210] | 5.341 [4.878–6.570] |
| Scalar late build | s | 2.189 [2.147–2.225] | 6.430 [5.286–6.558] | 0.235 [0.233–0.249] |
| Vector late build | s | 0.473 [0.452–0.576] | 1.660 [1.655–1.954] | N/A |
| Spatial late build | s | 0.871 [0.831–0.893] | 2.331 [2.248–2.869] | 1.313 [1.227–1.497] |
| Text late build | s | 18.677 [18.536–19.736] | 20.998 [20.709–22.253] | 0.380 [0.341–0.397] |
| CRUD round 1 update | s | 40.882 [40.362–41.164] | 40.149 [39.700–41.164] | 26.845 [26.049–31.090] |
| CRUD round 1 delete | s | 5.067 [4.720–5.376] | 4.957 [4.831–5.146] | 4.068 [3.987–4.210] |
| CRUD round 1 reinsert+edges | s | 6.086 [4.807–7.753] | 5.420 [5.246–5.433] | 1.986 [1.680–2.211] |
| CRUD round 1 total | s | 52.318 [49.890–54.011] | 50.716 [49.964–51.368] | 33.124 [31.715–37.286] |
| CRUD round 2 update | s | 35.245 [34.919–37.526] | 35.493 [35.362–40.401] | 30.013 [28.811–31.756] |
| CRUD round 2 delete | s | 4.061 [3.773–4.309] | 4.076 [3.939–4.263] | 3.055 [2.958–3.286] |
| CRUD round 2 reinsert+edges | s | 5.126 [4.956–5.399] | 5.611 [5.368–6.865] | 1.949 [1.826–2.266] |
| CRUD round 2 total | s | 44.185 [44.144–46.985] | 46.297 [45.049–50.032] | 34.798 [34.363–36.759] |
| CRUD round 3 update | s | 35.205 [34.638–40.365] | 36.566 [34.055–39.917] | 27.810 [24.774–29.586] |
| CRUD round 3 delete | s | 4.427 [4.393–4.433] | 4.307 [4.097–4.477] | 2.314 [2.219–2.745] |
| CRUD round 3 reinsert+edges | s | 5.785 [5.081–5.916] | 5.564 [4.949–6.138] | 1.885 [1.862–2.122] |
| CRUD round 3 total | s | 45.382 [44.147–50.715] | 46.437 [43.102–50.532] | 32.417 [28.878–34.022] |
| CRUD all three rounds total | s | 141.885 [138.181–151.711] | 144.101 [138.114–151.279] | 100.527 [98.931–103.905] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.555 [0.439–0.559] | 0.700 [0.512–2.700] | 0.573 [0.524–0.657] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 32.114 [31.986–50.482] | 31.427 [28.683–322.867] | 7.121 [6.765–10.173] |
| Pre-CRUD query `scalar_active_age` | ms | 18.606 [17.258–19.519] | 19.425 [19.088–25.289] | 0.208 [0.193–0.211] |
| Pre-CRUD query `spatial_bbox` | ms | 6.292 [4.973–7.278] | 5.992 [4.460–34.678] | 15.673 [14.706–16.082] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 46.504 [45.855–47.720] |
| Pre-CRUD query `text_active_vector` | ms | 213.366 [207.362–242.092] | 209.557 [206.548–240.440] | 66.088 [64.898–98.316] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 54.336 [51.784–73.235] | 54.634 [49.619–262.810] | 101.186 [82.224–124.574] |
| Pre-CRUD query `vector_cosine_k10` | ms | 168.178 [164.621–175.175] | 165.822 [164.693–168.476] | 85.839 [85.624–103.957] |
| Post-CRUD query `scalar_age` | ms | 0.504 [0.478–0.542] | 0.593 [0.521–1.288] | 0.456 [0.329–0.588] |
| Post-CRUD query `spatial` | ms | 4.928 [4.188–5.142] | 5.886 [4.724–6.359] | 18.334 [16.494–19.603] |
| Post-CRUD query `text` | ms | 55.316 [53.752–57.586] | 55.557 [55.108–56.087] | 133.725 [102.206–138.559] |
| Post-CRUD query `vector` | ms | 172.197 [167.286–177.156] | 176.567 [171.055–216.558] | 95.682 [88.288–104.014] |
| Reopen | ms | 10.673 [8.555–10.887] | 10.725 [9.900–11.016] | 1.451 [0.991–1.460] |
| Loaded logical | MiB | 44.344 [44.344–44.344] | 44.344 [44.344–44.344] | 41.090 [41.090–41.090] |
| Loaded allocated | MiB | 44.348 [44.348–44.348] | 44.348 [44.348–44.348] | 41.090 [41.090–41.090] |
| In-process final logical | MiB | 86.098 [86.098–86.098] | 86.098 [86.098–86.098] | 66.711 [66.711–66.711] |
| In-process final allocated | MiB | 86.102 [86.102–86.102] | 86.102 [86.102–86.102] | 66.711 [66.711–66.711] |
| After-close final logical | MiB | 86.098 [86.098–86.098] | 86.098 [86.098–86.098] | 66.680 [66.680–66.680] |
| After-close final allocated | MiB | 86.102 [86.102–86.102] | 86.102 [86.102–86.102] | 66.680 [66.680–66.680] |
| Sampled peak logical | MiB | 91.484 [91.433–91.484] | 91.484 [91.484–91.484] | 77.842 [77.842–77.842] |
| Sampled peak allocated | MiB | 124.629 [121.504–157.566] | 154.941 [125.004–221.879] | 106.852 [104.664–121.031] |
| Sampled peak / loaded logical | × | 2.063 [2.062–2.063] | 2.063 [2.063–2.063] | 1.894 [1.894–1.894] |
| Sampled peak / loaded allocated | × | 2.810 [2.740–3.553] | 3.494 [2.819–5.003] | 2.600 [2.547–2.946] |
| RSS high-water | MiB | 23.242 [23.234–23.488] | 23.418 [23.043–23.703] | 17.594 [17.586–17.707] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.538 [1.286–1.950]× | 1.409 [1.200–2.141]× |
| Graph load | 2.131 [1.864–2.171]× | 2.056 [1.523–2.298]× |
| Scalar late build | 9.413 [8.615–9.489]× | 26.314 [22.539–27.652]× |
| Vector late build | — | — |
| Spatial late build | 0.633 [0.582–0.728]× | 1.831 [1.558–2.185]× |
| Text late build | 49.694 [49.211–54.436]× | 54.566 [52.873–65.350]× |
| CRUD round 1 update | 1.504 [1.315–1.580]× | 1.496 [1.324–1.524]× |
| CRUD round 1 delete | 1.271 [1.160–1.277]× | 1.212 [1.178–1.265]× |
| CRUD round 1 reinsert+edges | 3.623 [2.174–3.904]× | 2.642 [2.451–3.234]× |
| CRUD round 1 total | 1.506 [1.449–1.650]× | 1.531 [1.378–1.575]× |
| CRUD round 2 update | 1.163 [1.110–1.302]× | 1.232 [1.178–1.272]× |
| CRUD round 2 delete | 1.236 [1.235–1.457]× | 1.378 [1.199–1.395]× |
| CRUD round 2 reinsert+edges | 2.630 [2.383–2.713]× | 3.030 [2.754–3.072]× |
| CRUD round 2 total | 1.270 [1.201–1.367]× | 1.347 [1.295–1.361]× |
| CRUD round 3 update | 1.266 [1.171–1.629]× | 1.349 [1.225–1.476]× |
| CRUD round 3 delete | 1.913 [1.600–1.997]× | 1.935 [1.493–1.941]× |
| CRUD round 3 reinsert+edges | 3.107 [2.394–3.138]× | 2.893 [2.658–2.951]× |
| CRUD round 3 total | 1.400 [1.298–1.756]× | 1.485 [1.330–1.608]× |
| CRUD all three rounds total | 1.434 [1.330–1.509]× | 1.433 [1.396–1.456]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.976 [0.668–1.060]× | 1.065 [0.979–4.714]× |
| Pre-CRUD query `members_active_spatial_vector` | 4.492 [3.157–7.462]× | 4.645 [4.028–31.739]× |
| Pre-CRUD query `scalar_active_age` | 89.609 [88.344–93.933]× | 99.114 [93.483–120.078]× |
| Pre-CRUD query `spatial_bbox` | 0.401 [0.309–0.495]× | 0.407 [0.285–2.156]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 3.138 [2.170–3.730]× | 3.171 [2.446–3.183]× |
| Pre-CRUD query `text_positive_bm25_k10` | 0.537 [0.416–0.891]× | 0.664 [0.490–2.110]× |
| Pre-CRUD query `vector_cosine_k10` | 1.923 [1.685–1.959]× | 1.923 [1.595–1.963]× |
| Post-CRUD query `scalar_age` | 1.189 [0.858–1.453]× | 1.803 [0.886–2.827]× |
| Post-CRUD query `spatial` | 0.269 [0.214–0.312]× | 0.324 [0.258–0.357]× |
| Post-CRUD query `text` | 0.414 [0.388–0.563]× | 0.412 [0.405–0.544]× |
| Post-CRUD query `vector` | 1.800 [1.608–2.007]× | 2.000 [1.788–2.082]× |
| Reopen | 7.308 [5.895–10.986]× | 7.344 [6.822–11.116]× |

## N=100,000, dimension=32, reader=`held`

one snapshot held across all three CRUD rounds

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 2.959 [2.620–6.038] |
| Graph load | s | — | — | 4.816 [4.677–31.947] |
| Scalar late build | s | — | — | 0.242 [0.241–0.472] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 1.173 [1.159–2.015] |
| Text late build | s | — | — | 0.328 [0.326–0.674] |
| CRUD round 1 update | s | — | — | 30.237 [27.305–41.477] |
| CRUD round 1 delete | s | — | — | 5.975 [4.783–9.189] |
| CRUD round 1 reinsert+edges | s | — | — | 2.040 [1.864–3.578] |
| CRUD round 1 total | s | — | — | 38.253 [33.953–54.243] |
| CRUD round 2 update | s | — | — | 28.561 [26.276–62.051] |
| CRUD round 2 delete | s | — | — | 2.842 [2.762–5.177] |
| CRUD round 2 reinsert+edges | s | — | — | 1.708 [1.619–2.245] |
| CRUD round 2 total | s | — | — | 33.031 [30.737–69.473] |
| CRUD round 3 update | s | — | — | 27.953 [25.676–28.353] |
| CRUD round 3 delete | s | — | — | 2.910 [2.631–3.887] |
| CRUD round 3 reinsert+edges | s | — | — | 1.906 [1.630–2.169] |
| CRUD round 3 total | s | — | — | 32.493 [30.214–34.408] |
| CRUD all three rounds total | s | — | — | 117.488 [97.183–142.134] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.581 [0.574–0.891] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 8.824 [8.628–15.077] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.272 [0.265–0.334] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 16.828 [16.010–20.682] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 46.335 [42.907–101.138] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 73.868 [67.783–249.605] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 87.404 [86.110–101.787] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 85.141 [83.008–157.716] |
| Post-CRUD query `scalar_age` | ms | — | — | 2.798 [1.081–3.743] |
| Post-CRUD query `spatial` | ms | — | — | 23.379 [17.841–24.302] |
| Post-CRUD query `text` | ms | — | — | 155.750 [127.994–163.691] |
| Post-CRUD query `vector` | ms | — | — | 127.583 [106.989–136.685] |
| Reopen | ms | — | — | 1.076 [1.061–1.201] |
| Loaded logical | MiB | — | — | 41.090 [41.090–41.090] |
| Loaded allocated | MiB | — | — | 41.090 [41.090–41.090] |
| In-process final logical | MiB | — | — | 70.961 [70.961–70.961] |
| In-process final allocated | MiB | — | — | 74.617 [74.617–74.680] |
| After-close final logical | MiB | — | — | 66.680 [66.680–66.680] |
| After-close final allocated | MiB | — | — | 66.680 [66.680–66.680] |
| Sampled peak logical | MiB | — | — | 2263.691 [2261.715–2263.691] |
| Sampled peak allocated | MiB | — | — | 4158.973 [4158.973–4159.035] |
| Sampled peak / loaded logical | × | — | — | 55.091 [55.043–55.091] |
| Sampled peak / loaded allocated | × | — | — | 101.217 [101.217–101.218] |
| RSS high-water | MiB | — | — | 23.602 [23.539–23.797] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | — | — |
| Graph load | — | — |
| Scalar late build | — | — |
| Vector late build | — | — |
| Spatial late build | — | — |
| Text late build | — | — |
| CRUD round 1 update | — | — |
| CRUD round 1 delete | — | — |
| CRUD round 1 reinsert+edges | — | — |
| CRUD round 1 total | — | — |
| CRUD round 2 update | — | — |
| CRUD round 2 delete | — | — |
| CRUD round 2 reinsert+edges | — | — |
| CRUD round 2 total | — | — |
| CRUD round 3 update | — | — |
| CRUD round 3 delete | — | — |
| CRUD round 3 reinsert+edges | — | — |
| CRUD round 3 total | — | — |
| CRUD all three rounds total | — | — |
| Pre-CRUD query `combined_graph_active_bbox_vector` | — | — |
| Pre-CRUD query `members_active_spatial_vector` | — | — |
| Pre-CRUD query `scalar_active_age` | — | — |
| Pre-CRUD query `spatial_bbox` | — | — |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | — | — |
| Pre-CRUD query `text_positive_bm25_k10` | — | — |
| Pre-CRUD query `vector_cosine_k10` | — | — |
| Post-CRUD query `scalar_age` | — | — |
| Post-CRUD query `spatial` | — | — |
| Post-CRUD query `text` | — | — |
| Post-CRUD query `vector` | — | — |
| Reopen | — | — |

## N=100,000, dimension=32, reader=`none`

no concurrent reader

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 4.118 [4.009–4.190] | 4.499 [4.306–4.539] | 2.618 [2.469–3.136] |
| Graph load | s | 10.429 [10.068–11.402] | 10.671 [10.387–13.119] | 5.034 [5.007–5.118] |
| Scalar late build | s | 2.231 [2.094–2.280] | 6.064 [5.490–6.704] | 0.218 [0.204–0.220] |
| Vector late build | s | 0.447 [0.443–0.457] | 1.581 [1.304–1.775] | N/A |
| Spatial late build | s | 0.862 [0.862–0.896] | 2.210 [1.795–2.373] | 1.252 [1.198–1.271] |
| Text late build | s | 18.791 [18.631–18.931] | 20.554 [20.059–22.211] | 0.313 [0.300–0.331] |
| CRUD round 1 update | s | 39.618 [39.124–40.149] | 39.056 [38.696–40.774] | 29.368 [28.143–33.025] |
| CRUD round 1 delete | s | 4.703 [4.617–5.047] | 4.974 [4.760–5.499] | 4.563 [4.173–5.885] |
| CRUD round 1 reinsert+edges | s | 5.129 [5.080–6.312] | 5.566 [5.031–5.880] | 2.054 [1.963–2.886] |
| CRUD round 1 total | s | 49.894 [48.907–50.978] | 49.061 [49.022–52.153] | 35.504 [34.761–41.796] |
| CRUD round 2 update | s | 35.894 [34.316–37.437] | 35.396 [34.962–42.639] | 27.667 [27.456–30.271] |
| CRUD round 2 delete | s | 3.901 [3.864–4.910] | 4.027 [3.844–4.906] | 2.801 [2.522–8.376] |
| CRUD round 2 reinsert+edges | s | 4.959 [4.884–5.290] | 5.072 [4.930–6.815] | 1.992 [1.912–5.467] |
| CRUD round 2 total | s | 44.679 [43.138–47.637] | 44.311 [43.919–54.361] | 34.984 [31.971–41.510] |
| CRUD round 3 update | s | 36.892 [35.566–39.701] | 37.648 [36.146–38.333] | 27.745 [25.813–30.817] |
| CRUD round 3 delete | s | 4.050 [3.803–4.151] | 3.808 [3.774–4.552] | 2.324 [2.190–2.344] |
| CRUD round 3 reinsert+edges | s | 5.306 [5.029–5.529] | 5.211 [5.114–5.424] | 1.768 [1.753–1.798] |
| CRUD round 3 total | s | 45.971 [44.674–49.381] | 47.317 [45.067–47.625] | 31.887 [29.756–34.909] |
| CRUD all three rounds total | s | 139.557 [137.707–147.996] | 141.140 [140.650–151.046] | 108.666 [96.487–111.922] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.577 [0.482–0.631] | 0.529 [0.525–0.611] | 0.518 [0.479–0.648] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 29.744 [25.977–36.655] | 29.800 [25.594–46.043] | 8.927 [7.145–9.458] |
| Pre-CRUD query `scalar_active_age` | ms | 19.528 [16.239–26.112] | 18.026 [17.189–19.379] | 0.239 [0.207–0.351] |
| Pre-CRUD query `spatial_bbox` | ms | 7.531 [5.520–7.686] | 5.165 [3.584–5.683] | 15.553 [13.387–16.609] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 41.704 [40.861–56.844] |
| Pre-CRUD query `text_active_vector` | ms | 228.606 [205.344–271.213] | 207.172 [200.407–212.608] | 67.823 [62.183–72.098] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 58.278 [52.848–62.083] | 52.818 [52.092–54.187] | 79.744 [76.574–95.994] |
| Pre-CRUD query `vector_cosine_k10` | ms | 165.632 [158.605–204.819] | 166.728 [154.631–189.167] | 75.842 [71.854–91.614] |
| Post-CRUD query `scalar_age` | ms | 0.515 [0.491–0.595] | 0.476 [0.428–0.792] | 0.381 [0.366–0.388] |
| Post-CRUD query `spatial` | ms | 4.122 [3.768–4.288] | 4.456 [4.145–5.737] | 19.477 [14.948–20.647] |
| Post-CRUD query `text` | ms | 52.868 [51.514–54.188] | 56.021 [50.341–56.803] | 101.664 [99.639–115.542] |
| Post-CRUD query `vector` | ms | 174.281 [156.632–174.447] | 155.837 [150.923–178.740] | 89.084 [85.096–92.435] |
| Reopen | ms | 10.105 [9.744–11.110] | 10.063 [8.438–12.187] | 1.236 [1.035–1.959] |
| Loaded logical | MiB | 44.344 [44.344–44.344] | 44.344 [44.344–44.344] | 41.090 [41.090–41.090] |
| Loaded allocated | MiB | 44.348 [44.348–44.348] | 44.348 [44.348–44.348] | 41.090 [41.090–41.090] |
| In-process final logical | MiB | 86.098 [86.098–86.098] | 86.098 [86.098–86.098] | 66.711 [66.711–66.711] |
| In-process final allocated | MiB | 86.102 [86.102–86.102] | 86.102 [86.102–86.102] | 66.711 [66.711–66.711] |
| After-close final logical | MiB | 86.098 [86.098–86.098] | 86.098 [86.098–86.098] | 66.680 [66.680–66.680] |
| After-close final allocated | MiB | 86.102 [86.102–86.102] | 86.102 [86.102–86.102] | 66.680 [66.680–66.680] |
| Sampled peak logical | MiB | 91.484 [91.433–91.484] | 91.484 [91.433–91.484] | 77.842 [77.842–77.842] |
| Sampled peak allocated | MiB | 157.816 [118.566–157.941] | 221.754 [118.004–221.941] | 121.031 [107.781–141.281] |
| Sampled peak / loaded logical | × | 2.063 [2.062–2.063] | 2.063 [2.062–2.063] | 1.894 [1.894–1.894] |
| Sampled peak / loaded allocated | × | 3.559 [2.674–3.561] | 5.000 [2.661–5.005] | 2.946 [2.623–3.438] |
| RSS high-water | MiB | 23.223 [23.164–23.500] | 23.422 [23.363–23.570] | 17.711 [17.594–17.832] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.573 [1.278–1.697]× | 1.719 [1.447–1.744]× |
| Graph load | 2.038 [2.000–2.277]× | 2.131 [2.063–2.563]× |
| Scalar late build | 10.252 [10.155–10.471]× | 27.851 [26.884–30.512]× |
| Vector late build | — | — |
| Spatial late build | 0.716 [0.679–0.720]× | 1.766 [1.498–1.867]× |
| Text late build | 60.013 [56.293–63.023]× | 65.645 [60.609–73.943]× |
| CRUD round 1 update | 1.349 [1.216–1.390]× | 1.330 [1.172–1.449]× |
| CRUD round 1 delete | 1.031 [0.785–1.210]× | 1.192 [0.809–1.205]× |
| CRUD round 1 reinsert+edges | 2.474 [1.777–3.216]× | 2.563 [1.929–2.863]× |
| CRUD round 1 total | 1.407 [1.194–1.436]× | 1.382 [1.173–1.500]× |
| CRUD round 2 update | 1.307 [1.134–1.353]× | 1.273 [1.169–1.541]× |
| CRUD round 2 delete | 1.380 [0.586–1.546]× | 1.372 [0.586–1.596]× |
| CRUD round 2 reinsert+edges | 2.452 [0.968–2.593]× | 2.475 [1.247–2.652]× |
| CRUD round 2 total | 1.233 [1.148–1.397]× | 1.310 [1.267–1.374]× |
| CRUD round 3 update | 1.288 [1.282–1.429]× | 1.382 [1.222–1.400]× |
| CRUD round 3 delete | 1.786 [1.623–1.849]× | 1.739 [1.610–1.959]× |
| CRUD round 3 reinsert+edges | 2.951 [2.869–3.127]× | 2.917 [2.898–3.068]× |
| CRUD round 3 total | 1.415 [1.401–1.545]× | 1.484 [1.364–1.515]× |
| CRUD all three rounds total | 1.322 [1.267–1.446]× | 1.350 [1.294–1.463]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.974 [0.930–1.204]× | 1.021 [0.810–1.274]× |
| Pre-CRUD query `members_active_spatial_vector` | 3.876 [2.910–4.163]× | 3.338 [2.706–6.444]× |
| Pre-CRUD query `scalar_active_age` | 94.371 [46.246–109.382]× | 81.177 [48.952–87.114]× |
| Pre-CRUD query `spatial_bbox` | 0.453 [0.412–0.494]× | 0.332 [0.216–0.425]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 3.371 [2.848–4.362]× | 3.135 [2.780–3.332]× |
| Pre-CRUD query `text_positive_bm25_k10` | 0.731 [0.551–0.811]× | 0.653 [0.550–0.708]× |
| Pre-CRUD query `vector_cosine_k10` | 2.305 [1.731–2.701]× | 2.198 [1.688–2.633]× |
| Post-CRUD query `scalar_age` | 1.341 [1.326–1.561]× | 1.301 [1.104–2.080]× |
| Post-CRUD query `spatial` | 0.212 [0.182–0.287]× | 0.216 [0.213–0.384]× |
| Post-CRUD query `text` | 0.507 [0.458–0.544]× | 0.495 [0.485–0.570]× |
| Post-CRUD query `vector` | 1.956 [1.695–2.050]× | 1.774 [1.749–1.934]× |
| Reopen | 8.989 [4.973–9.760]× | 8.142 [6.220–8.149]× |

## N=100,000, dimension=32, reader=`short`

one snapshot held across each complete CRUD round

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 2.586 [2.530–2.636] |
| Graph load | s | — | — | 4.856 [4.646–4.887] |
| Scalar late build | s | — | — | 0.221 [0.201–0.228] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 1.232 [1.227–1.313] |
| Text late build | s | — | — | 0.323 [0.316–0.337] |
| CRUD round 1 update | s | — | — | 26.652 [26.094–27.952] |
| CRUD round 1 delete | s | — | — | 4.091 [3.968–4.228] |
| CRUD round 1 reinsert+edges | s | — | — | 1.763 [1.763–2.020] |
| CRUD round 1 total | s | — | — | 32.383 [32.342–33.806] |
| CRUD round 2 update | s | — | — | 28.957 [26.717–29.603] |
| CRUD round 2 delete | s | — | — | 2.891 [2.844–2.935] |
| CRUD round 2 reinsert+edges | s | — | — | 1.910 [1.687–2.168] |
| CRUD round 2 total | s | — | — | 33.488 [31.517–34.706] |
| CRUD round 3 update | s | — | — | 26.636 [24.825–53.722] |
| CRUD round 3 delete | s | — | — | 2.266 [2.169–2.457] |
| CRUD round 3 reinsert+edges | s | — | — | 1.819 [1.780–2.116] |
| CRUD round 3 total | s | — | — | 30.912 [28.773–58.103] |
| CRUD all three rounds total | s | — | — | 98.001 [94.097–123.934] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.537 [0.507–0.587] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 7.721 [7.712–8.318] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.261 [0.257–0.261] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 16.665 [15.958–18.646] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 46.984 [46.106–49.970] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 65.981 [63.202–67.135] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 85.420 [82.528–85.626] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 92.022 [87.569–93.149] |
| Post-CRUD query `scalar_age` | ms | — | — | 0.462 [0.447–1.045] |
| Post-CRUD query `spatial` | ms | — | — | 22.860 [20.204–51.349] |
| Post-CRUD query `text` | ms | — | — | 137.042 [129.153–181.183] |
| Post-CRUD query `vector` | ms | — | — | 137.788 [123.204–185.768] |
| Reopen | ms | — | — | 1.334 [1.230–1.658] |
| Loaded logical | MiB | — | — | 41.090 [41.090–41.090] |
| Loaded allocated | MiB | — | — | 41.090 [41.090–41.090] |
| In-process final logical | MiB | — | — | 70.961 [70.961–70.961] |
| In-process final allocated | MiB | — | — | 74.617 [74.555–74.617] |
| After-close final logical | MiB | — | — | 66.680 [66.680–66.680] |
| After-close final allocated | MiB | — | — | 66.680 [66.680–66.680] |
| Sampled peak logical | MiB | — | — | 2263.691 [2263.105–2263.691] |
| Sampled peak allocated | MiB | — | — | 3200.441 [2852.691–3876.691] |
| Sampled peak / loaded logical | × | — | — | 55.091 [55.077–55.091] |
| Sampled peak / loaded allocated | × | — | — | 77.889 [69.426–94.347] |
| RSS high-water | MiB | — | — | 22.469 [22.320–22.477] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | — | — |
| Graph load | — | — |
| Scalar late build | — | — |
| Vector late build | — | — |
| Spatial late build | — | — |
| Text late build | — | — |
| CRUD round 1 update | — | — |
| CRUD round 1 delete | — | — |
| CRUD round 1 reinsert+edges | — | — |
| CRUD round 1 total | — | — |
| CRUD round 2 update | — | — |
| CRUD round 2 delete | — | — |
| CRUD round 2 reinsert+edges | — | — |
| CRUD round 2 total | — | — |
| CRUD round 3 update | — | — |
| CRUD round 3 delete | — | — |
| CRUD round 3 reinsert+edges | — | — |
| CRUD round 3 total | — | — |
| CRUD all three rounds total | — | — |
| Pre-CRUD query `combined_graph_active_bbox_vector` | — | — |
| Pre-CRUD query `members_active_spatial_vector` | — | — |
| Pre-CRUD query `scalar_active_age` | — | — |
| Pre-CRUD query `spatial_bbox` | — | — |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | — | — |
| Pre-CRUD query `text_positive_bm25_k10` | — | — |
| Pre-CRUD query `vector_cosine_k10` | — | — |
| Post-CRUD query `scalar_age` | — | — |
| Post-CRUD query `spatial` | — | — |
| Post-CRUD query `text` | — | — |
| Post-CRUD query `vector` | — | — |
| Reopen | — | — |

## Resource refusals

| N/dim | Reader | Arm/trial | Failed stage | Last confirmed commit | Confirmed boundaries | Oracle verified | Reason |
|---|---|---|---|---:|---|---:|---|
| 100,000/32 | `held` | e4 atomic / 0 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `held` | e4 atomic / 1 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `held` | e4 atomic / 2 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `held` | e4 resumable / 0 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `held` | e4 resumable / 1 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `held` | e4 resumable / 2 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 atomic / 0 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 atomic / 1 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 atomic / 2 | crud_update | 804 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 resumable / 0 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 resumable / 1 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 100,000/32 | `short` | e4 resumable / 2 | crud_update | 2755 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[4352,0,0],"entities":100000,"relationships":300000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
