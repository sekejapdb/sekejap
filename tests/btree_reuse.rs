//! Deterministic small-cache churn: independent row oracle across CoW epochs.
use kernel::{btree::BTree,budget::MemoryBudget,io::{open_file,IoMode,Barrier},pool::BufferPool,meta::{Meta,FORMAT_VERSION}};
use std::{cell::Cell,sync::Arc};
#[test]
fn mixed_reuse_keeps_scan_and_point_answers_equal(){
    let n:u64=std::env::var("E4_REUSE_ROWS").unwrap_or_else(|_|"4000".into()).parse().unwrap();
    let sizes=std::env::var("E4_REUSE_FRAMES").map(|v|vec![v.parse().unwrap()]).unwrap_or_else(|_|vec![8,16,64]);
    let batch:u64=std::env::var("E4_REUSE_BATCH").unwrap_or_else(|_|"97".into()).parse().unwrap();
    for frames in sizes {
        let d=tempfile::tempdir().unwrap();let (f,_)=open_file(&d.path().join("data"),IoMode::Buffered).unwrap();
        let pool=BufferPool::new(f.into(),Arc::new(MemoryBudget::new(frames*4096)),frames).unwrap();
        drop(pool.allocate().unwrap());drop(pool.allocate().unwrap());
        let (last,hits,attempts)=(Cell::new(None),Cell::new(0),Cell::new(0));
        let mut t=BTree::create(&pool,1,&last,&hits,&attempts).unwrap();
        pool.flush_all(Barrier::None).unwrap();pool.set_frozen_boundary();
        let mut expected=std::collections::BTreeMap::new();let mut gen=1;let mut ops=0;
        for round in 0..20u64 {
            for slot in 0..n {
                let mut change=Vec::new();
                if round==0||slot%5==0 {change.push((slot,Some(round)));}
                else if slot%10==1 {change.push((if round==1{slot}else{n+(round-2)*(n/10)+slot/10},None));change.push((n+(round-1)*(n/10)+slot/10,Some(round)));}
                for (id,value) in change {
                    let key=id.to_be_bytes();
                    if let Some(version)=value{let mut v=vec![version as u8;256];v[..8].copy_from_slice(&slot.to_le_bytes());t.insert(&key,&v).unwrap();expected.insert(key,v);}
                    else{assert!(t.delete(&key).unwrap());expected.remove(&key);}
                    ops+=1;
                    if ops%batch==0 {pool.flush_all(Barrier::None).unwrap();Meta{format_version:FORMAT_VERSION,roots:[t.root(),0,0,0,0,0,0,0],next_lsn:ops,generation:gen}.write_slot(&pool).unwrap();pool.flush_all(Barrier::None).unwrap();pool.set_frozen_boundary();gen+=1;pool.set_stamp_gen(gen);pool.set_reuse_limit(gen.saturating_sub(2));}
                }
            }
            pool.flush_all(Barrier::None).unwrap();Meta{format_version:FORMAT_VERSION,roots:[t.root(),0,0,0,0,0,0,0],next_lsn:ops,generation:gen}.write_slot(&pool).unwrap();pool.flush_all(Barrier::None).unwrap();pool.set_frozen_boundary();gen+=1;pool.set_stamp_gen(gen);pool.set_reuse_limit(gen.saturating_sub(2));
            let mut actual=Vec::new();t.range(&[]).unwrap().for_each_ref(|k,v|{actual.push((k.to_vec(),v.to_vec()));true}).unwrap();
            assert_eq!(actual.len(),expected.len(),"frames={frames} round={round}");
            for ((k,v),(ek,ev)) in actual.iter().zip(expected.iter()) {assert_eq!((k.as_slice(),v),(ek.as_slice(),ev),"frames={frames} round={round}");}
        }
    }
}
