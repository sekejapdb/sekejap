# Phase 2 multimodel benchmark report

**Evidence state:** INCOMPLETE — candidate evidence only, not public acceptance

- Raw report SHA-256: `93996b654049bc64b901b6d485d0d6e164f682428ede30cc383c18a8157b9550`
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

## N=10,000, dimension=32, reader=`batch`

one snapshot held across the first 256-row update commit of each CRUD round, then released

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 0.392 [0.372–0.410] | 0.414 [0.351–0.420] | 0.276 [0.269–0.364] |
| Graph load | s | 0.895 [0.880–0.998] | 0.883 [0.860–0.949] | 0.343 [0.336–0.528] |
| Scalar late build | s | 0.192 [0.181–0.193] | 0.468 [0.463–0.498] | 0.027 [0.018–0.028] |
| Vector late build | s | 0.046 [0.040–0.049] | 0.162 [0.137–0.289] | N/A |
| Spatial late build | s | 0.084 [0.068–0.117] | 0.229 [0.201–0.231] | 0.091 [0.087–0.097] |
| Text late build | s | 1.754 [1.716–1.789] | 2.040 [1.948–2.100] | 0.028 [0.026–0.045] |
| CRUD round 1 update | s | 3.734 [3.695–3.863] | 3.746 [3.578–3.761] | 1.935 [1.803–2.024] |
| CRUD round 1 delete | s | 0.472 [0.465–0.503] | 0.468 [0.447–0.506] | 0.347 [0.345–0.370] |
| CRUD round 1 reinsert+edges | s | 0.480 [0.471–0.486] | 0.453 [0.443–0.541] | 0.131 [0.129–0.144] |
| CRUD round 1 total | s | 4.684 [4.677–4.808] | 4.646 [4.489–4.808] | 2.410 [2.279–2.537] |
| CRUD round 2 update | s | 3.368 [3.339–3.452] | 3.417 [3.279–3.508] | 2.013 [1.777–2.064] |
| CRUD round 2 delete | s | 0.424 [0.410–0.458] | 0.390 [0.368–0.420] | 0.164 [0.159–0.196] |
| CRUD round 2 reinsert+edges | s | 0.468 [0.444–0.527] | 0.458 [0.435–0.522] | 0.140 [0.133–0.143] |
| CRUD round 2 total | s | 4.305 [4.232–4.355] | 4.220 [4.220–4.356] | 2.310 [2.077–2.402] |
| CRUD round 3 update | s | 3.457 [3.330–3.537] | 3.542 [3.369–3.602] | 1.662 [1.613–1.803] |
| CRUD round 3 delete | s | 0.426 [0.418–0.470] | 0.402 [0.389–0.411] | 0.190 [0.178–0.218] |
| CRUD round 3 reinsert+edges | s | 0.509 [0.465–0.620] | 0.525 [0.501–0.595] | 0.141 [0.135–0.166] |
| CRUD round 3 total | s | 4.436 [4.220–4.574] | 4.432 [4.296–4.608] | 1.975 [1.945–2.186] |
| CRUD all three rounds total | s | 13.352 [13.202–13.737] | 13.162 [13.141–13.772] | 6.564 [6.559–6.999] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.257 [0.200–0.280] | 0.240 [0.219–0.241] | 0.435 [0.347–0.460] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 1.062 [0.858–1.167] | 1.008 [0.952–1.065] | 0.491 [0.351–0.495] |
| Pre-CRUD query `scalar_active_age` | ms | 0.956 [0.686–1.165] | 0.851 [0.841–0.923] | 0.090 [0.079–0.100] |
| Pre-CRUD query `spatial_bbox` | ms | 0.430 [0.406–0.471] | 0.366 [0.362–0.407] | 1.132 [0.975–1.313] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 4.549 [4.205–4.790] |
| Pre-CRUD query `text_active_vector` | ms | 13.822 [12.906–16.137] | 15.100 [13.789–15.587] | 2.794 [2.473–3.095] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 4.441 [3.946–5.058] | 4.538 [3.936–4.600] | 5.034 [4.775–5.297] |
| Pre-CRUD query `vector_cosine_k10` | ms | 14.485 [13.067–15.226] | 12.537 [12.113–13.614] | 6.780 [5.742–6.873] |
| Post-CRUD query `scalar_age` | ms | 0.095 [0.089–0.129] | 0.089 [0.084–0.115] | 0.095 [0.087–0.121] |
| Post-CRUD query `spatial` | ms | 0.559 [0.554–0.596] | 0.526 [0.512–0.560] | 1.244 [0.937–11.160] |
| Post-CRUD query `text` | ms | 4.957 [4.879–5.581] | 4.725 [4.283–4.933] | 5.906 [4.160–8.338] |
| Post-CRUD query `vector` | ms | 13.627 [12.923–14.330] | 13.492 [12.357–13.650] | 6.877 [6.224–10.558] |
| Reopen | ms | 8.999 [8.763–11.317] | 10.490 [8.954–10.942] | 1.076 [1.065–1.209] |
| Loaded logical | MiB | 4.508 [4.508–4.508] | 4.508 [4.508–4.508] | 4.059 [4.059–4.059] |
| Loaded allocated | MiB | 4.512 [4.512–4.512] | 4.512 [4.512–4.512] | 4.059 [4.059–4.059] |
| In-process final logical | MiB | 8.664 [8.664–8.664] | 8.664 [8.664–8.664] | 6.754 [6.754–6.754] |
| In-process final allocated | MiB | 8.668 [8.668–8.668] | 8.668 [8.668–8.668] | 6.754 [6.754–6.754] |
| After-close final logical | MiB | 8.664 [8.664–8.664] | 8.664 [8.664–8.664] | 6.723 [6.723–6.723] |
| After-close final allocated | MiB | 8.668 [8.668–8.668] | 8.668 [8.668–8.668] | 6.723 [6.723–6.723] |
| Sampled peak logical | MiB | 13.308 [13.004–13.308] | 13.308 [13.308–13.308] | 12.231 [12.231–12.231] |
| Sampled peak allocated | MiB | 16.605 [16.605–16.605] | 23.191 [16.754–24.566] | 12.941 [12.941–12.941] |
| Sampled peak / loaded logical | × | 2.952 [2.885–2.952] | 2.952 [2.952–2.952] | 3.014 [3.014–3.014] |
| Sampled peak / loaded allocated | × | 3.681 [3.681–3.681] | 5.140 [3.713–5.445] | 3.189 [3.189–3.189] |
| RSS high-water | MiB | 21.449 [21.398–21.543] | 21.582 [21.465–21.617] | 13.812 [13.707–13.992] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.345 [1.126–1.456]× | 1.270 [1.137–1.559]× |
| Graph load | 2.609 [1.890–2.616]× | 2.558 [1.673–2.766]× |
| Scalar late build | 7.241 [6.959–10.057]× | 18.027 [17.549–25.698]× |
| Vector late build | — | — |
| Spatial late build | 0.926 [0.705–1.346]× | 2.358 [2.208–2.663]× |
| Text late build | 61.959 [39.644–68.203]× | 70.333 [46.536–79.320]× |
| CRUD round 1 update | 1.910 [1.845–2.143]× | 1.944 [1.768–2.078]× |
| CRUD round 1 delete | 1.339 [1.275–1.461]× | 1.286 [1.265–1.469]× |
| CRUD round 1 reinsert+edges | 3.700 [3.279–3.720]× | 3.509 [3.085–4.116]× |
| CRUD round 1 total | 1.943 [1.843–2.110]× | 1.995 [1.769–2.039]× |
| CRUD round 2 update | 1.715 [1.618–1.895]× | 1.700 [1.628–1.923]× |
| CRUD round 2 delete | 2.572 [2.169–2.798]× | 2.310 [1.994–2.564]× |
| CRUD round 2 reinsert+edges | 3.352 [3.275–3.759]× | 3.202 [3.101–3.935]× |
| CRUD round 2 total | 1.885 [1.762–2.072]× | 1.827 [1.813–2.032]× |
| CRUD round 3 update | 2.064 [1.918–2.129]× | 2.027 [1.998–2.195]× |
| CRUD round 3 delete | 2.237 [2.156–2.345]× | 2.042 [1.886–2.258]× |
| CRUD round 3 reinsert+edges | 3.287 [3.069–4.579]× | 3.588 [3.547–3.880]× |
| CRUD round 3 total | 2.170 [2.029–2.316]× | 2.175 [2.108–2.279]× |
| CRUD all three rounds total | 2.013 [1.908–2.093]× | 2.003 [1.968–2.005]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.645 [0.436–0.740]× | 0.555 [0.523–0.631]× |
| Pre-CRUD query `members_active_spatial_vector` | 2.160 [1.733–3.321]× | 2.151 [2.052–2.709]× |
| Pre-CRUD query `scalar_active_age` | 9.528 [7.662–14.657]× | 10.299 [8.483–10.583]× |
| Pre-CRUD query `spatial_bbox` | 0.359 [0.328–0.483]× | 0.359 [0.276–0.376]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 5.590 [4.170–5.775]× | 5.404 [5.037–5.577]× |
| Pre-CRUD query `text_positive_bm25_k10` | 0.838 [0.827–1.005]× | 0.901 [0.743–0.963]× |
| Pre-CRUD query `vector_cosine_k10` | 2.215 [1.927–2.523]× | 1.981 [1.849–2.110]× |
| Post-CRUD query `scalar_age` | 1.070 [0.943–1.095]× | 1.025 [0.693–1.218]× |
| Post-CRUD query `spatial` | 0.445 [0.050–0.637]× | 0.423 [0.046–0.598]× |
| Post-CRUD query `text` | 0.839 [0.585–1.342]× | 0.725 [0.567–1.186]× |
| Post-CRUD query `vector` | 1.982 [1.224–2.302]× | 1.985 [1.278–1.985]× |
| Reopen | 8.229 [7.445–10.519]× | 8.680 [8.323–10.276]× |

