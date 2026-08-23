"""Independent structural validator for the WebM files we emit.

Walks the EBML tree without using any of the muxer's own logic, so it catches
VINT/size mistakes that the Rust unit tests could agree with by construction.

Usage: python tools/ebml_check.py FILE
"""
import sys

MASTER = {
    0x1A45DFA3: "EBML", 0x18538067: "Segment", 0x1549A966: "Info",
    0x1654AE6B: "Tracks", 0xAE: "TrackEntry", 0xE0: "Video", 0xE1: "Audio",
    0x1F43B675: "Cluster", 0x1C53BB6B: "Cues", 0xBB: "CuePoint",
    0xB7: "CueTrackPositions",
}
NAMES = {
    0x4282: "DocType", 0x2AD7B1: "TimestampScale", 0x4D80: "MuxingApp",
    0x5741: "WritingApp", 0xD7: "TrackNumber", 0x83: "TrackType",
    0x86: "CodecID", 0x63A2: "CodecPrivate", 0xB0: "PixelWidth",
    0xBA: "PixelHeight", 0xB5: "SamplingFrequency", 0x9F: "Channels",
    0xE7: "Timestamp", 0xA3: "SimpleBlock", 0x73C5: "TrackUID",
    0x9C: "FlagLacing", 0x4286: "EBMLVersion", 0x42F7: "EBMLReadVersion",
    0x42F2: "EBMLMaxIDLength", 0x42F3: "EBMLMaxSizeLength",
    0x4287: "DocTypeVersion", 0x4285: "DocTypeReadVersion",
}

errors = []
stats = {"clusters": 0, "blocks": 0, "elements": 0}


def read_vint(buf, pos, keep_marker):
    if pos >= len(buf):
        raise ValueError("truncated vint at %d" % pos)
    first = buf[pos]
    if first == 0:
        raise ValueError("invalid vint length at %d" % pos)
    length = 1
    mask = 0x80
    while not (first & mask):
        mask >>= 1
        length += 1
    if pos + length > len(buf):
        raise ValueError("vint overruns buffer at %d" % pos)
    raw = buf[pos:pos + length]
    if keep_marker:
        value = int.from_bytes(raw, "big")
    else:
        value = first & (mask - 1)
        for b in raw[1:]:
            value = (value << 8) | b
        # All data bits set means "unknown size".
        if value == (1 << (7 * length)) - 1:
            value = None
    return value, length


def walk(buf, start, end, depth, path):
    pos = start
    while pos < end:
        try:
            eid, idlen = read_vint(buf, pos, True)
            size, szlen = read_vint(buf, pos + idlen, False)
        except ValueError as e:
            errors.append("%s: %s" % (path, e))
            return end
        body = pos + idlen + szlen
        unknown = size is None
        stop = end if unknown else body + size

        if stop > end:
            errors.append(
                "%s/%s at 0x%x: size %d overruns parent by %d bytes"
                % (path, NAMES.get(eid, hex(eid)), pos, size, stop - end))
            return end

        name = MASTER.get(eid) or NAMES.get(eid) or hex(eid)
        stats["elements"] += 1
        if eid == 0x1F43B675:
            stats["clusters"] += 1
        if eid == 0xA3:
            stats["blocks"] += 1
            if size is not None and size < 4:
                errors.append("%s: SimpleBlock too short (%d)" % (path, size))

        if depth <= 2 and eid in MASTER:
            print("%s%s  size=%s @0x%x" % ("  " * depth, name,
                                           "unknown" if unknown else size, pos))

        if eid in MASTER:
            walk(buf, body, stop, depth + 1, path + "/" + name)
        pos = stop
    return pos


def main():
    data = open(sys.argv[1], "rb").read()
    if data[:4] != b"\x1a\x45\xdf\xa3":
        print("FAIL: not an EBML file")
        return 1
    walk(data, 0, len(data), 0, "")

    doctype = b"\x42\x82"
    i = data.find(doctype)
    if i < 0 or b"webm" not in data[i:i + 16]:
        errors.append("DocType is not 'webm'; browsers will reject the file")

    print("\nparsed %d elements, %d clusters, %d blocks over %d bytes"
          % (stats["elements"], stats["clusters"], stats["blocks"], len(data)))
    if errors:
        print("\nFAIL")
        for e in errors[:20]:
            print("  " + e)
        return 1
    print("OK: structure is internally consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main())
