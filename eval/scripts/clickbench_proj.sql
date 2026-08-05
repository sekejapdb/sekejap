COPY (
  SELECT WatchID, RegionID, ResolutionWidth::INTEGER AS ResolutionWidth, OS::INTEGER AS OS,
         SearchPhrase, CounterID, UserID, URL
  FROM read_parquet('data/datasets/clickbench/hits_10m.parquet')
  LIMIT 1000000
) TO 'data/prepared/relational/clickbench_proj.ndjson' (FORMAT JSON);
