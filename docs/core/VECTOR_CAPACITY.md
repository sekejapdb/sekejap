# Planning vector storage

What a vector column costs before any index exists, and the one decision that
changes that cost more than any index choice does.

## The dimension decides the size

A `VECTOR(n)` column stores `n` numbers per row. That is the whole cost model,
and it dominates everything else. At 50 million rows:

| dimensions | as f32 | as int8 |
|---|---|---|
| 4096 | 819 GB | 205 GB |
| 2048 | 410 GB | 102 GB |
| 1024 | 205 GB | 51 GB |
| 512 | 102 GB | 26 GB |
| 256 | 51 GB | 13 GB |

Halving the dimension halves the store. No index, no compression setting and no
tuning knob comes close to that leverage.

## Choose the dimension before you choose the index

Many current embedding models are trained so their vectors can be **truncated**
and still work. The technique is Matryoshka representation learning: the model
packs the most important information into the leading dimensions, so taking the
first 1024 of a 4096-dimension vector keeps most of the quality at a quarter of
the size. Qwen3 embedding models support this, and so do several others.

sekejap does nothing special to enable it. `VECTOR(n)` accepts any `n`, so
truncating is a decision you make when you write the row:

```python
full = embed(text)            # 4096 numbers from the model
db.execute(
    "INSERT INTO docs (_key, embedding) VALUES ($1, $2)",
    [key, full[:1024]],       # store 1024
)
```

Measure the quality loss on your own data before committing. Truncation is
cheap to try and expensive to discover late, because changing the dimension
later means rewriting every row.

## Scale, by row count

Where the vector data alone lands, as int8:

| rows | at 1024 dims | at 4096 dims |
|---|---|---|
| 1 million | 1.0 GB | 4.1 GB |
| 10 million | 10.2 GB | 41 GB |
| 50 million | 51 GB | 205 GB |

## What each index costs, per row

sekejap ships three vector index families. `exact` reads the true f32 vectors
and answers exactly. `quantized` stores int8 codes, scans them, and reranks the
shortlist against the f32 originals. `vamana` is the Vamana/DiskANN graph over
those same int8 codes: it reads the nodes its search list reaches instead of
every entry. The first two are **linear** — they look at every entry of the
column — and the third is not.

The index is on top of the column. These are the index's own bytes per row,
measured over the `0x73`, `0x79` and `0x7D`/`0x7F` keyspaces of a real database
(`bench/src/bin/vamana_bench.rs`, 128 lanes, key bytes included). A `vamana`
row is TWO records under one feature bit — its HEAD in `0x7D` and its
neighbour list in `0x7F` — so it pays a second key:

| family | per row, formula | at 128 lanes | at 1024 lanes | at 4096 lanes |
|---|---|---|---|---|
| `exact` | key + a 6-byte locator | ~17 B | ~17 B | ~17 B |
| `quantized` | key + 6 + 8 + `n` | 148 B | 1.0 kB | 4.1 kB |
| `vamana` | 2 keys + 6 + 8 + `n` + 4 + 12·degree | 666–883 B | 1.6–1.9 kB | 4.7–4.9 kB |

`vamana`'s spread is its degree: a neighbour list is pruned back to R = 48 and
may grow to 96 before the next prune, so the occupancy sits between 48 and 96
edges of 12 bytes each. The measured range above is what four real builds
occupied. Note what the table says at high dimension: **the adjacency is a
rounding error once `n` is large.** At 4096 lanes a vamana index costs about
18% more than the quantized one it replaces, and it answers without reading
the other 99.98% of the corpus.

At 50 million rows and 1024 lanes: `exact` 0.9 GB, `quantized` 51 GB, `vamana`
80–95 GB, on top of the 205 GB of f32 column.

## Where the linear families stop

A linear scan is the right shape at small and medium scale, and it is why
sekejap answers vector queries faster than PostgreSQL with pgvector at 50,000
rows while returning every correct neighbour. A scan cannot miss what a graph
can.

It stops being the right shape as rows grow, because a linear scan's cost grows
with the row count and nothing makes it stop. Reading 205 GB of int8 codes to
answer one query is not a tuning problem.

**Rule of thumb, measured.** Over 10,000 clustered 128-lane rows the graph
reads 1,832 records where the scan reads 10,000, and returns recall@10 of 0.99
at a search list of 200. At 2,000 rows the scan is three times faster in wall
clock, because 2,000 sequential int8 entries are cheaper than 1,700 scattered
node records. The crossover is where a record read stops being a page-cache hit
— which is exactly where these numbers stop being the interesting ones, and
where the 205 GB does.

**The other axis is the data.** Recall is a property of the corpus as much as
of the algorithm. The same graph over 10,000 rows of INDEPENDENT uniform lanes
in 128 dimensions returns 0.94 at a search list of 200, and 0.62 at 50,000
rows: in 128 independent dimensions every pair of points is nearly equidistant
and there is barely a neighbourhood to navigate. Real embeddings are not that;
they are clustered, with an intrinsic dimension far below their lane count, and
the clustered numbers above are the ones that resemble them. Measure recall on
your OWN vectors before you choose the family, exactly as you measure
truncation.

## What to measure on your own data

Three numbers decide your configuration, and all three are specific to you:

1. the row count you expect, not the one you have today,
2. the dimension you can truncate to without losing answers you care about,
3. how much disk and memory you are willing to spend to make queries faster,
4. for `vamana`, the recall you need and therefore the search list you will
   set (`SET LOCAL diskann.query_search_list_size = n`) — and the BUILD cost
   that index carries, which is one bounded graph search per row: 55 seconds
   for 10,000 clustered 128-lane rows and 16 minutes for 50,000 uniform ones
   on the reference Mac, against one and five seconds for the same corpora's
   quantized index.

State those three and the choice usually makes itself.
