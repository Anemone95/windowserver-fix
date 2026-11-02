"""lldb command `shieldfix check|apply`: live-patch SkyLight show_shield_windows
so each shield window id is recorded in CGXSessionPackageData+0x180 and
hide_shield_windows releases it. Build 25F84 (macOS 26.5.2, arm64e) only."""
import lldb
import struct

SKYLIGHT_UUID = "6F1978A2-1B8A-3A44-903D-532EED7106EB"
UNSLID_TEXT = 0x186CD9000

# Expected code 0x186f94b28..0x186f94b58 (unslid), one word per instruction.
CONTEXT_START = 0x186F94B28
CONTEXT = [
    0xB4000156,  # cbz  x22, b50
    0xF940C668,  # ldr  x8, [x19,#0x188]
    0xB4000148,  # cbz  x8, b58
    0xA903FFE8,  # stp  x8, xzr, [sp,#0x38]
    0xA902FFF6,  # stp  x22, xzr, [sp,#0x28]
    0x9100E3E0,  # add  x0, sp, #0x38
    0x9100A3E1,  # add  x1, sp, #0x28
    0xD2800002,  # mov  x2, #0
    0x97FA2BE3,  # bl   PKGSpacesAddWindowsWithConnection
    0x14000003,  # b    b58               -> ldr w22, [x22]   (window id)
    0xF940C268,  # ldr  x8, [x19,#0x180]
    0xB83B691F,  # str  wzr, [x8,x27]    -> str w22, [x8,x27] (id, or 0 when x22==NULL)
    0x9100435A,  # add  x26, x26, #0x10
]
PATCHES = {0x186F94B4C: 0xB94002D6, 0x186F94B54: 0xB83B6916}


def _skylight(target):
    for m in target.module_iter():
        if m.GetFileSpec().GetFilename() == "SkyLight":
            return m
    return None


def _read_words(process, addr, n):
    err = lldb.SBError()
    data = process.ReadMemory(addr, 4 * n, err)
    if not err.Success():
        raise RuntimeError("read 0x%x failed: %s" % (addr, err))
    return list(struct.unpack("<%dI" % n, data))


def shieldfix(debugger, command, result, internal_dict):
    mode = command.strip() or "check"
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    if not process.IsValid():
        result.SetError("no attached process")
        return
    mod = _skylight(target)
    if mod is None:
        result.SetError("SkyLight not loaded in target")
        return
    uuid = mod.GetUUIDString().upper()
    if uuid != SKYLIGHT_UUID:
        result.SetError("SkyLight UUID %s != %s (different build); nothing written" % (uuid, SKYLIGHT_UUID))
        return
    slide = mod.GetObjectFileHeaderAddress().GetLoadAddress(target) - UNSLID_TEXT
    base = CONTEXT_START + slide
    words = _read_words(process, base, len(CONTEXT))

    patched = dict(PATCHES)
    state = []
    for i, (have, want) in enumerate(zip(words, CONTEXT)):
        a = CONTEXT_START + 4 * i
        if have == want:
            state.append("orig" if a in patched else "ok")
        elif a in patched and have == patched[a]:
            state.append("new")
        else:
            result.SetError("unexpected word at 0x%x: %08x (want %08x); nothing written" % (a + slide, have, want))
            return
    kinds = {state[(a - CONTEXT_START) // 4] for a in patched}
    print("pid %d  slide 0x%x  site 0x%x  status: %s" % (
        process.GetProcessID(), slide, 0x186F94B4C + slide,
        "PATCHED" if kinds == {"new"} else "ORIGINAL" if kinds == {"orig"} else "PARTIAL"))

    if mode != "apply" or kinds == {"new"}:
        return
    for a, w in sorted(patched.items()):
        err = lldb.SBError()
        process.WriteMemory(a + slide, struct.pack("<I", w), err)
        if not err.Success():
            result.SetError("write 0x%x failed: %s" % (a + slide, err))
            return
    for a, w in sorted(patched.items()):
        got = _read_words(process, a + slide, 1)[0]
        print("  0x%x = %08x %s" % (a + slide, got, "ok" if got == w else "MISMATCH"))


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -o -f %s.shieldfix shieldfix" % __name__)
