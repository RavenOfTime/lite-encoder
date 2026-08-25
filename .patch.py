p = "src/codec/h264/recon.rs"
s = open(p, encoding="utf-8").read()

s = s.replace(
    """/// Scan order to raster order for a 4x4 block.
///
/// `ac_only` shifts every level up by one position, for the categories whose
/// DC coefficient was coded in a separate block.
fn dezigzag_4x4(levels: &[i32; 16], ac_only: bool) -> [i32; 16] {
    let mut block = [0i32; 16];
    let start = usize::from(ac_only);
    for (scan, &level) in levels.iter().enumerate() {
        if scan + start >= 16 {
            break;
        }
        block[ZIGZAG_4X4[scan + start]] = level;
    }
    block
}""",
    """/// Scan order to raster order for a 4x4 block.
///
/// `levels` is indexed by scan position throughout, including for the
/// categories whose DC was coded in a separate block: those leave index 0
/// empty rather than packing the AC coefficients down onto it, so there is no
/// shift to apply here. Applying one anyway moves every AC coefficient a
/// position further along the scan, which turns a horizontal frequency into a
/// vertical one — invisible on symmetric content, and badly wrong on a
/// vertical edge.
fn dezigzag_4x4(levels: &[i32; 16]) -> [i32; 16] {
    let mut block = [0i32; 16];
    for (scan, &level) in levels.iter().enumerate() {
        block[ZIGZAG_4X4[scan]] = level;
    }
    block
}""",
    1,
)

s = s.replace("dezigzag_4x4(&residual.luma[blk as usize], false)", "dezigzag_4x4(&residual.luma[blk as usize])")
s = s.replace("dezigzag_4x4(&residual.luma[blk as usize], true)", "dezigzag_4x4(&residual.luma[blk as usize])")
s = s.replace("dezigzag_4x4(&residual.chroma[comp][blk], true)", "dezigzag_4x4(&residual.chroma[comp][blk])")

s = s.replace(
    """    /// De-zigzagging must place every level, and the AC form must leave""",
    """    /// De-zigzagging must place every level, and a block whose DC was coded
    /// separately must leave""",
    1,
)
open(p, "w", encoding="utf-8", newline="").write(s)
print("ok")
