use crate::label_hmm::label_with_hmm;
use crate::label_motifs::label_with_motifs;
use crate::locus::{self, Allele, Locus};
use crate::read::Read;
use crate::struc::RegionLabel;
use itertools::Itertools;
use rust_htslib::{
    bam::{self, record::Aux, Read as BamRead},
    bcf::{self, record::GenotypeAllele::UnphasedMissing, Read as BcfRead, Record},
    faidx,
};
use std::path::PathBuf;
use std::{
    io::{BufRead, BufReader, Read as ioRead},
    str,
};

type RegionLabels = Vec<RegionLabel>;

#[derive(Debug)]
pub struct Span {
    pub index: usize,
    pub start: usize,
    pub end: usize,
}
pub type Spans = Vec<Span>;

pub fn get_genotype(bcf_path: &PathBuf, locus: &Locus) -> Result<Vec<Allele>, String> {
    let mut bcf = bcf::Reader::from_path(bcf_path).unwrap();
    for record in bcf.records() {
        let record = record.unwrap();

        let tr_id = record.info(b"TRID").string().unwrap().unwrap();
        let tr_id = str::from_utf8(tr_id.to_vec()[0]).unwrap();

        if tr_id != locus.id {
            continue;
        }

        let gt = record.genotypes().unwrap().get(0);
        if gt[0] == UnphasedMissing {
            return Err(format!("TRID={} misses genotyping", tr_id));
        }

        let allele_seqs = get_allele_seqs(locus, &record);
        let region_labels_by_allele = get_region_labels(locus, &allele_seqs, &record);
        let flank_labels_by_allele = get_flank_labels(locus, &region_labels_by_allele);
        let base_labels_by_allele = get_base_labels(locus, &allele_seqs, &record);

        let mut genotype = Vec::new();
        for (index, seq) in allele_seqs.into_iter().enumerate() {
            genotype.push(Allele {
                seq,
                region_labels: region_labels_by_allele[index].clone(),
                flank_labels: flank_labels_by_allele[index].clone(),
                base_labels: base_labels_by_allele[index].clone(),
            });
        }

        return Ok(genotype);
    }
    return Err(format!("TRID={} missing", &locus.id));
}

pub fn get_locus(
    genome_path: &PathBuf,
    catalog_reader: BufReader<Box<dyn ioRead>>,
    tr_id: &str,
    flank_len: usize,
) -> Result<Locus, String> {
    let genome = faidx::Reader::from_path(genome_path).unwrap();

    let query = format!("ID={};", tr_id);
    for line in catalog_reader.lines() {
        let line = line.unwrap();
        if line.contains(&query) {
            return locus::decode(flank_len, &genome, &line);
        }
    }
    return Err(format!("Unable to find locus {}", tr_id));
}

pub fn get_reads(bam_path: &PathBuf, locus: &Locus) -> Result<Vec<Read>, String> {
    let mut reads = bam::IndexedReader::from_path(bam_path).unwrap();
    // This assumes that TRGT outputs flanks shorter than 1Kbps in length. We may want
    // to implement a more flexible mechanism for handling flank lengths here and elsewhere.
    let search_radius = 1000;
    let search_start = std::cmp::max(0, locus.region.start as i64 - search_radius as i64) as u32;
    let search_end = locus.region.end + search_radius;
    let extraction_region = (locus.region.contig.as_str(), search_start, search_end);
    reads.fetch(extraction_region).unwrap();

    let mut seqs = Vec::new();
    for read in reads.records() {
        let read = read.unwrap();
        let seq = str::from_utf8(&read.seq().as_bytes()).unwrap().to_string();

        let trid = match read.aux(b"TR") {
            Ok(Aux::String(value)) => value.to_string(),
            Ok(_) | Err(_) => {
                return Err(format!(
                    "Missing or malformed TR tag in read {}. Was this BAM file generated by the latest version of TRGT?",
                    std::str::from_utf8(read.qname()).unwrap()
                ));
            }
        };

        if trid != locus.id {
            continue;
        }

        let meth = match read.aux(b"MC") {
            Ok(Aux::ArrayU8(value)) => {
                if !value.is_empty() {
                    Some(value.iter().collect::<Vec<_>>())
                } else {
                    None
                }
            }
            Ok(_) => {
                return Err(format!(
                    "malformed MC tag in read {:?}.",
                    String::from_utf8(read.qname().to_vec()).unwrap()
                ))
            }
            Err(_) => None,
        };

        let allele = match read.aux(b"AL") {
            Ok(Aux::I32(value)) => value,
            Ok(_) => {
                return Err(format!(
                    "malformed AL tag in read {:?}.",
                    String::from_utf8(read.qname().to_vec()).unwrap()
                ))
            }
            Err(_) => {
                return Err(format!(
                    "malformatted read. Expected AL tag not found: {:?}",
                    String::from_utf8(read.qname().to_vec()).unwrap()
                ))
            }
        };

        let (left_flank, right_flank) = match read.aux(b"FL") {
            Ok(Aux::ArrayU32(value)) => {
                if value.len() != 2 {
                    return Err(format!(
                        "Malformed FL tag in read {:?}. Expected 2 values, found {}",
                        String::from_utf8(read.qname().to_vec()).unwrap(),
                        value.len()
                    ));
                }
                let vals = value.iter().collect::<Vec<_>>();
                (vals[0] as usize, vals[1] as usize)
            }
            Ok(_) => {
                return Err(format!(
                    "malformatted FL tag in read {:?}.",
                    String::from_utf8(read.qname().to_vec()).unwrap()
                ))
            }
            Err(_) => {
                return Err(format!(
                    "malformatted read. Expected FL tag not found: {:?}",
                    String::from_utf8(read.qname().to_vec()).unwrap()
                ))
            }
        };

        seqs.push(Read {
            seq,
            left_flank,
            right_flank,
            allele,
            meth,
        });
    }

    Ok(seqs)
}

