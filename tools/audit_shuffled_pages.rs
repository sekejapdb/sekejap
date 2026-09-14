//! Read-only physical audit of the fixed 8-byte-key / 200-byte-value fixture.
//! No Store open, WAL replay, or production record decoder. Physical presence
//! is not committed membership; unflushed cache pages may be absent.
use serde_json::{json, Value};
use std::{fs::File, io::Read};
fn u16le(b: &[u8], i: usize) -> usize { u16::from_le_bytes(b[i..i+2].try_into().unwrap()) as usize }
fn u32le(b: &[u8], i: usize) -> u32 { u32::from_le_bytes(b[i..i+4].try_into().unwrap()) }
fn example(out: &mut Vec<Value>, v: Value) { if out.len()<32 { out.push(v); } }
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let n: usize = args[2].parse()?;
    assert!(n<=1_000_000);
    let mut file=File::open(&args[1])?;
    let bytes=file.metadata()?.len(); assert_eq!(bytes%4096,0);
    let pages=(bytes/4096) as usize; assert!(pages<=1_000_000);
    let mut seen=vec![false;n];
    let mut bounds=vec![None;pages];
    let mut parents=Vec::new();
    let mut rows=0;let mut duplicates=0;let mut disorder=0;let mut invalid=0;
    let mut errors=Vec::new();let mut issues=Vec::new();let mut checksum_errors=0;
    let mut b=[0u8;4096];
    // Multiplicative inverse in Z/(2^64), computed independently of key encoder.
    let multiplier=0x9E37_79B9_7F4A_7C15u64;
    let mut inverse=1u64;for _ in 0..6 {inverse=inverse.wrapping_mul(2u64.wrapping_sub(multiplier.wrapping_mul(inverse)));}
    for no in 0..pages {
        file.read_exact(&mut b)?;
        let crc=crc32c::crc32c_append(crc32c::crc32c(&b[..36]),&b[40..]);
        if u32le(&b,0)!=0x53454b32 || u32le(&b,12)!=no as u32 || crc!=u32le(&b,36) {
            checksum_errors+=1;example(&mut errors,json!({"page":no,"magic":u32le(&b,0),"identity":u32le(&b,12),"crc_ok":crc==u32le(&b,36)}));continue;
        }
        let kind=u16le(&b,6); if kind!=2 && kind!=3 {continue;}
        let count=u16le(&b,10);assert!(40+count*4<=4096);
        let mut keys=Vec::new();let mut children=vec![u32le(&b,20)];
        for slot in 0..count {
            let offset=u16le(&b,40+slot*4);let len=u16le(&b,42+slot*4);
            assert!(offset>=40+count*4 && offset+len<=4096 && len>=2);
            let rec=&b[offset..offset+len];let header=u16le(rec,0);
            // This fixture's arbitrary 8-byte keys can also match the integer
            // compact key form when the first byte is 0x87.
            let (key,value_at)=if kind==2 && rec[0]==0xff && rec[1]==0x87 {
                assert!(len>=9);(&rec[1..9],9)
            } else {
                let compact=kind==2 && header&0xf000==0x4000;
                let k=if compact {header&0xfff}else{header};assert_eq!(k,8);
                assert!(len>=2+k);(&rec[2..10],if compact {10}else{12})
            };
            let key=u64::from_be_bytes(key.try_into().unwrap());keys.push(key);
            if kind==2 {
                rows+=1;
                let index=key.wrapping_mul(inverse);
                if index>=n as u64 || rec.len()!=value_at+200 || rec[value_at..].iter().any(|v|*v!=b'x') {
                    invalid+=1;example(&mut issues,json!({"page":no,"slot":slot,"key":key,"index":index,"issue":"unexpected key/value"}));
                } else if seen[index as usize] {duplicates+=1;example(&mut issues,json!({"page":no,"slot":slot,"index":index,"issue":"duplicate physical key"}));}
                else {seen[index as usize]=true;}
            } else {assert_eq!(rec.len(),14);children.push(u32le(rec,10));}
        }
        for pair in keys.windows(2) {if pair[0]>=pair[1] {disorder+=1;example(&mut issues,json!({"page":no,"kind":kind,"left":pair[0],"right":pair[1],"issue":"within-page ordering"}));}}
        bounds[no]=keys.first().copied().zip(keys.last().copied());
        if kind==3 {parents.push((no,keys,children));}
    }
    let mut bad_routes=0;let mut referenced=vec![false;pages];
    for (parent,keys,children) in &parents {
        for (i,&child) in children.iter().enumerate() {
            if child as usize>=pages {example(&mut issues,json!({"parent":parent,"child":child,"issue":"child beyond physical file"}));continue;}
            referenced[child as usize]=true;
            if let Some((min,max))=bounds[child as usize] {
                if (i>0 && min<keys[i-1]) || (i<keys.len() && max>=keys[i]) {
                    bad_routes+=1;example(&mut issues,json!({"parent":parent,"child":child,"min":min,"max":max,"lower":i.checked_sub(1).map(|j|keys[j]),"upper":keys.get(i),"issue":"child outside parent interval"}));
                }
            }
        }
    }
    let roots:Vec<_>=parents.iter().filter(|(no,_,_)|!referenced[*no]).map(|(no,_,_)|*no).collect();
    let missing_examples:Vec<_>=seen.iter().enumerate().filter(|(_,v)|!**v).take(32).map(|(i,_)|json!({"index":i,"key":(i as u64).wrapping_mul(multiplier)})).collect();
    println!("{}",json!({"path":args[1],"physical_pages":pages,"physical_rows":rows,"expected":n,"missing_physical_keys":seen.iter().filter(|v|!**v).count(),"missing_examples":missing_examples,"duplicate_physical_keys":duplicates,"invalid_rows":invalid,"within_page_disorder":disorder,"bad_immediate_routes":bad_routes,"invalid_pages":checksum_errors,"page_errors":errors,"issues":issues,"unreferenced_interiors":roots,"scope":"physical pages only; cache not flushed; no committed-membership claim"}));
    Ok(())
}
