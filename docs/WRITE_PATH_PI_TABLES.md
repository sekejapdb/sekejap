# WRITE PATH PI benchmark tables — 2026-09-12

MiB = 1,048,576 bytes; times are seconds. All three arms shown: baseline (original E4),
current (new E4), and sqlite (SQLite). Peak includes all database files, WAL, reader release
and schema alteration; 1 ms samples are lower bounds, not caps.

| Run | Arm | Load s | Churn s | Loaded MiB | Peak MiB | Churn final MiB | Allocated peak MiB | Logical factor | Allocated factor |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-400000 | baseline | 28.992 | 178.741 | 126.684 | 140.027 | 139.769 | 140.035 | 1.105× | 1.105× |
| mixed-400000 | current | 27.755 | 161.576 | 126.684 | 139.596 | 139.589 | 139.602 | 1.102× | 1.102× |
| mixed-400000 | sqlite | 8.796 | 76.289 | 93.266 | 121.840 | 117.129 | 121.848 | 1.306× | 1.306× |
| vectors-stable | baseline | 2.092 | 2.943 | 81.387 | 105.841 | 99.726 | 105.844 | 1.300× | 1.300× |
| vectors-stable | current | 2.206 | 1.045 | 81.387 | 87.413 | 83.769 | 87.422 | 1.074× | 1.074× |
| vectors-stable | sqlite | 2.874 | 2.350 | 84.027 | 92.546 | 84.035 | 92.551 | 1.101× | 1.101× |
| vectors-changing | baseline | 2.111 | 2.945 | 81.387 | 105.841 | 99.726 | 105.844 | 1.300× | 1.300× |
| vectors-changing | current | 2.110 | 2.817 | 81.387 | 105.812 | 99.726 | 105.816 | 1.300× | 1.300× |
| vectors-changing | sqlite | 2.842 | 4.948 | 84.027 | 94.711 | 84.035 | 94.711 | 1.127× | 1.127× |
| vectors-held | baseline | 2.105 | 2.071 | 81.387 | 124.312 | 118.143 | 124.320 | 1.527× | 1.527× |
| vectors-held | current | 2.171 | 0.832 | 81.387 | 87.413 | 86.241 | 87.422 | 1.074× | 1.074× |
| vectors-held | sqlite | 2.823 | 21.395 | 84.027 | 109.747 | 109.739 | 109.754 | 1.306× | 1.306× |

## E4 issued writes during churn

Bytes handed to the storage writers, including rewritten/reused pages; these are not file sizes or device-level NAND writes. SQLite issued bytes were not instrumented.

| Run | Arm | Data MiB | WAL MiB | Sidecars MiB | Total MiB |
|---|---|---:|---:|---:|---:|
| mixed-400000 | baseline | 1943.523 | 240.033 | 12.882 | 2196.439 |
| mixed-400000 | current | 1789.340 | 0.000 | 11.955 | 1801.294 |
| vectors-stable | baseline | 73.598 | 48.296 | 0.620 | 122.514 |
| vectors-stable | current | 9.957 | 0.000 | 0.084 | 10.041 |
| vectors-changing | baseline | 73.598 | 48.296 | 0.620 | 122.514 |
| vectors-changing | current | 73.598 | 47.633 | 0.620 | 121.851 |
| vectors-held | baseline | 73.598 | 48.296 | 1.077 | 122.971 |
| vectors-held | current | 9.957 | 0.000 | 0.146 | 10.103 |

## Reader release

| Run | Arm | Release s | Logical retained MiB | Allocated retained MiB |
|---|---|---:|---:|---:|
| vectors-held | baseline | 0.019 | 118.143 | 118.145 |
| vectors-held | current | 0.013 | 86.241 | 86.242 |
| vectors-held | sqlite | 0.352 | 84.066 | 84.066 |

## Phase detail

