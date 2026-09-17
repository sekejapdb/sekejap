# Phase 2 multimodel benchmark report

**Evidence state:** INCOMPLETE — candidate evidence only, not public acceptance

- Raw report SHA-256: `f21b5260001c7e55da41a609b4ae117c8af8acf0a7b4cb36690a4fc1efcf702f`
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

## N=2,000, dimension=1536, reader=`batch`

one snapshot held across the first 256-row update commit of each CRUD round, then released

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 0.354 [0.344–0.441] | 0.393 [0.337–0.490] | 0.198 [0.183–0.225] |
| Graph load | s | 0.209 [0.185–0.217] | 0.242 [0.194–0.466] | 0.083 [0.078–0.088] |
| Scalar late build | s | 0.096 [0.095–0.102] | 0.149 [0.131–0.160] | 0.029 [0.028–0.032] |
| Vector late build | s | 0.032 [0.032–0.039] | 0.058 [0.052–0.065] | N/A |
| Spatial late build | s | 0.017 [0.017–0.020] | 0.053 [0.033–0.082] | 0.048 [0.045–0.048] |
| Text late build | s | 0.358 [0.337–0.363] | 0.401 [0.383–0.403] | 0.035 [0.031–0.036] |
| CRUD round 1 update | s | 1.606 [1.494–1.633] | 1.517 [1.492–1.589] | 0.700 [0.678–0.724] |
| CRUD round 1 delete | s | 0.095 [0.092–0.110] | 0.092 [0.090–0.100] | 0.089 [0.087–0.090] |
| CRUD round 1 reinsert+edges | s | 0.165 [0.156–0.176] | 0.162 [0.145–0.166] | 0.065 [0.064–0.066] |
| CRUD round 1 total | s | 1.875 [1.754–1.899] | 1.773 [1.736–1.842] | 0.854 [0.832–0.878] |
| CRUD round 2 update | s | 1.436 [1.406–1.561] | 1.466 [1.447–1.511] | 0.668 [0.665–0.751] |
| CRUD round 2 delete | s | 0.079 [0.077–0.085] | 0.081 [0.074–0.082] | 0.066 [0.062–0.079] |
| CRUD round 2 reinsert+edges | s | 0.153 [0.150–0.174] | 0.152 [0.152–0.175] | 0.035 [0.032–0.044] |
| CRUD round 2 total | s | 1.687 [1.639–1.796] | 1.723 [1.674–1.745] | 0.777 [0.759–0.864] |
| CRUD round 3 update | s | 1.508 [1.477–1.592] | 1.441 [1.420–1.646] | 0.637 [0.575–0.735] |
| CRUD round 3 delete | s | 0.079 [0.075–0.097] | 0.075 [0.071–0.086] | 0.069 [0.067–0.096] |
| CRUD round 3 reinsert+edges | s | 0.143 [0.142–0.157] | 0.147 [0.135–0.157] | 0.033 [0.032–0.033] |
| CRUD round 3 total | s | 1.762 [1.695–1.813] | 1.653 [1.647–1.879] | 0.739 [0.676–0.862] |
| CRUD all three rounds total | s | 5.350 [5.136–5.432] | 5.240 [5.143–5.288] | 2.480 [2.267–2.494] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.398 [0.396–0.404] | 0.353 [0.329–0.354] | 0.652 [0.560–0.734] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 0.637 [0.626–0.717] | 0.569 [0.529–0.604] | 0.498 [0.315–0.519] |
| Pre-CRUD query `scalar_active_age` | ms | 0.375 [0.369–0.398] | 0.391 [0.352–0.430] | 0.225 [0.207–0.249] |
| Pre-CRUD query `spatial_bbox` | ms | 0.132 [0.129–0.134] | 0.121 [0.103–0.209] | 2.137 [2.035–2.159] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 1.076 [1.016–1.170] |
| Pre-CRUD query `text_active_vector` | ms | 11.932 [11.219–12.009] | 10.034 [9.636–10.825] | 5.703 [4.948–5.866] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 1.188 [1.012–1.892] | 0.999 [0.806–1.002] | 2.346 [2.232–2.475] |
| Pre-CRUD query `vector_cosine_k10` | ms | 35.044 [33.649–38.257] | 33.436 [33.321–33.989] | 27.503 [26.606–30.121] |
| Post-CRUD query `scalar_age` | ms | 0.123 [0.107–0.141] | 0.146 [0.130–0.165] | 0.087 [0.083–0.100] |
| Post-CRUD query `spatial` | ms | 0.254 [0.251–0.255] | 0.246 [0.238–0.247] | 2.197 [2.112–2.208] |
| Post-CRUD query `text` | ms | 1.308 [1.140–1.364] | 1.251 [1.177–1.586] | 3.727 [3.722–3.989] |
| Post-CRUD query `vector` | ms | 71.357 [70.248–72.681] | 66.408 [65.445–90.902] | 28.584 [28.352–30.034] |
| Reopen | ms | 9.255 [8.051–9.464] | 9.557 [8.771–10.763] | 1.239 [1.188–1.247] |
| Loaded logical | MiB | 16.402 [16.402–16.402] | 16.402 [16.402–16.402] | 15.953 [15.953–15.953] |
| Loaded allocated | MiB | 16.406 [16.406–16.406] | 16.406 [16.406–16.406] | 15.953 [15.953–15.953] |
| In-process final logical | MiB | 17.281 [17.281–17.281] | 17.281 [17.281–17.281] | 16.516 [16.516–16.516] |
| In-process final allocated | MiB | 17.285 [17.285–17.285] | 17.285 [17.285–17.285] | 16.516 [16.516–16.516] |
| After-close final logical | MiB | 17.281 [17.281–17.281] | 17.281 [17.281–17.281] | 16.484 [16.484–16.484] |
| After-close final allocated | MiB | 17.285 [17.285–17.285] | 17.285 [17.285–17.285] | 16.484 [16.484–16.484] |
| Sampled peak logical | MiB | 23.956 [23.956–23.956] | 23.956 [23.956–23.956] | 23.796 [23.796–23.796] |
| Sampled peak allocated | MiB | 25.223 [25.223–25.223] | 28.316 [25.223–57.129] | 28.094 [27.078–32.031] |
| Sampled peak / loaded logical | × | 1.461 [1.461–1.461] | 1.461 [1.461–1.461] | 1.492 [1.492–1.492] |
| Sampled peak / loaded allocated | × | 1.537 [1.537–1.537] | 1.726 [1.537–3.482] | 1.761 [1.697–2.008] |
| RSS high-water | MiB | 21.500 [21.496–21.805] | 21.379 [21.375–21.770] | 14.699 [14.602–14.703] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.937 [1.739–1.960]× | 1.986 [1.846–2.175]× |
| Graph load | 2.383 [2.371–2.618]× | 3.094 [2.337–5.323]× |
| Scalar late build | 3.291 [3.228–3.432]× | 4.701 [4.538–5.735]× |
| Vector late build | — | — |
| Spatial late build | 0.390 [0.348–0.410]× | 1.186 [0.682–1.716]× |
| Text late build | 10.081 [9.324–11.522]× | 11.300 [11.151–12.182]× |
| CRUD round 1 update | 2.293 [2.063–2.407]× | 2.199 [2.096–2.268]× |
| CRUD round 1 delete | 1.063 [1.029–1.267]× | 1.025 [1.009–1.145]× |
| CRUD round 1 reinsert+edges | 2.562 [2.350–2.767]× | 2.540 [2.183–2.565]× |
| CRUD round 1 total | 2.195 [1.998–2.283]× | 2.088 [2.020–2.157]× |
| CRUD round 2 update | 2.114 [1.914–2.337]× | 2.175 [1.953–2.263]× |
| CRUD round 2 delete | 1.286 [0.971–1.289]× | 1.203 [1.031–1.244]× |
| CRUD round 2 reinsert+edges | 4.726 [3.435–5.029]× | 4.708 [3.473–5.068]× |
| CRUD round 2 total | 2.158 [1.952–2.310]× | 2.204 [1.993–2.244]× |
| CRUD round 3 update | 2.320 [2.053–2.766]× | 2.263 [1.933–2.861]× |
| CRUD round 3 delete | 1.098 [1.013–1.172]× | 1.033 [0.893–1.115]× |
| CRUD round 3 reinsert+edges | 4.310 [4.287–4.944]× | 4.638 [4.068–4.765]× |
| CRUD round 3 total | 2.295 [2.043–2.682]× | 2.230 [1.917–2.780]× |
| CRUD all three rounds total | 2.178 [2.071–2.360]× | 2.101 [2.073–2.333]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.611 [0.551–0.707]× | 0.542 [0.448–0.630]× |
| Pre-CRUD query `members_active_spatial_vector` | 1.257 [1.228–2.272]× | 1.142 [1.019–1.916]× |
| Pre-CRUD query `scalar_active_age` | 1.641 [1.503–1.917]× | 1.568 [1.566–2.075]× |
| Pre-CRUD query `spatial_bbox` | 0.063 [0.061–0.063]× | 0.059 [0.048–0.098]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 2.106 [1.912–2.411]× | 1.759 [1.643–2.188]× |
| Pre-CRUD query `text_positive_bm25_k10` | 0.480 [0.431–0.848]× | 0.404 [0.344–0.449]× |
| Pre-CRUD query `vector_cosine_k10` | 1.265 [1.163–1.391]× | 1.212 [1.128–1.257]× |
| Post-CRUD query `scalar_age` | 1.409 [1.233–1.480]× | 1.646 [1.505–1.757]× |
| Post-CRUD query `spatial` | 0.115 [0.114–0.120]× | 0.111 [0.109–0.117]× |
| Post-CRUD query `text` | 0.342 [0.306–0.351]× | 0.336 [0.295–0.426]× |
| Post-CRUD query `vector` | 2.496 [2.339–2.563]× | 2.323 [2.179–3.206]× |
| Reopen | 7.468 [6.458–7.968]× | 7.666 [7.077–9.061]× |

