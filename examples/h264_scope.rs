//! Report which H.264 coding tools a stream actually uses.
//!
//! The pure-Rust decoder in `codec::h264` deliberately targets only the subset
//! surveillance cameras emit, and refuses the rest at the parameter-set stage.
//! This tool answers the question that decides how large that subset has to
//! be, by reading a captured Annex B dump and reporting the features present.
//!
//! Run with:
//!   cargo run --release --example h264_scope -- probe.h264
//!
//! Capture a dump first with `rtsp_probe --dump`.

use std::collections::BTreeMap;

use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::pps::PicParameterSet;
use h264_reader::nal::slice::{SliceFamily, SliceHeader};
use h264_reader::nal::sps::{ChromaFormat, FrameMbsFlags, SeqParameterSet};
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::push::{AccumulatedNalHandler, NalInterest};
use h264_reader::Context;

#[derive(Default)]
struct Scan {
    ctx: Context,
    sps: Option<SeqParameterSet>,
    pps: Option<PicParameterSet>,
    /// Slice families seen, and how many of each.
    slices: BTreeMap<&'static str, u32>,
    /// Slices whose header failed to parse. Non-zero means either a decoder
    /// assumption is wrong or the capture is damaged; either way it matters.
    slice_errors: u32,
    nals: BTreeMap<String, u32>,
}

impl Scan {
    fn note_nal(&mut self, kind: &str) {
        *self.nals.entry(kind.to_string()).or_default() += 1;
    }

    fn handle(&mut self, nal: RefNal<'_>) -> NalInterest {
        if !nal.is_complete() {
            return NalInterest::Buffer;
        }
        let Ok(header) = nal.header() else {
            self.note_nal("corrupt header");
            return NalInterest::Ignore;
        };

        match header.nal_unit_type() {
            UnitType::SeqParameterSet => {
                self.note_nal("SPS");
                match SeqParameterSet::from_bits(nal.rbsp_bits()) {
                    Ok(sps) => {
                        self.ctx.put_seq_param_set(sps.clone());
                        // Keep the first: cameras resend an identical SPS on
                        // every keyframe, and a mid-stream change would be a
                        // finding in itself rather than something to average.
                        self.sps.get_or_insert(sps);
                    }
                    Err(e) => eprintln!("SPS parse failed: {e:?}"),
                }
            }
            UnitType::PicParameterSet => {
                self.note_nal("PPS");
                match PicParameterSet::from_bits(&self.ctx, nal.rbsp_bits()) {
                    Ok(pps) => {
                        self.ctx.put_pic_param_set(pps.clone());
                        self.pps.get_or_insert(pps);
                    }
                    Err(e) => eprintln!("PPS parse failed: {e:?}"),
                }
            }
            UnitType::SliceLayerWithoutPartitioningIdr => {
                self.note_nal("IDR slice");
                self.note_slice(&nal, header);
            }
            UnitType::SliceLayerWithoutPartitioningNonIdr => {
                self.note_nal("non-IDR slice");
                self.note_slice(&nal, header);
            }
            UnitType::SEI => self.note_nal("SEI"),
            UnitType::AccessUnitDelimiter => self.note_nal("AUD"),
            UnitType::SliceDataPartitionALayer
            | UnitType::SliceDataPartitionBLayer
            | UnitType::SliceDataPartitionCLayer => self.note_nal("data partition (!)"),
            other => self.note_nal(&format!("{other:?}")),
        }
        NalInterest::Ignore
    }

    fn note_slice(&mut self, nal: &RefNal<'_>, header: h264_reader::nal::NalHeader) {
        match SliceHeader::from_bits(&self.ctx, &mut nal.rbsp_bits(), header) {
            Ok((sh, _, _)) => {
                let family = match sh.slice_type.family {
                    SliceFamily::I => "I",
                    SliceFamily::P => "P",
                    SliceFamily::B => "B",
                    SliceFamily::SP => "SP",
                    SliceFamily::SI => "SI",
                };
                *self.slices.entry(family).or_default() += 1;
            }
            Err(_) => self.slice_errors += 1,
        }
    }
}