## N=10,000, dimension=32, reader=`held`

one snapshot held across all three CRUD rounds

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 0.291 [0.268–0.339] |
| Graph load | s | — | — | 0.339 [0.321–0.372] |
| Scalar late build | s | — | — | 0.015 [0.014–0.017] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 0.087 [0.087–0.089] |
| Text late build | s | — | — | 0.028 [0.025–0.029] |
| CRUD round 1 update | s | — | — | 1.899 [1.739–1.937] |
| CRUD round 1 delete | s | — | — | 0.320 [0.310–0.346] |
| CRUD round 1 reinsert+edges | s | — | — | 0.133 [0.127–0.134] |
| CRUD round 1 total | s | — | — | 2.343 [2.218–2.384] |
| CRUD round 2 update | s | — | — | 1.725 [1.694–1.811] |
| CRUD round 2 delete | s | — | — | 0.150 [0.141–0.167] |
| CRUD round 2 reinsert+edges | s | — | — | 0.134 [0.115–0.145] |
| CRUD round 2 total | s | — | — | 2.037 [1.950–2.095] |
| CRUD round 3 update | s | — | — | 1.849 [1.518–1.862] |
| CRUD round 3 delete | s | — | — | 0.174 [0.171–0.260] |
| CRUD round 3 reinsert+edges | s | — | — | 0.167 [0.152–0.210] |
| CRUD round 3 total | s | — | — | 2.203 [1.841–2.320] |
| CRUD all three rounds total | s | — | — | 6.640 [6.009–6.741] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.437 [0.408–0.478] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 0.479 [0.402–0.506] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.095 [0.085–0.165] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 1.140 [1.055–1.385] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 4.429 [4.399–4.978] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 3.102 [2.912–4.451] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 4.704 [4.410–5.411] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 7.036 [6.611–8.259] |
| Post-CRUD query `scalar_age` | ms | — | — | 0.088 [0.084–0.090] |
| Post-CRUD query `spatial` | ms | — | — | 0.979 [0.968–1.273] |
| Post-CRUD query `text` | ms | — | — | 5.798 [5.378–6.004] |
| Post-CRUD query `vector` | ms | — | — | 6.984 [5.855–7.594] |
| Reopen | ms | — | — | 0.868 [0.835–1.169] |
| Loaded logical | MiB | — | — | 4.059 [4.059–4.059] |
| Loaded allocated | MiB | — | — | 4.059 [4.059–4.059] |
| In-process final logical | MiB | — | — | 6.941 [6.941–6.941] |
| In-process final allocated | MiB | — | — | 7.160 [7.160–7.160] |
| After-close final logical | MiB | — | — | 6.723 [6.723–6.723] |
| After-close final allocated | MiB | — | — | 6.723 [6.723–6.723] |
| Sampled peak logical | MiB | — | — | 112.707 [112.707–112.707] |
| Sampled peak allocated | MiB | — | — | 133.520 [133.520–133.520] |
| Sampled peak / loaded logical | × | — | — | 27.770 [27.770–27.770] |
| Sampled peak / loaded allocated | × | — | — | 32.898 [32.898–32.898] |
| RSS high-water | MiB | — | — | 14.160 [14.117–14.324] |

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