## N=2,000, dimension=1536, reader=`held`

one snapshot held across all three CRUD rounds

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 0.238 [0.214–0.286] |
| Graph load | s | — | — | 0.117 [0.077–0.118] |
| Scalar late build | s | — | — | 0.033 [0.028–0.044] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 0.045 [0.042–0.051] |
| Text late build | s | — | — | 0.035 [0.033–0.052] |
| CRUD round 1 update | s | — | — | 0.706 [0.703–0.909] |
| CRUD round 1 delete | s | — | — | 0.097 [0.089–0.109] |
| CRUD round 1 reinsert+edges | s | — | — | 0.049 [0.043–0.066] |
| CRUD round 1 total | s | — | — | 0.878 [0.851–1.041] |
| CRUD round 2 update | s | — | — | 0.610 [0.600–0.646] |
| CRUD round 2 delete | s | — | — | 0.078 [0.069–0.082] |
| CRUD round 2 reinsert+edges | s | — | — | 0.043 [0.043–0.046] |
| CRUD round 2 total | s | — | — | 0.735 [0.721–0.762] |
| CRUD round 3 update | s | — | — | 0.596 [0.571–0.617] |
| CRUD round 3 delete | s | — | — | 0.076 [0.068–0.077] |
| CRUD round 3 reinsert+edges | s | — | — | 0.049 [0.047–0.050] |
| CRUD round 3 total | s | — | — | 0.719 [0.689–0.742] |
| CRUD all three rounds total | s | — | — | 2.355 [2.332–2.451] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.668 [0.607–0.750] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 0.602 [0.326–0.824] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.288 [0.223–0.308] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 2.339 [2.091–6.129] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 1.278 [1.229–1.393] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 6.116 [4.792–8.789] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 2.357 [2.308–2.714] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 30.228 [28.476–36.555] |
| Post-CRUD query `scalar_age` | ms | — | — | 0.096 [0.087–0.128] |
| Post-CRUD query `spatial` | ms | — | — | 2.684 [2.380–3.017] |
| Post-CRUD query `text` | ms | — | — | 5.856 [5.386–5.979] |
| Post-CRUD query `vector` | ms | — | — | 34.292 [34.093–34.473] |
| Reopen | ms | — | — | 1.199 [1.069–1.226] |
| Loaded logical | MiB | — | — | 15.953 [15.953–15.953] |
| Loaded allocated | MiB | — | — | 15.953 [15.953–15.953] |
| In-process final logical | MiB | — | — | 16.609 [16.609–16.609] |
| In-process final allocated | MiB | — | — | 16.672 [16.672–16.672] |
| After-close final logical | MiB | — | — | 16.484 [16.484–16.484] |
| After-close final allocated | MiB | — | — | 16.484 [16.484–16.484] |
| Sampled peak logical | MiB | — | — | 80.224 [80.224–80.224] |
| Sampled peak allocated | MiB | — | — | 80.613 [80.285–80.613] |
| Sampled peak / loaded logical | × | — | — | 5.029 [5.029–5.029] |
| Sampled peak / loaded allocated | × | — | — | 5.053 [5.033–5.053] |
| RSS high-water | MiB | — | — | 14.812 [14.758–14.887] |

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

