"""Reading the boxes of ISO BMFF (MP4) files, for the test tools."""
import struct


def boxes(data, start=0, end=None):
    """Yields (type, offset, header size, box size) for each box from start to end."""
    if end is None:
        end = len(data)
    p = start
    while p + 8 <= end:
        size, kind = struct.unpack('>I4s', data[p:p + 8])
        header = 8
        if size == 1:
            size = struct.unpack('>Q', data[p + 8:p + 16])[0]
            header = 16
        elif size == 0:
            size = end - p
        if size < header or p + size > end:
            return
        yield kind, p, header, size
        p += size


def children(payload):
    """Yields (type, payload) for each box in a payload."""
    for kind, p, header, size in boxes(payload):
        yield kind, payload[p + header:p + size]


def child(payload, kind):
    """The payload of the first box of that type, or None."""
    for k, body in children(payload):
        if k == kind:
            return body
    return None


def full_box(payload):
    """(version, flags, the rest) of a full box."""
    return payload[0], int.from_bytes(payload[1:4], 'big'), payload[4:]
