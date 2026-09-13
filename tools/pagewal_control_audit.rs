//! Read-only oracle audit for the frozen accepted-Store control fixture.
use kernel::{btree::BTree,budget::MemoryBudget,io::open_recovery_source,meta::Meta,pool::BufferPool};
use serde_json::json;
use std::{cell::Cell,path::Path,sync::Arc};
fn main()->Result<(),Box<dyn std::error::Error>>{
    let a:Vec<String>=std::env::args().collect();let source=Path::new(&a[1]);let n:u64=a[2].parse()?;let cycle:u64=a[3].parse()?;
    let pool=BufferPool::new(open_recovery_source(&source.join("data"))?.into(),Arc::new(MemoryBudget::new(8<<20)),2048)?;
    let meta=Meta::read_latest(&pool)?;let last=Cell::new(None);let hits=Cell::new(0);let attempts=Cell::new(0);
    let tree=BTree::open(&pool,1,meta.roots[0],&last,&hits,&attempts);
    let mut count=0;let mut wrong=0;let mut ordering_errors=0;let mut first=Vec::new();let mut previous=None;
    tree.range(&[])?.for_each_ref(|k,v|{
        count+=1;assert_eq!(k.len(),8);let id=u64::from_be_bytes(k.try_into().unwrap());
        if previous.is_some_and(|p|id<=p){ordering_errors+=1;}previous=Some(id);
        let (slot,version,allowed)=if id<n{(id,if id%5==0{cycle}else{0},id%10!=1)}else{
            ((id-n)%(n/10)*10+1,cycle,(id-n)/(n/10)+1==cycle)
        };
        let mut want=vec![b'a'+((slot+version)%26) as u8;256];want[..8].copy_from_slice(&slot.to_le_bytes());want[8..16].copy_from_slice(&version.to_le_bytes());
        if !allowed||v!=want {wrong+=1;if first.len()<32{first.push(json!({"key":id,"expected_slot":slot,"expected_version":version,"actual_version":v.get(8..16).map(|v|u64::from_le_bytes(v.try_into().unwrap())),"allowed_key":allowed}));}}
        count<=2*n
    })?;
    let point=tree.get(&395080u64.to_be_bytes())?;
    println!("{}",json!({"source":source,"source_access":"read-only file handle; no Store opener, reader registration or mutation",
        "generation":meta.generation,"root":meta.roots[0],"rows":count,"expected_rows":n,"cycle":cycle,
        "mismatches":wrong,"ordering_errors":ordering_errors,"scan_limit_reached":count>2*n,"first":first,"point_395080_version":point.map(|v|u64::from_le_bytes(v[8..16].try_into().unwrap()))}));
    Ok(())
}