## N=2,000, dimension=1536, reader=`none`

no concurrent reader

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 3 | 0 | atomic |
| E4 resumable | 3 | 3 | 0 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | 0.415 [0.375–1.248] | 0.466 [0.417–1.016] | 0.405 [0.198–1.465] |
| Graph load | s | 0.310 [0.194–0.323] | 0.270 [0.181–0.299] | 0.111 [0.073–0.160] |
| Scalar late build | s | 0.153 [0.098–0.159] | 0.165 [0.138–0.494] | 0.034 [0.031–0.086] |
| Vector late build | s | 0.036 [0.035–0.323] | 0.096 [0.055–0.583] | N/A |
| Spatial late build | s | 0.018 [0.018–0.022] | 0.058 [0.055–0.161] | 0.051 [0.045–0.062] |
| Text late build | s | 0.381 [0.380–0.411] | 0.413 [0.389–0.864] | 0.037 [0.037–0.126] |
| CRUD round 1 update | s | 1.912 [1.635–5.026] | 1.638 [1.519–2.994] | 0.911 [0.655–1.169] |
| CRUD round 1 delete | s | 0.103 [0.094–0.114] | 0.101 [0.094–0.157] | 0.090 [0.078–0.201] |
| CRUD round 1 reinsert+edges | s | 0.258 [0.174–0.999] | 0.197 [0.145–0.243] | 0.079 [0.056–0.362] |
| CRUD round 1 total | s | 2.284 [1.903–6.128] | 1.878 [1.817–3.394] | 1.081 [0.789–1.733] |
| CRUD round 2 update | s | 1.852 [1.722–3.812] | 1.782 [1.500–2.452] | 0.763 [0.635–1.179] |
| CRUD round 2 delete | s | 0.091 [0.086–0.119] | 0.084 [0.081–0.129] | 0.081 [0.073–0.092] |
| CRUD round 2 reinsert+edges | s | 0.163 [0.155–0.422] | 0.205 [0.203–0.226] | 0.054 [0.037–0.061] |
| CRUD round 2 total | s | 2.106 [1.963–4.353] | 2.089 [1.789–2.784] | 0.898 [0.745–1.332] |
| CRUD round 3 update | s | 1.885 [1.665–4.993] | 1.653 [1.485–1.676] | 0.701 [0.593–1.889] |
| CRUD round 3 delete | s | 0.084 [0.081–0.113] | 0.075 [0.071–0.084] | 0.081 [0.069–0.123] |
| CRUD round 3 reinsert+edges | s | 0.154 [0.145–0.215] | 0.152 [0.142–0.174] | 0.133 [0.059–0.265] |
| CRUD round 3 total | s | 2.124 [1.890–5.322] | 1.903 [1.698–1.911] | 0.829 [0.807–2.278] |
| CRUD all three rounds total | s | 6.513 [5.756–15.802] | 5.817 [5.365–8.081] | 2.807 [2.341–5.342] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | 0.373 [0.344–0.540] | 0.345 [0.312–0.393] | 0.778 [0.533–1.123] |
| Pre-CRUD query `members_active_spatial_vector` | ms | 0.663 [0.623–0.665] | 0.655 [0.608–0.787] | 0.511 [0.415–0.539] |
| Pre-CRUD query `scalar_active_age` | ms | 0.379 [0.331–0.449] | 0.650 [0.312–1.198] | 0.256 [0.229–0.276] |
| Pre-CRUD query `spatial_bbox` | ms | 0.149 [0.130–0.162] | 0.127 [0.126–0.129] | 2.382 [2.157–2.921] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 1.240 [0.961–1.291] |
| Pre-CRUD query `text_active_vector` | ms | 11.965 [11.818–14.618] | 11.412 [9.988–11.637] | 6.431 [5.130–6.601] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | 1.011 [0.842–1.028] | 0.988 [0.939–1.462] | 2.575 [2.204–2.795] |
| Pre-CRUD query `vector_cosine_k10` | ms | 38.559 [35.050–43.492] | 35.407 [31.866–61.947] | 31.022 [29.211–34.400] |
| Post-CRUD query `scalar_age` | ms | 0.130 [0.118–0.140] | 0.133 [0.095–0.142] | 0.093 [0.083–0.531] |
| Post-CRUD query `spatial` | ms | 0.266 [0.265–0.287] | 0.261 [0.250–0.261] | 2.263 [1.981–2.919] |
| Post-CRUD query `text` | ms | 1.235 [1.200–1.368] | 1.284 [1.130–1.298] | 3.098 [2.988–3.816] |
| Post-CRUD query `vector` | ms | 71.008 [70.956–74.544] | 69.429 [69.116–73.325] | 29.964 [29.019–35.162] |
| Reopen | ms | 10.111 [7.317–211.904] | 10.590 [9.486–11.425] | 1.027 [0.942–8.644] |
| Loaded logical | MiB | 16.402 [16.402–16.402] | 16.402 [16.402–16.402] | 15.953 [15.953–15.953] |
| Loaded allocated | MiB | 16.406 [16.406–16.406] | 16.406 [16.406–16.406] | 15.953 [15.953–15.953] |
| In-process final logical | MiB | 17.281 [17.281–17.281] | 17.281 [17.281–17.281] | 16.516 [16.516–16.516] |
| In-process final allocated | MiB | 17.285 [17.285–17.285] | 17.285 [17.285–17.285] | 16.516 [16.516–16.516] |
| After-close final logical | MiB | 17.281 [17.281–17.281] | 17.281 [17.281–17.281] | 16.484 [16.484–16.484] |
| After-close final allocated | MiB | 17.285 [17.285–17.285] | 17.285 [17.285–17.285] | 16.484 [16.484–16.484] |
| Sampled peak logical | MiB | 23.956 [23.956–23.956] | 23.956 [23.956–23.956] | 21.502 [21.502–21.502] |
| Sampled peak allocated | MiB | 57.191 [25.223–57.191] | 52.316 [25.379–57.004] | 32.031 [24.516–36.094] |
| Sampled peak / loaded logical | × | 1.461 [1.461–1.461] | 1.461 [1.461–1.461] | 1.348 [1.348–1.348] |
| Sampled peak / loaded allocated | × | 3.486 [1.537–3.486] | 3.189 [1.547–3.475] | 2.008 [1.537–2.262] |
| RSS high-water | MiB | 20.898 [20.836–20.980] | 21.344 [21.227–21.344] | 14.703 [14.316–14.750] |