| Run | Arm | Cycle | Operations | Time s | Logical peak MiB | Logical final MiB | Allocated peak MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| mixed-400000 | baseline | 0 | 400000 | 28.992 | 126.935 | 126.684 | 126.945 |
| mixed-400000 | baseline | 1 | 160000 | 10.492 | 139.131 | 138.876 | 139.145 |
| mixed-400000 | baseline | 2 | 160000 | 12.163 | 139.235 | 139.229 | 139.242 |
| mixed-400000 | baseline | 3 | 160000 | 12.956 | 140.020 | 139.764 | 140.031 |
| mixed-400000 | baseline | 4 | 160000 | 14.038 | 139.773 | 139.766 | 139.777 |
| mixed-400000 | baseline | 5 | 160000 | 15.751 | 140.024 | 139.768 | 140.035 |
| mixed-400000 | baseline | 6 | 160000 | 15.116 | 139.777 | 139.769 | 139.781 |
| mixed-400000 | baseline | 7 | 160000 | 16.285 | 140.027 | 139.768 | 140.035 |
| mixed-400000 | baseline | 8 | 160000 | 16.165 | 139.777 | 139.769 | 139.781 |
| mixed-400000 | baseline | 9 | 160000 | 16.422 | 140.027 | 139.768 | 140.035 |
| mixed-400000 | baseline | 10 | 160000 | 16.242 | 139.777 | 139.769 | 139.781 |
| mixed-400000 | baseline | 11 | 160000 | 16.337 | 140.027 | 139.768 | 140.035 |
| mixed-400000 | baseline | 12 | 160000 | 16.774 | 139.777 | 139.769 | 139.781 |
| mixed-400000 | current | 0 | 400000 | 27.755 | 126.935 | 126.684 | 126.945 |
| mixed-400000 | current | 1 | 160000 | 9.602 | 138.877 | 138.872 | 138.887 |
| mixed-400000 | current | 2 | 160000 | 11.116 | 139.062 | 139.056 | 139.070 |
| mixed-400000 | current | 3 | 160000 | 12.003 | 139.589 | 139.584 | 139.598 |
| mixed-400000 | current | 4 | 160000 | 12.637 | 139.593 | 139.585 | 139.598 |
| mixed-400000 | current | 5 | 160000 | 14.098 | 139.593 | 139.588 | 139.602 |
| mixed-400000 | current | 6 | 160000 | 13.992 | 139.596 | 139.589 | 139.602 |
| mixed-400000 | current | 7 | 160000 | 14.292 | 139.596 | 139.588 | 139.602 |
| mixed-400000 | current | 8 | 160000 | 14.183 | 139.596 | 139.589 | 139.602 |
| mixed-400000 | current | 9 | 160000 | 15.008 | 139.596 | 139.588 | 139.602 |
| mixed-400000 | current | 10 | 160000 | 14.591 | 139.596 | 139.589 | 139.602 |
| mixed-400000 | current | 11 | 160000 | 14.985 | 139.596 | 139.588 | 139.602 |
| mixed-400000 | current | 12 | 160000 | 15.070 | 139.596 | 139.589 | 139.602 |
| mixed-400000 | sqlite | 0 | 400000 | 8.796 | 97.423 | 93.266 | 97.430 |
| mixed-400000 | sqlite | 1 | 160000 | 5.790 | 121.840 | 117.129 | 121.848 |
| mixed-400000 | sqlite | 2 | 160000 | 6.036 | 121.655 | 117.129 | 121.660 |
| mixed-400000 | sqlite | 3 | 160000 | 6.820 | 121.286 | 117.129 | 121.293 |
| mixed-400000 | sqlite | 4 | 160000 | 6.063 | 121.632 | 117.129 | 121.637 |
| mixed-400000 | sqlite | 5 | 160000 | 6.874 | 121.286 | 117.129 | 121.293 |
| mixed-400000 | sqlite | 6 | 160000 | 6.017 | 121.651 | 117.129 | 121.656 |
| mixed-400000 | sqlite | 7 | 160000 | 6.875 | 121.207 | 117.129 | 121.215 |
| mixed-400000 | sqlite | 8 | 160000 | 5.944 | 121.616 | 117.129 | 121.621 |
| mixed-400000 | sqlite | 9 | 160000 | 7.047 | 121.172 | 117.129 | 121.180 |
| mixed-400000 | sqlite | 10 | 160000 | 5.959 | 121.522 | 117.129 | 121.527 |
| mixed-400000 | sqlite | 11 | 160000 | 6.853 | 121.180 | 117.129 | 121.188 |
| mixed-400000 | sqlite | 12 | 160000 | 6.011 | 121.478 | 117.129 | 121.484 |
| vectors-stable | baseline | 0 | 10000 | 2.092 | 87.413 | 81.387 | 87.422 |
| vectors-stable | baseline | 1 | 2000 | 0.517 | 105.814 | 99.726 | 105.816 |
| vectors-stable | baseline | 2 | 2000 | 0.978 | 105.793 | 99.726 | 105.797 |
| vectors-stable | baseline | 3 | 2000 | 0.525 | 105.841 | 99.726 | 105.844 |
| vectors-stable | baseline | 4 | 2000 | 0.923 | 105.793 | 99.726 | 105.797 |
| vectors-stable | current | 0 | 10000 | 2.206 | 87.413 | 81.387 | 87.422 |
| vectors-stable | current | 1 | 2000 | 0.209 | 83.776 | 83.769 | 83.777 |
| vectors-stable | current | 2 | 2000 | 0.314 | 83.776 | 83.769 | 83.777 |
| vectors-stable | current | 3 | 2000 | 0.204 | 83.776 | 83.769 | 83.777 |
| vectors-stable | current | 4 | 2000 | 0.318 | 83.776 | 83.769 | 83.777 |
| vectors-stable | sqlite | 0 | 10000 | 2.874 | 92.546 | 84.027 | 92.551 |
| vectors-stable | sqlite | 1 | 2000 | 0.572 | 90.455 | 84.035 | 90.457 |
| vectors-stable | sqlite | 2 | 2000 | 0.582 | 90.455 | 84.035 | 90.457 |
| vectors-stable | sqlite | 3 | 2000 | 0.578 | 90.455 | 84.035 | 90.457 |
| vectors-stable | sqlite | 4 | 2000 | 0.618 | 90.455 | 84.035 | 90.457 |
| vectors-changing | baseline | 0 | 10000 | 2.111 | 87.413 | 81.387 | 87.422 |
| vectors-changing | baseline | 1 | 2000 | 0.514 | 105.814 | 99.726 | 105.816 |
| vectors-changing | baseline | 2 | 2000 | 0.949 | 105.793 | 99.726 | 105.797 |
| vectors-changing | baseline | 3 | 2000 | 0.519 | 105.841 | 99.726 | 105.844 |
| vectors-changing | baseline | 4 | 2000 | 0.963 | 105.793 | 99.726 | 105.797 |
| vectors-changing | current | 0 | 10000 | 2.110 | 87.413 | 81.387 | 87.422 |
| vectors-changing | current | 1 | 2000 | 0.502 | 105.786 | 99.726 | 105.789 |
| vectors-changing | current | 2 | 2000 | 0.903 | 105.655 | 99.726 | 105.660 |
| vectors-changing | current | 3 | 2000 | 0.493 | 105.812 | 99.726 | 105.816 |
| vectors-changing | current | 4 | 2000 | 0.919 | 105.655 | 99.726 | 105.660 |
| vectors-changing | sqlite | 0 | 10000 | 2.842 | 92.546 | 84.027 | 92.551 |
| vectors-changing | sqlite | 1 | 2000 | 1.234 | 94.711 | 84.035 | 94.711 |
| vectors-changing | sqlite | 2 | 2000 | 1.248 | 94.711 | 84.035 | 94.711 |
| vectors-changing | sqlite | 3 | 2000 | 1.231 | 94.711 | 84.035 | 94.711 |
| vectors-changing | sqlite | 4 | 2000 | 1.236 | 94.711 | 84.035 | 94.711 |
| vectors-held | baseline | 0 | 10000 | 2.105 | 87.413 | 81.387 | 87.422 |
| vectors-held | baseline | 1 | 2000 | 0.521 | 105.814 | 99.726 | 105.820 |
| vectors-held | baseline | 2 | 2000 | 0.520 | 124.237 | 118.143 | 124.246 |
| vectors-held | baseline | 3 | 2000 | 0.517 | 124.312 | 118.143 | 124.320 |
| vectors-held | baseline | 4 | 2000 | 0.514 | 124.264 | 118.143 | 124.273 |
| vectors-held | current | 0 | 10000 | 2.171 | 87.413 | 81.387 | 87.422 |
| vectors-held | current | 1 | 2000 | 0.207 | 83.776 | 83.769 | 83.781 |
| vectors-held | current | 2 | 2000 | 0.206 | 86.256 | 86.241 | 86.262 |
| vectors-held | current | 3 | 2000 | 0.208 | 86.256 | 86.241 | 86.262 |
| vectors-held | current | 4 | 2000 | 0.211 | 86.256 | 86.241 | 86.262 |
| vectors-held | sqlite | 0 | 10000 | 2.823 | 92.546 | 84.027 | 92.551 |
| vectors-held | sqlite | 1 | 2000 | 5.248 | 90.448 | 90.448 | 90.449 |
| vectors-held | sqlite | 2 | 2000 | 5.370 | 96.868 | 96.868 | 96.875 |
| vectors-held | sqlite | 3 | 2000 | 5.384 | 103.319 | 103.319 | 103.324 |
| vectors-held | sqlite | 4 | 2000 | 5.393 | 109.739 | 109.739 | 109.746 |

## Process usage

Each process used `prlimit --as=134217728` (128 MiB virtual address space).
RSS excludes filesystem cache and other services.

| Run | Arm | Max RSS MiB | User s | System s |
|---|---|---:|---:|---:|
| mixed-400000 | baseline | 10.734 | 137.729 | 18.965 |
| mixed-400000 | current | 10.672 | 126.670 | 16.456 |
| mixed-400000 | sqlite | 11.531 | 82.099 | 12.493 |
| vectors-stable | baseline | 10.672 | 12.065 | 1.262 |
| vectors-stable | current | 10.672 | 12.204 | 1.069 |
| vectors-stable | sqlite | 12.062 | 11.343 | 1.773 |
| vectors-changing | baseline | 10.719 | 12.142 | 1.033 |
| vectors-changing | current | 10.734 | 11.870 | 1.029 |
| vectors-changing | sqlite | 12.062 | 11.286 | 1.742 |
| vectors-held | baseline | 14.078 | 18.061 | 2.548 |
| vectors-held | current | 14.031 | 18.412 | 1.774 |
| vectors-held | sqlite | 28.047 | 17.442 | 3.416 |