## N=10,000, dimension=32, reader=`none`

no concurrent reader

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 0.451 [0.399–0.478] | 0.460 [0.399–0.672] | 0.290 [0.282–0.290] |
| Graph load | s | 0.903 [0.889–1.275] | 0.978 [0.848–1.463] | 0.343 [0.326–0.361] |
| Scalar late build | s | 0.207 [0.177–0.285] | 0.604 [0.532–0.945] | 0.018 [0.018–0.031] |
| Vector late build | s | 0.041 [0.039–0.061] | 0.178 [0.157–0.190] | N/A |
| Spatial late build | s | 0.087 [0.085–0.090] | 0.308 [0.218–0.326] | 0.087 [0.079–0.088] |
| Text late build | s | 1.837 [1.789–1.856] | 2.202 [2.164–2.214] | 0.026 [0.024–0.034] |
| CRUD round 1 update | s | 4.009 [3.822–4.032] | 3.920 [3.687–4.185] | 1.822 [1.755–1.864] |
| CRUD round 1 delete | s | 0.474 [0.470–0.522] | 0.519 [0.493–0.647] | 0.360 [0.342–0.396] |
| CRUD round 1 reinsert+edges | s | 0.486 [0.470–0.510] | 0.500 [0.487–0.547] | 0.138 [0.126–0.143] |
| CRUD round 1 total | s | 4.969 [4.762–5.064] | 5.067 [4.694–5.225] | 2.320 [2.294–2.333] |
| CRUD round 2 update | s | 3.591 [3.396–3.732] | 3.430 [3.424–3.637] | 1.801 [1.745–1.830] |
| CRUD round 2 delete | s | 0.405 [0.404–0.405] | 0.427 [0.415–0.447] | 0.153 [0.149–0.163] |
| CRUD round 2 reinsert+edges | s | 0.470 [0.428–0.509] | 0.500 [0.497–0.886] | 0.137 [0.117–0.143] |
| CRUD round 2 total | s | 4.466 [4.229–4.646] | 4.351 [4.342–4.969] | 2.097 [2.010–2.130] |
| CRUD round 3 update | s | 3.444 [3.432–3.581] | 3.339 [3.321–3.703] | 1.754 [1.697–1.813] |
| CRUD round 3 delete | s | 0.414 [0.408–0.434] | 0.428 [0.389–0.450] | 0.189 [0.176–0.217] |
| CRUD round 3 reinsert+edges | s | 0.507 [0.499–0.533] | 0.499 [0.484–0.503] | 0.193 [0.131–0.193] |
| CRUD round 3 total | s | 4.386 [4.345–4.522] | 4.266 [4.193–4.656] | 2.136 [2.004–2.223] |
| CRUD all three rounds total | s | 13.916 [13.336–14.137] | 13.833 [13.238–14.693] | 6.526 [6.454–6.566] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.206 [0.199–0.270] | 0.254 [0.246–0.330] | 0.423 [0.403–0.438] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 1.089 [1.017–1.302] | 1.021 [0.943–1.332] | 0.491 [0.232–0.522] |
| Pre-CRUD query `scalar_active_age` | ms | 1.443 [0.662–1.848] | 0.981 [0.664–1.059] | 0.110 [0.078–0.191] |
| Pre-CRUD query `spatial_bbox` | ms | 0.492 [0.413–0.561] | 0.455 [0.421–0.602] | 1.109 [1.021–1.150] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 4.753 [4.336–4.810] |
| Pre-CRUD query `text_active_vector` | ms | 15.219 [14.726–16.635] | 14.657 [13.176–16.811] | 2.998 [2.612–3.007] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 4.712 [4.473–5.205] | 4.943 [4.771–5.180] | 4.333 [4.255–4.483] |
| Pre-CRUD query `vector_cosine_k10` | ms | 14.584 [13.899–14.989] | 16.459 [10.631–16.788] | 6.764 [6.493–6.870] |
| Post-CRUD query `scalar_age` | ms | 0.112 [0.109–0.129] | 0.127 [0.113–0.145] | 0.102 [0.090–0.122] |
| Post-CRUD query `spatial` | ms | 0.569 [0.522–0.623] | 0.670 [0.603–0.681] | 1.104 [1.083–1.516] |
| Post-CRUD query `text` | ms | 4.623 [4.461–4.921] | 4.707 [4.697–5.173] | 5.549 [4.903–6.917] |
| Post-CRUD query `vector` | ms | 13.424 [13.179–14.725] | 13.127 [12.444–14.391] | 6.623 [6.617–7.907] |
| Reopen | ms | 10.724 [10.230–11.995] | 10.014 [9.846–10.813] | 1.130 [0.932–2.643] |
| Loaded logical | MiB | 4.508 [4.508–4.508] | 4.508 [4.508–4.508] | 4.059 [4.059–4.059] |
| Loaded allocated | MiB | 4.512 [4.512–4.512] | 4.512 [4.512–4.512] | 4.059 [4.059–4.059] |
| In-process final logical | MiB | 8.664 [8.664–8.664] | 8.664 [8.664–8.664] | 6.754 [6.754–6.754] |
| In-process final allocated | MiB | 8.668 [8.668–8.668] | 8.668 [8.668–8.668] | 6.754 [6.754–6.754] |
| After-close final logical | MiB | 8.664 [8.664–8.664] | 8.664 [8.664–8.664] | 6.723 [6.723–6.723] |
| After-close final allocated | MiB | 8.668 [8.668–8.668] | 8.668 [8.668–8.668] | 6.723 [6.723–6.723] |
| Sampled peak logical | MiB | 13.308 [13.158–13.308] | 13.004 [13.004–13.308] | 12.231 [12.231–12.231] |
| Sampled peak allocated | MiB | 16.617 [16.605–23.191] | 16.605 [16.605–20.629] | 14.754 [14.754–14.754] |
| Sampled peak / loaded logical | × | 2.952 [2.919–2.952] | 2.885 [2.885–2.952] | 3.014 [3.014–3.014] |
| Sampled peak / loaded allocated | × | 3.683 [3.681–5.140] | 3.681 [3.681–4.572] | 3.635 [3.635–3.635] |
| RSS high-water | MiB | 21.387 [21.375–21.457] | 21.164 [20.754–21.668] | 13.742 [13.734–13.930] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.603 [1.378–1.648]× | 1.587 [1.378–2.387]× |
| Graph load | 2.729 [2.506–3.711]× | 2.711 [2.470–4.490]× |
| Scalar late build | 11.168 [5.695–15.537]× | 30.446 [28.796–32.852]× |
| Vector late build | — | — |
| Spatial late build | 1.031 [0.979–1.068]× | 3.684 [2.503–3.877]× |
| Text late build | 71.269 [54.683–73.374]× | 83.925 [65.222–90.275]× |
| CRUD round 1 update | 2.178 [2.162–2.200]× | 2.103 [2.102–2.297]× |
| CRUD round 1 delete | 1.317 [1.187–1.524]× | 1.367 [1.310–1.890]× |
| CRUD round 1 reinsert+edges | 3.521 [3.285–4.038]× | 3.957 [3.403–3.966]× |
| CRUD round 1 total | 2.141 [2.076–2.170]× | 2.172 [2.046–2.252]× |
| CRUD round 2 update | 1.994 [1.947–2.040]× | 1.963 [1.875–2.019]× |
| CRUD round 2 delete | 2.643 [2.482–2.723]× | 2.877 [2.542–2.916]× |
| CRUD round 2 reinsert+edges | 3.672 [3.288–3.716]× | 4.289 [3.633–6.198]× |
| CRUD round 2 total | 2.130 [2.104–2.181]× | 2.165 [2.039–2.370]× |
| CRUD round 3 update | 1.963 [1.893–2.110]× | 1.967 [1.831–2.111]× |
| CRUD round 3 delete | 2.159 [1.906–2.467]× | 2.379 [1.792–2.433]× |
| CRUD round 3 reinsert+edges | 2.769 [2.590–3.881]× | 2.612 [2.509–3.821]× |
| CRUD round 3 total | 2.053 [1.954–2.257]× | 2.129 [1.886–2.180]× |
| CRUD all three rounds total | 2.119 [2.043–2.190]× | 2.143 [2.028–2.238]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.486 [0.453–0.669]× | 0.582 [0.580–0.820]× |
| Pre-CRUD query `members_active_spatial_vector` | 2.087 [2.069–5.602]× | 2.552 [1.919–4.392]× |
| Pre-CRUD query `scalar_active_age` | 7.557 [6.041–23.821]× | 8.556 [5.544–8.956]× |
| Pre-CRUD query `spatial_bbox` | 0.428 [0.373–0.550]× | 0.446 [0.379–0.524]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 5.533 [4.913–5.827]× | 4.875 [4.396–6.436]× |
| Pre-CRUD query `text_positive_bm25_k10` | 1.108 [1.032–1.161]× | 1.121 [1.103–1.195]× |
| Pre-CRUD query `vector_cosine_k10` | 2.156 [2.023–2.309]× | 2.433 [1.547–2.586]× |
| Post-CRUD query `scalar_age` | 1.216 [0.920–1.257]× | 1.243 [1.192–1.254]× |
| Post-CRUD query `spatial` | 0.473 [0.375–0.575]× | 0.557 [0.449–0.607]× |
| Post-CRUD query `text` | 0.804 [0.712–0.943]× | 0.848 [0.679–1.055]× |
| Post-CRUD query `vector` | 2.029 [1.667–2.223]× | 1.879 [1.660–2.175]× |
| Reopen | 10.615 [4.058–10.979]× | 8.863 [3.725–11.604]× |

