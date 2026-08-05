SET memory_limit='5GB';
SET temp_directory='data/tmp';
SET preserve_insertion_order=false;
SET threads=3;
INSTALL httpfs; LOAD httpfs;
COPY (SELECT * FROM read_parquet('https://datasets.clickhouse.com/hits_compatible/hits.parquet') LIMIT 10000000)
TO 'data/datasets/clickbench/hits_10m.parquet' (FORMAT PARQUET, ROW_GROUP_SIZE 100000);
