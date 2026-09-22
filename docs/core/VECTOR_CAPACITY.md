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

## What the current index does, and where it stops

sekejap ships two vector index families. `exact` reads the true f32 vectors and
answers exactly. `quantized` stores int8 codes, scans them, and reranks the
shortlist against the f32 originals. Both are **linear**: they look at every
entry of the column.

That is the right shape at small and medium scale, and it is why sekejap answers
vector queries faster than PostgreSQL with pgvector at 50,000 rows while
returning every correct neighbour. A scan cannot miss what a graph can.

It stops being the right shape as rows grow, because a linear scan's cost grows
with the row count and nothing makes it stop. Reading 205 GB of int8 codes to
answer one query is not a tuning problem.

**Rule of thumb:** the linear families are comfortable into the hundreds of
thousands of rows. Past a few million, plan for an approximate graph index, and
plan the dimension down first.

## What to measure on your own data

Three numbers decide your configuration, and all three are specific to you:

1. the row count you expect, not the one you have today,
2. the dimension you can truncate to without losing answers you care about,
3. how much disk and memory you are willing to spend to make queries faster.

State those three and the choice usually makes itself.