Ratios pair the same three trial numbers and are emitted only when both arms completed all three. E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.

| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |
|---|---:|---:|
| Entity load | 1.026 [0.851–1.890]× | 1.152 [0.693–2.103]× |
| Graph load | 2.676 [2.015–2.806]× | 2.442 [1.871–2.499]× |
| Scalar late build | 3.207 [1.859–4.528]× | 4.884 [4.525–5.766]× |
| Vector late build | — | — |
| Spatial late build | 0.361 [0.357–0.412]× | 1.231 [1.134–2.605]× |
| Text late build | 10.265 [3.271–10.314]× | 10.475 [6.885–11.202]× |
| CRUD round 1 update | 2.496 [2.098–4.298]× | 2.319 [1.797–2.560]× |
| CRUD round 1 delete | 1.206 [0.513–1.257]× | 1.045 [0.781–1.296]× |
| CRUD round 1 reinsert+edges | 3.090 [2.759–3.276]× | 1.841 [0.671–3.498]× |
| CRUD round 1 total | 2.412 [2.113–3.537]× | 1.959 [1.738–2.303]× |
| CRUD round 2 update | 2.714 [2.428–3.234]× | 2.081 [1.967–2.807]× |
| CRUD round 2 delete | 1.166 [1.125–1.290]× | 1.105 [1.042–1.401]× |
| CRUD round 2 reinsert+edges | 4.199 [3.020–6.956]× | 3.789 [3.346–6.148]× |
| CRUD round 2 total | 2.634 [2.346–3.269]× | 2.091 [1.993–2.804]× |
| CRUD round 3 update | 2.689 [2.643–2.807]× | 2.118 [0.887–2.786]× |
| CRUD round 3 delete | 0.995 [0.915–1.227]× | 1.029 [0.609–1.039]× |
| CRUD round 3 reinsert+edges | 1.090 [0.812–2.603]× | 1.312 [0.573–2.395]× |
| CRUD round 3 total | 2.342 [2.336–2.561]× | 2.049 [0.836–2.367]× |
| CRUD all three rounds total | 2.459 [2.320–2.958]× | 1.911 [1.513–2.485]× |
| Pre-CRUD query `combined_graph_active_bbox_vector` | 0.442 [0.332–1.015]× | 0.443 [0.350–0.585]× |
| Pre-CRUD query `members_active_spatial_vector` | 1.231 [1.220–1.602]× | 1.460 [1.282–1.462]× |
| Pre-CRUD query `scalar_active_age` | 1.374 [1.292–1.966]× | 2.540 [1.364–4.338]× |
| Pre-CRUD query `spatial_bbox` | 0.055 [0.051–0.075]× | 0.053 [0.044–0.059]× |
| Pre-CRUD query `sqlite_native_bm25_k10` | — | — |
| Pre-CRUD query `text_active_vector` | 2.273 [1.813–2.303]× | 1.775 [1.763–1.947]× |
| Pre-CRUD query `text_positive_bm25_k10` | 0.382 [0.368–0.393]× | 0.426 [0.354–0.568]× |
| Pre-CRUD query `vector_cosine_k10` | 1.200 [1.121–1.402]× | 1.091 [1.029–1.997]× |
| Post-CRUD query `scalar_age` | 1.387 [0.264–1.433]× | 1.020 [0.267–1.614]× |
| Post-CRUD query `spatial` | 0.117 [0.091–0.145]× | 0.115 [0.089–0.126]× |
| Post-CRUD query `text` | 0.399 [0.314–0.458]× | 0.414 [0.296–0.434]× |
| Post-CRUD query `vector` | 2.445 [2.019–2.488]× | 2.317 [1.966–2.527]× |
| Reopen | 10.734 [7.125–24.513]× | 9.238 [1.322–11.242]× |