impl AccumulatedNalHandler for Scan {
    fn nal(&mut self, nal: RefNal<'_>) -> NalInterest {
        self.handle(nal)
    }
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: h264_scope <file.h264>   (Annex B, from rtsp_probe --dump)");
            std::process::exit(2);
        }
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(1);
        }
    };

    let mut reader = AnnexBReader::accumulate(Scan::default());
    reader.push(&data);
    reader.reset();
    let scan = reader.into_nal_handler();

    println!("{path}: {} bytes\n", data.len());

    println!("NAL units:");
    for (kind, count) in &scan.nals {
        println!("  {count:>6}  {kind}");
    }

    println!("\nslice types:");
    for (family, count) in &scan.slices {
        println!("  {count:>6}  {family}");
    }
    if scan.slice_errors > 0 {
        println!("  {:>6}  FAILED TO PARSE", scan.slice_errors);
    }

    let Some(sps) = &scan.sps else {
        println!("\nno SPS found; cannot report decoder scope");
        std::process::exit(1);
    };

    let width = (sps.pic_width_in_mbs_minus1 + 1) * 16;
    let height_map_units = sps.pic_height_in_map_units_minus1 + 1;
    let progressive = matches!(sps.frame_mbs_flags, FrameMbsFlags::Frames);
    let height = height_map_units * 16 * if progressive { 1 } else { 2 };

    println!("\nSPS:");
    println!("  profile_idc          {:?}", sps.profile_idc);
    println!("  level_idc            {}", sps.level_idc);
    println!("  resolution           {width}x{height} (before cropping)");
    println!("  chroma_format        {:?}", sps.chroma_info.chroma_format);
    println!(
        "  bit depth            luma {}, chroma {}",
        sps.chroma_info.bit_depth_luma_minus8 as u16 + 8,
        sps.chroma_info.bit_depth_chroma_minus8 as u16 + 8
    );
    println!("  frame_mbs_flags      {:?}", sps.frame_mbs_flags);
    println!("  pic_order_cnt        {:?}", sps.pic_order_cnt);
    println!("  max_num_ref_frames   {}", sps.max_num_ref_frames);
    println!(
        "  gaps_in_frame_num    {}",
        sps.gaps_in_frame_num_value_allowed_flag
    );
    println!(
        "  seq scaling matrix   {}",
        has(&sps.chroma_info.scaling_matrix)
    );
    println!("  frame cropping       {}", has(&sps.frame_cropping));

    if let Some(pps) = &scan.pps {
        println!("\nPPS:");
        println!("  entropy coding       {}", entropy(pps));
        println!(
            "  transform_8x8        {}",
            pps.extension
                .as_ref()
                .map(|e| e.transform_8x8_mode_flag)
                .unwrap_or(false)
        );
        println!(
            "  weighted_pred        {} / bipred idc {}",
            pps.weighted_pred_flag, pps.weighted_bipred_idc
        );
        println!(
            "  num_ref_idx_default  l0 {}, l1 {}",
            pps.num_ref_idx_l0_default_active_minus1 + 1,
            pps.num_ref_idx_l1_default_active_minus1 + 1
        );
        println!(
            "  deblocking control   {}",
            pps.deblocking_filter_control_present_flag
        );
        println!("  constrained_intra    {}", pps.constrained_intra_pred_flag);
    }

    // The verdict is the point of the tool: which decoder modules this stream
    // forces us to write, and which we can legitimately refuse.
    println!("\ndecoder scope:");
    let mut blockers = Vec::new();
    if !progressive {
        blockers.push("interlaced (MBAFF/PAFF) — out of declared scope");
    }
    if sps.chroma_info.chroma_format != ChromaFormat::YUV420 {
        blockers.push("chroma format is not 4:2:0 — out of declared scope");
    }
    if sps.chroma_info.bit_depth_luma_minus8 != 0 || sps.chroma_info.bit_depth_chroma_minus8 != 0 {
        blockers.push("bit depth above 8 — out of declared scope");
    }
    if scan.nals.contains_key("data partition (!)") {
        blockers.push("data partitioning — out of declared scope");
    }

    if blockers.is_empty() {
        println!("  within the camera subset the decoder targets");
    } else {
        for b in &blockers {
            println!("  BLOCKER: {b}");
        }
    }

    let b_slices = scan.slices.get("B").copied().unwrap_or(0);
    println!(
        "  B-frames             {}",
        if b_slices > 0 {
            "REQUIRED — direct-mode MV derivation must be implemented"
        } else {
            "absent — direct-mode MV derivation can be skipped"
        }
    );
    if let Some(pps) = &scan.pps {
        println!(
            "  entropy decoder      {}",
            if pps.entropy_coding_mode_flag {
                "CABAC required (the expensive one)"
            } else {
                "CAVLC only — CABAC can wait"
            }
        );
        let t8 = pps
            .extension
            .as_ref()
            .map(|e| e.transform_8x8_mode_flag)
            .unwrap_or(false);
        println!(
            "  8x8 transform        {}",
            if t8 {
                "required"
            } else {
                "not used — 4x4 only"
            }
        );
        println!(
            "  weighted prediction  {}",
            if pps.weighted_pred_flag || pps.weighted_bipred_idc != 0 {
                "required"
            } else {
                "not used"
            }
        );
    }
    println!(
        "  scaling lists        {}",
        if sps.chroma_info.scaling_matrix.is_some() {
            "custom — dequant must honour them"
        } else {
            "flat — FLAT_WEIGHT_SCALE_4X4 suffices"
        }
    );
}

fn has<T>(o: &Option<T>) -> &'static str {
    if o.is_some() {
        "present"
    } else {
        "absent"
    }
}

fn entropy(pps: &PicParameterSet) -> &'static str {
    if pps.entropy_coding_mode_flag {
        "CABAC"
    } else {
        "CAVLC"
    }
}
