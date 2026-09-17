cargo test --release --locked --offline --no-fail-fast --features compact-cells\,sqlite-balance\,keyspace-append\,slotref-split --lib current_reader::tests -- --test-threads=1 