## N=2,000, dimension=1536, reader=`short`

one snapshot held across each complete CRUD round

| Arm | Captured trials | Completed | Refused | Publication policy |
|---|---:|---:|---:|---|
| E4 atomic | 3 | 0 | 3 | atomic |
| E4 resumable | 3 | 0 | 3 | resumable |
| SQLite atomic | 3 | 3 | 0 | native SQLite atomic DDL |

Values are medians [min–max] from completed arms only.

| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |
|---|---:|---:|---:|---:|
| Entity load | s | — | — | 0.236 [0.186–0.251] |
| Graph load | s | — | — | 0.100 [0.087–0.111] |
| Scalar late build | s | — | — | 0.036 [0.033–0.036] |
| Vector late build | s | — | — | N/A |
| Spatial late build | s | — | — | 0.045 [0.044–0.047] |
| Text late build | s | — | — | 0.036 [0.033–0.042] |
| CRUD round 1 update | s | — | — | 0.655 [0.600–0.671] |
| CRUD round 1 delete | s | — | — | 0.092 [0.091–0.098] |
| CRUD round 1 reinsert+edges | s | — | — | 0.045 [0.044–0.046] |
| CRUD round 1 total | s | — | — | 0.792 [0.744–0.806] |
| CRUD round 2 update | s | — | — | 0.679 [0.623–0.689] |
| CRUD round 2 delete | s | — | — | 0.072 [0.070–0.089] |
| CRUD round 2 reinsert+edges | s | — | — | 0.045 [0.038–0.055] |
| CRUD round 2 total | s | — | — | 0.796 [0.748–0.815] |
| CRUD round 3 update | s | — | — | 0.609 [0.595–0.653] |
| CRUD round 3 delete | s | — | — | 0.070 [0.066–0.074] |
| CRUD round 3 reinsert+edges | s | — | — | 0.042 [0.041–0.058] |
| CRUD round 3 total | s | — | — | 0.723 [0.717–0.768] |
| CRUD all three rounds total | s | — | — | 2.308 [2.263–2.338] |
| Pre-CRUD query `combined_graph_active_bbox_vector` | ms | — | — | 0.613 [0.584–0.652] |
| Pre-CRUD query `members_active_spatial_vector` | ms | — | — | 0.449 [0.407–0.538] |
| Pre-CRUD query `scalar_active_age` | ms | — | — | 0.235 [0.226–0.240] |
| Pre-CRUD query `spatial_bbox` | ms | — | — | 2.323 [2.138–3.375] |
| Pre-CRUD query `sqlite_native_bm25_k10` | ms | — | — | 1.230 [1.224–1.501] |
| Pre-CRUD query `text_active_vector` | ms | — | — | 5.752 [5.237–6.712] |
| Pre-CRUD query `text_positive_bm25_k10` | ms | — | — | 2.092 [1.859–2.600] |
| Pre-CRUD query `vector_cosine_k10` | ms | — | — | 26.848 [25.256–29.397] |
| Post-CRUD query `scalar_age` | ms | — | — | 0.089 [0.080–0.567] |
| Post-CRUD query `spatial` | ms | — | — | 2.738 [2.478–3.048] |
| Post-CRUD query `text` | ms | — | — | 5.676 [5.401–5.994] |
| Post-CRUD query `vector` | ms | — | — | 32.328 [31.765–35.929] |
| Reopen | ms | — | — | 1.061 [0.842–1.144] |
| Loaded logical | MiB | — | — | 15.953 [15.953–15.953] |
| Loaded allocated | MiB | — | — | 15.953 [15.953–15.953] |
| In-process final logical | MiB | — | — | 16.609 [16.609–16.609] |
| In-process final allocated | MiB | — | — | 16.672 [16.672–16.672] |
| After-close final logical | MiB | — | — | 16.484 [16.484–16.484] |
| After-close final allocated | MiB | — | — | 16.484 [16.484–16.484] |
| Sampled peak logical | MiB | — | — | 80.513 [80.513–80.513] |
| Sampled peak allocated | MiB | — | — | 103.230 [103.230–103.234] |
| Sampled peak / loaded logical | × | — | — | 5.047 [5.047–5.047] |
| Sampled peak / loaded allocated | × | — | — | 6.471 [6.471–6.471] |
| RSS high-water | MiB | — | — | 14.945 [14.945–14.973] |

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
| 2,000/1536 | `held` | e4 atomic / 0 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `held` | e4 atomic / 1 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `held` | e4 atomic / 2 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `held` | e4 resumable / 0 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `held` | e4 resumable / 1 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `held` | e4 resumable / 2 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 atomic / 0 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 atomic / 1 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 atomic / 2 | crud_update | 23 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 resumable / 0 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 resumable / 1 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
| 2,000/1536 | `short` | e4 resumable / 2 | crud_update | 59 | `{"crud_deleted":[0,0,0],"crud_edges_restored":[0,0,0],"crud_reinserted":[0,0,0],"crud_updated":[512,0,0],"entities":2000,"relationships":6000}` | yes | Kernel(ResourceLimit("page-WAL managed-byte allowance")) |
