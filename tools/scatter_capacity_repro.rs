//! Extracted pre-fix capacity calculation. Expected to panic: diagnostic RED.
//! Run with rustc; excluded from the normal passing Cargo suite.
fn previous_cuts(all: &[Vec<u8>], count: usize, usable: usize) -> Option<Vec<usize>> {
    let total:usize=all.iter().map(|r|r.len()+4).sum();
    let groups=if total>usable*count{count+1}else{count};
    if total>usable*groups || all.len()<groups{return None;}
    let mut cuts=vec![0];let mut remaining=total;let mut possible=true;
    for group in 0..groups-1{
        let begin=*cuts.last().unwrap();let target=remaining/(groups-group);
        let mut bytes=0;let mut best=None;
        for end in begin+1..=all.len()-(groups-group-1){
            bytes+=all[end-1].len()+4;if bytes>usable{break;}
            if remaining-bytes>usable*(groups-group-1){continue;}
            let delta=bytes.abs_diff(target);
            if best.is_none_or(|(_,_,old)|delta<old){best=Some((end,bytes,delta));}
        }
        if let Some((end,bytes,_))=best{cuts.push(end);remaining-=bytes;}else{possible=false;break;}
    }
    if !possible || remaining>usable{return None;}cuts.push(all.len());Some(cuts)
}
fn main() {
    let cells=vec![vec![0u8;268];43];
    assert_eq!(previous_cuts(&cells,3,4056).map(|v|v.len()-1),Some(4),
        "43 indivisible 272-byte cells fit four pages, but the old planner refuses");
}