## N=10,000, dimension=32, reader=`short`

one snapshot held across each complete CRUD round

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 0.258 [0.240–0.277] |
| Graph load | s | — | — | 0.307 [0.264–0.330] |
| Scalar late build | s | — | — | 0.018 [0.016–0.021] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 0.084 [0.083–0.093] |
| Text late build | s | — | — | 0.030 [0.027–0.032] |
| CRUD round 1 update | s | — | — | 1.743 [1.742–1.826] |
| CRUD round 1 delete | s | — | — | 0.331 [0.319–0.337] |
| CRUD round 1 reinsert+edges | s | — | — | 0.132 [0.131–0.135] |
| CRUD round 1 total | s | — | — | 2.211 [2.195–2.289] |
| CRUD round 2 update | s | — | — | 1.814 [1.696–1.977] |
| CRUD round 2 delete | s | — | — | 0.180 [0.152–0.202] |
| CRUD round 2 reinsert+edges | s | — | — | 0.142 [0.138–0.143] |
| CRUD round 2 total | s | — | — | 2.158 [1.986–2.301] |
| CRUD round 3 update | s | — | — | 1.730 [1.631–1.758] |
| CRUD round 3 delete | s | — | — | 0.172 [0.169–0.175] |
| CRUD round 3 reinsert+edges | s | — | — | 0.147 [0.144–0.154] |
| CRUD round 3 total | s | — | — | 2.054 [1.950–2.077] |
| CRUD all three rounds total | s | — | — | 6.500 [6.148–6.573] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.460 [0.384–0.499] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 0.300 [0.286–0.485] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.087 [0.081–0.223] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 1.262 [1.019–1.353] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 4.591 [3.878–4.639] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 3.055 [2.467–3.742] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 4.713 [4.075–4.890] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 6.440 [6.212–8.508] |
| Post-CRUD query `scalar_age` | ms | — | — | 0.090 [0.088–0.096] |
| Post-CRUD query `spatial` | ms | — | — | 0.985 [0.965–1.109] |
| Post-CRUD query `text` | ms | — | — | 4.875 [4.844–4.976] |
| Post-CRUD query `vector` | ms | — | — | 5.928 [5.810–6.824] |
| Reopen | ms | — | — | 1.015 [0.980–1.033] |
| Loaded logical | MiB | — | — | 4.059 [4.059–4.059] |
| Loaded allocated | MiB | — | — | 4.059 [4.059–4.059] |
| In-process final logical | MiB | — | — | 6.941 [6.941–6.941] |
| In-process final allocated | MiB | — | — | 7.160 [7.160–7.160] |
| After-close final logical | MiB | — | — | 6.723 [6.723–6.723] |
| After-close final allocated | MiB | — | — | 6.723 [6.723–6.723] |
| Sampled peak logical | MiB | — | — | 114.027 [114.027–114.027] |
| Sampled peak allocated | MiB | — | — | 173.066 [172.805–173.160] |
| Sampled peak / loaded logical | × | — | — | 28.095 [28.095–28.095] |
| Sampled peak / loaded allocated | × | — | — | 42.642 [42.577–42.665] |
| RSS high-water | MiB | — | — | 14.066 [13.789–14.242] |

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
| 10,000/32 | `held` | e4 atomic / 0 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `held` | e4 atomic / 1 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `held` | e4 atomic / 2 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `held` | e4 resumable / 0 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `held` | e4 resumable / 1 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `held` | e4 resumable / 2 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 atomic / 0 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 atomic / 1 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 atomic / 2 | crud_update | 108 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 resumable / 0 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 resumable / 1 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 10,000/32 | `short` | e4 resumable / 2 | crud_update | 304 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[5888,0,0],"entities":10000,"relationships":30000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