fn get_allele_seqs(locus: &Locus, record: &Record) -> Vec<String> {
    let lf = &locus.left_flank;
    let rf = &locus.right_flank;
    let mut alleles = Vec::new();
    let genotype = record.genotypes().unwrap().get(0);
    for allele in genotype.iter() {
        let allele_index = allele.index().unwrap() as usize;
        let allele_seq = str::from_utf8(record.alleles()[allele_index]).unwrap();
        alleles.push(lf.clone() + allele_seq + &rf.clone());
    }
    alleles
}

fn get_region_labels(locus: &Locus, alleles: &[String], record: &Record) -> Vec<RegionLabels> {
    let lf_len = locus.left_flank.len();
    let rf_len = locus.right_flank.len();

    let mut labels_by_hap = Vec::new();
    let ms_field = record.format(b"MS").string().unwrap();
    let ms_field = str::from_utf8(ms_field.to_vec()[0]).unwrap();
    for (allele_index, spans) in ms_field.split(',').enumerate() {
        let allele_len = alleles[allele_index].len();
        if spans == "." {
            let tr_start = lf_len;
            let tr_end = allele_len - rf_len;

            labels_by_hap.push(vec![
                RegionLabel::Flank(0, tr_start),
                RegionLabel::Other(tr_start, tr_end),
                RegionLabel::Flank(tr_end, allele_len),
            ]);
            continue;
        }
        let mut labels = vec![RegionLabel::Flank(0, locus.left_flank.len())];
        let mut last_seg_end = locus.left_flank.len();
        for span in spans.split('_') {
            let (motif_index, start, end) = span
                .trim_end_matches(')')
                .split(&['(', '-'])
                .map(|s| s.parse::<usize>().unwrap())
                .collect_tuple()
                .unwrap();
            let motif = locus.motifs[motif_index].clone();
            let start = start + locus.left_flank.len();
            let end = end + locus.left_flank.len();

            if start != last_seg_end {
                labels.push(RegionLabel::Seq(last_seg_end, start));
            }
            labels.push(RegionLabel::Tr(start, end, motif));
            last_seg_end = end;
        }

        if last_seg_end != allele_len - rf_len {
            let seg_end = allele_len - rf_len;
            labels.push(RegionLabel::Seq(last_seg_end, seg_end));
            last_seg_end = seg_end;
        }

        labels.push(RegionLabel::Flank(
            last_seg_end,
            last_seg_end + locus.right_flank.len(),
        ));
        labels_by_hap.push(labels);
    }

    labels_by_hap
}

fn get_motif_spans(record: &Record) -> Vec<Option<Spans>> {
    let mut spans_by_allele = Vec::new();
    let ms_field = record.format(b"MS").string().unwrap();
    let ms_field = str::from_utf8(ms_field.to_vec()[0]).unwrap();

    for encoding in ms_field.split(',') {
        let spans = match encoding {
            "." => None,
            _ => Some(
                encoding
                    .split('_')
                    .map(|e| {
                        let (index, start, end) = e
                            .replace(')', "")
                            .replace('(', "-")
                            .split('-')
                            .map(|n| n.parse::<usize>().unwrap())
                            .collect_tuple()
                            .unwrap();
                        Span { index, start, end }
                    })
                    .collect_vec(),
            ),
        };
        spans_by_allele.push(spans);
    }

    spans_by_allele
}

fn get_flank_labels(locus: &Locus, all_labels_by_allele: &Vec<RegionLabels>) -> Vec<RegionLabels> {
    let mut flank_labels_by_allele = Vec::new();
    for all_labels in all_labels_by_allele {
        let tr_len = all_labels
            .iter()
            .map(|l| match l {
                RegionLabel::Flank(_, _) => 0,
                RegionLabel::Other(start, end) => end - start,
                RegionLabel::Seq(start, end) => end - start,
                RegionLabel::Tr(start, end, _) => end - start,
            })
            .sum::<usize>();

        let tr_start = locus.left_flank.len();
        let tr_end = tr_start + tr_len;
        let allele_end = tr_end + locus.right_flank.len();

        flank_labels_by_allele.push(vec![
            RegionLabel::Flank(0, tr_start),
            RegionLabel::Other(tr_start, tr_end),
            RegionLabel::Flank(tr_end, allele_end),
        ]);
    }
    flank_labels_by_allele
}

fn get_base_labels(
    locus: &Locus,
    alleles: &Vec<String>,
    record: &Record,
) -> Vec<Vec<locus::BaseLabel>> {
    let spans_by_allele = get_motif_spans(record);

    if locus.struc.contains('<') {
        label_with_hmm(locus, alleles)
    } else {
        label_with_motifs(locus, &spans_by_allele, alleles)
    }
}