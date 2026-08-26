"""Independent structural validator for the MP4 files we emit.

Walks the ISOBMFF box tree without using any of the muxer's own logic, and
cross-checks the sample tables against `mdat`: every `stco` offset must land
inside `mdat`, and `stts`/`stsz`/`stco` must agree on sample count.

Usage: python tools/mp4_check.py FILE
"""
import sys
from collections import namedtuple

Box = namedtuple("Box", "fourcc start body end")

CONTAINER = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"dinf", b"udta"}

errors = []
stats = {"boxes": 0}


def read_boxes(buf, start, end):
    pos = start
    out = []
    while pos < end:
        if pos + 8 > end:
            errors.append("truncated box header at 0x%x" % pos)
            break
        size = int.from_bytes(buf[pos:pos + 4], "big")
        fourcc = buf[pos + 4:pos + 8]
        if size < 8:
            errors.append("box %r at 0x%x has impossible size %d" % (fourcc, pos, size))
            break
        if pos + size > end:
            errors.append("box %r at 0x%x overruns its parent" % (fourcc, pos))
            break
        out.append(Box(fourcc, pos, pos + 8, pos + size))
        stats["boxes"] += 1
        pos += size
    return out


def find(boxes, fourcc):
    return next((b for b in boxes if b.fourcc == fourcc), None)


def u32(buf, off):
    return int.from_bytes(buf[off:off + 4], "big")


def check_stbl(buf, stbl, mdat):
    boxes = read_boxes(buf, stbl.body, stbl.end)
    stsd = find(boxes, b"stsd")
    stts = find(boxes, b"stts")
    stsz = find(boxes, b"stsz")
    stco = find(boxes, b"stco")
    stsc = find(boxes, b"stsc")
    if not all([stsd, stts, stsz, stco, stsc]):
        errors.append("stbl missing one of stsd/stts/stsz/stco/stsc")
        return

    stsd_boxes = read_boxes(buf, stsd.body + 8, stsd.end)  # +8 skips version/flags+entry_count
    avc1 = find(stsd_boxes, b"avc1")
    if avc1 is None:
        errors.append("stsd has no avc1 sample entry")
    else:
        # avc1 body: reserved(6)+data_ref_index(2)+predefined(16)+dims(4)+
        # res(8)+reserved(4)+frame_count(2)+compressorname(32)+depth(2)+
        # predefined(2) = 78 bytes, then nested boxes.
        avc1_boxes = read_boxes(buf, avc1.body + 78, avc1.end)
        if find(avc1_boxes, b"avcC") is None:
            errors.append("avc1 sample entry has no avcC box")

    sample_count = u32(buf, stsz.body + 8)  # version+flags(4), sample_size(4), then count
    stco_count = u32(buf, stco.body + 4)
    if sample_count != stco_count:
        errors.append("stsz sample_count %d != stco entry_count %d" % (sample_count, stco_count))

    stts_entries = u32(buf, stts.body + 4)
    stts_total = 0
    for i in range(stts_entries):
        off = stts.body + 8 + i * 8
        stts_total += u32(buf, off)
    if stts_total != sample_count:
        errors.append("stts covers %d samples, stsz declares %d" % (stts_total, sample_count))

    for i in range(stco_count):
        offset = u32(buf, stco.body + 8 + i * 4)
        if not (mdat.body <= offset < mdat.end):
            errors.append("stco[%d] offset 0x%x falls outside mdat" % (i, offset))


def main():
    data = open(sys.argv[1], "rb").read()
    top = read_boxes(data, 0, len(data))

    ftyp = find(top, b"ftyp")
    mdat = find(top, b"mdat")
    moov = find(top, b"moov")
    if not ftyp:
        errors.append("no ftyp box")
    if not mdat:
        errors.append("no mdat box")
    if not moov:
        errors.append("no moov box")

    if moov and mdat:
        moov_boxes = read_boxes(data, moov.body, moov.end)
        traks = [b for b in moov_boxes if b.fourcc == b"trak"]
        if not traks:
            errors.append("moov has no trak")
        for trak in traks:
            trak_boxes = read_boxes(data, trak.body, trak.end)
            mdia = find(trak_boxes, b"mdia")
            if mdia is None:
                errors.append("trak missing mdia")
                continue
            mdia_boxes = read_boxes(data, mdia.body, mdia.end)
            minf = find(mdia_boxes, b"minf")
            if minf is None:
                errors.append("mdia missing minf")
                continue
            minf_boxes = read_boxes(data, minf.body, minf.end)
            stbl = find(minf_boxes, b"stbl")
            if stbl is None:
                errors.append("minf missing stbl")
                continue
            check_stbl(data, stbl, mdat)

    print("parsed %d boxes over %d bytes" % (stats["boxes"], len(data)))
    if errors:
        print("\nFAIL")
        for e in errors[:20]:
            print("  " + e)
        return 1
    print("OK: structure is internally consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main())
