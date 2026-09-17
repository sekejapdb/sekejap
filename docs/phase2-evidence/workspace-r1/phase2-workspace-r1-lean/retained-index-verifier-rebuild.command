cargo test --release --locked --offline --no-fail-fast --features compact-cells\,sqlite-balance\,keyspace-append\,slotref-split --test index_verifier --test index_rebuild -- --test-threads=1 
