use e4_prototype::pagewal::{PageWalStore,recover_to};
use kernel::page::{PageKind,PageRef};
use std::fs;
fn payload(i:u64)->Vec<u8>{let mut v=vec![7;if i==17{8192}else{256}];v[..8].copy_from_slice(&i.to_le_bytes());v}
#[test]
fn source_preserving_repair_contains_damage_and_never_resurrects_deletes(){
    let t=tempfile::tempdir().unwrap();
    for fault in ["none","wal-delete","leaf","overflow","root","meta","meta-single","free","wal-bad"]{
        let p=t.path().join(fault);let mut s=PageWalStore::open(&p,true,64<<10).unwrap();
        for i in 0..1000u64{s.put(&i.to_be_bytes(),&payload(i)).unwrap();}s.commit().unwrap();s.checkpoint().unwrap();
        for i in (0..1000u64).step_by(10){s.delete(&i.to_be_bytes()).unwrap();}s.commit().unwrap();
        // Free an overflow chain, so allocator damage is a real separate case.
        s.put(b"temporary",&vec![5;8192]).unwrap();s.commit().unwrap();s.delete(b"temporary").unwrap();s.commit().unwrap();
        if fault!="wal-delete"&&fault!="wal-bad"{s.checkpoint().unwrap();}drop(s);
        let data=p.join("data");let mut bytes=fs::read(&data).unwrap();
        if matches!(fault,"leaf"|"overflow"|"free"){
            let kind=match fault{"leaf"=>PageKind::Leaf,"overflow"=>PageKind::Overflow,_=>PageKind::Free};
            let no=bytes.chunks_exact(4096).enumerate().skip(2).find(|(n,b)|PageRef::open(b,*n as u32).is_ok_and(|p|p.kind()==kind)).unwrap().0;
            bytes[no*4096+100]^=1;fs::write(&data,&bytes).unwrap();
        }else if fault=="root" {
            let p=PageRef::open(&bytes[..4096],0).unwrap();let root=u32::from_le_bytes(p.slot(0)[8..12].try_into().unwrap()) as usize;
            bytes[root*4096+100]^=1;fs::write(&data,&bytes).unwrap();
        }else if fault=="meta"||fault=="meta-single"{
            bytes[100]^=1;
            if fault=="meta"{bytes[4096+100]^=1;}
            fs::write(&data,&bytes).unwrap();
        }
        if fault=="wal-bad"{let mut b=fs::read(p.join("wal")).unwrap();b[100]^=1;fs::write(p.join("wal"),b).unwrap();}
        let before=[fs::read(&data).unwrap(),fs::read(p.join("wal")).unwrap()];
        let dest=t.path().join(format!("repair-{fault}"));let r=recover_to(&p,&dest,1<<20);
        assert_eq!(before,[fs::read(&data).unwrap(),fs::read(p.join("wal")).unwrap()]);
        if fault=="wal-bad"{assert!(r.is_err());assert!(!dest.join("COMPLETE.json").exists());continue;}
        let report=r.unwrap();let db=PageWalStore::open(&dest.join("current"),false,64<<10).unwrap();let mut rows=0;
        db.scan(|k,v|{let i=u64::from_be_bytes(k.try_into().unwrap());assert!(i<1000&&i%10!=0);assert_eq!(v,payload(i));rows+=1;true}).unwrap();
        assert_eq!(report["current_rows"],rows);
        match fault {
            "none"|"wal-delete"|"free"|"meta-single"=>assert_eq!(rows,900),
            "overflow"=>{assert_eq!(rows,899);assert_eq!(report["known_affected_keys"],1);},
            "leaf"=>assert!(rows>800&&rows<900),
            "root"|"meta"=>{assert_eq!(rows,0);assert_eq!(report["candidate_rows"],900);},_=>unreachable!(),
        }
        assert!(dest.join("COMPLETE.json").exists());
        assert!(recover_to(&p,&dest,1<<20).is_err());
        assert!(recover_to(&p,&p.join("alias"),1<<20).is_err());
    }
}
