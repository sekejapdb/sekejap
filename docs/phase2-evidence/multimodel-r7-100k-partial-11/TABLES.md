# Phase 2 multimodel benchmark report

**Evidence state:** INCOMPLETE — candidate evidence only, not public acceptance

- Raw report SHA-256: `e0b2267755e7e2432d59dfc0d7e33dcb5e92b13ed118beba42c62f4d07580583`
- Benchmark binary SHA-256: `5034ad305932ecf60b28a1c9a61be5a08935cd56f2beeb09807ca24addb8e71e`
- Driver format: `phase2-multimodel-driver-v2`
- Capture complete: `false`
- Workload complete: `false`
- Captured arms: 11
- Driver scheduled arms: unknown
- Completed workloads: 11
- Typed resource refusals: 0
- Nonzero process exits: 0

All numeric summaries below exclude refused arms. A dash means no complete comparable measurement; it never means zero. Sampled disk peaks are lower bounds because polling can miss transients and unlinked SQLite temporary files. The in-process final sample is taken while the benchmark process and live database files still exist; the after-close final tree is measured by the driver after process exit. RSS includes the benchmark harness, deterministic generator and any in-process correctness work.

## N=100,000, dimension=32, reader=`batch`

one snapshot held across the first 256-row update commit of each CRUD round, then released

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 1 | 1 | 0 | atomic |
| E4 resumable | 0 | 0 | 0 | — |
| SQLite atomic | 1 | 1 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 4.598 [4.598–4.598] | — | 2.989 [2.989–2.989] |
| Graph load | s | 11.597 [11.597–11.597] | — | 5.341 [5.341–5.341] |
| Scalar late build | s | 2.189 [2.189–2.189] | — | 0.233 [0.233–0.233] |
| Vector late build | s | 0.452 [0.452–0.452] | — | N/A |
| Spatial late build | s | 0.831 [0.831–0.831] | — | 1.313 [1.313–1.313] |
| Text late build | s | 18.536 [18.536–18.536] | — | 0.341 [0.341–0.341] |
| CRUD round 1 update | s | 41.164 [41.164–41.164] | — | 26.049 [26.049–26.049] |
| CRUD round 1 delete | s | 5.067 [5.067–5.067] | — | 3.987 [3.987–3.987] |
| CRUD round 1 reinsert+edges | s | 6.086 [6.086–6.086] | — | 1.680 [1.680–1.680] |
| CRUD round 1 total | s | 52.318 [52.318–52.318] | — | 31.715 [31.715–31.715] |
| CRUD round 2 update | s | 34.919 [34.919–34.919] | — | 30.013 [30.013–30.013] |
| CRUD round 2 delete | s | 4.309 [4.309–4.309] | — | 2.958 [2.958–2.958] |
| CRUD round 2 reinsert+edges | s | 4.956 [4.956–4.956] | — | 1.826 [1.826–1.826] |
| CRUD round 2 total | s | 44.185 [44.185–44.185] | — | 34.798 [34.798–34.798] |
| CRUD round 3 update | s | 35.205 [35.205–35.205] | — | 27.810 [27.810–27.810] |
| CRUD round 3 delete | s | 4.393 [4.393–4.393] | — | 2.745 [2.745–2.745] |
| CRUD round 3 reinsert+edges | s | 5.785 [5.785–5.785] | — | 1.862 [1.862–1.862] |
| CRUD round 3 total | s | 45.382 [45.382–45.382] | — | 32.417 [32.417–32.417] |
| CRUD all three rounds total | s | 141.885 [141.885–141.885] | — | 98.931 [98.931–98.931] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.555 [0.555–0.555] | — | 0.524 [0.524–0.524] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 31.986 [31.986–31.986] | — | 7.121 [7.121–7.121] |
| Pre-CRUD query `scalar_active_age` | ms | 17.258 [17.258–17.258] | — | 0.193 [0.193–0.193] |
| Pre-CRUD query `spatial_bbox` | ms | 6.292 [6.292–6.292] | — | 15.673 [15.673–15.673] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 46.504 [46.504–46.504] |
| Pre-CRUD query `text_active_vector` | ms | 207.362 [207.362–207.362] | — | 66.088 [66.088–66.088] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 54.336 [54.336–54.336] | — | 101.186 [101.186–101.186] |
| Pre-CRUD query `vector_cosine_k10` | ms | 164.621 [164.621–164.621] | — | 85.624 [85.624–85.624] |
| Post-CRUD query `scalar_age` | ms | 0.542 [0.542–0.542] | — | 0.456 [0.456–0.456] |
| Post-CRUD query `spatial` | ms | 4.188 [4.188–4.188] | — | 19.603 [19.603–19.603] |
| Post-CRUD query `text` | ms | 53.752 [53.752–53.752] | — | 138.559 [138.559–138.559] |
| Post-CRUD query `vector` | ms | 167.286 [167.286–167.286] | — | 104.014 [104.014–104.014] |
| Reopen | ms | 8.555 [8.555–8.555] | — | 1.451 [1.451–1.451] |
| Loaded logical | MiB | 44.344 [44.344–44.344] | — | 41.090 [41.090–41.090] |
| Loaded allocated | MiB | 44.348 [44.348–44.348] | — | 41.090 [41.090–41.090] |
| In-process final logical | MiB | 86.098 [86.098–86.098] | — | 66.711 [66.711–66.711] |
| In-process final allocated | MiB | 86.102 [86.102–86.102] | — | 66.711 [66.711–66.711] |
| After-close final logical | MiB | 86.098 [86.098–86.098] | — | 66.680 [66.680–66.680] |
| After-close final allocated | MiB | 86.102 [86.102–86.102] | — | 66.680 [66.680–66.680] |
| Sampled peak logical | MiB | 91.433 [91.433–91.433] | — | 77.842 [77.842–77.842] |
| Sampled peak allocated | MiB | 124.629 [124.629–124.629] | — | 104.664 [104.664–104.664] |
| Sampled peak / loaded logical | × | 2.062 [2.062–2.062] | — | 1.894 [1.894–1.894] |
| Sampled peak / loaded allocated | × | 2.810 [2.810–2.810] | — | 2.547 [2.547–2.547] |
| RSS high-water | MiB | 23.242 [23.242–23.242] | — | 17.594 [17.594–17.594] |

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

## Resource refusals

No typed resource refusals were recorded.
