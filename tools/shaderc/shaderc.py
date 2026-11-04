#!/usr/bin/env python3
"""Compiles GLSL shader pairs for the Mali-400 into Rust: `exynos_4412_rs::gpu::Program`s.

usage: shaderc.py -o OUT.rs NAME.vert...

Each NAME.vert goes with NAME.frag beside it, GLSL ES 1.00 (`attribute`, `varying`,
`gl_FragColor`). OUT.rs gets one `pub const NAME: Program` each, NAME in capitals.

The pair runs through Mesa's lima driver under drm-shim (tools/shaderc/setup.sh builds both,
once), with LIMA_DEBUG=dump; harness.c draws a triangle with it, and this reads the dump: the
binaries, the vertex shader's prefetch, the varyings, the render state, and where each uniform
went, by the sentinel values harness.c set them to. Attributes are numbered in the order the
vertex shader declares them.

Building an application needs none of this: the Rust it writes is kept in the repository.
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile

HOME = os.environ.get("SHADERC_HOME", os.path.expanduser("~/.cache/exynos-shaderc"))
PREFIX = os.path.join(HOME, "prefix")
HARNESS = os.path.join(HOME, "harness")
# What harness.c draws into: the viewport's scale and translation it leaves in the uniforms.
VIEWPORT = [32.0, 32.0, 0.5, 0.0, 32.0, 32.0, 0.5, 0.0]
POSITION_VARYING = 0x8020


def fail(text):
    sys.exit("shaderc: " + text)


def attributes_of(source):
    """The vertex shader's attributes, by name, in the order declared."""
    names = []
    for match in re.finditer(r"\battribute\s+(?:(?:lowp|mediump|highp)\s+)?\w+\s+([\w\s,]+);", source):
        names += [name.strip() for name in match.group(1).split(",")]
    return names


def run(vertex, fragment):
    """Runs the pair through the harness; returns its lines and the dump's text."""
    if not os.path.exists(HARNESS):
        fail("no %s: run tools/shaderc/setup.sh first" % HARNESS)
    with open(vertex) as file:
        names = attributes_of(file.read())
    env = dict(os.environ)
    env.update({
        "LD_PRELOAD": os.path.join(PREFIX, "lib", "liblima_noop_drm_shim.so"),
        "LD_LIBRARY_PATH": os.path.join(PREFIX, "lib"),
        "__EGL_VENDOR_LIBRARY_FILENAMES": os.path.join(PREFIX, "share", "glvnd", "egl_vendor.d", "50_mesa.json"),
        "LIMA_DEBUG": "dump,notiling",
    })
    with tempfile.TemporaryDirectory() as directory:
        result = subprocess.run([HARNESS, os.path.abspath(vertex), os.path.abspath(fragment)] + names,
                                cwd=directory, env=env, capture_output=True, text=True, timeout=60)
        if result.returncode != 0:
            fail("%s: %s%s" % (vertex, result.stderr, result.stdout))
        dump = os.path.join(directory, "lima.dump.0000")
        if not os.path.exists(dump):
            fail("%s: the driver wrote no dump" % vertex)
        with open(dump) as file:
            return result.stdout.splitlines(), file.read()


def block(dump, header):
    """The values in braces after the line that starts with `header`: ints, or floats."""
    match = re.search(r"^" + re.escape(header) + r"[^\n]*\n\{\n(.*?)^\}", dump, re.M | re.S)
    if not match:
        return None
    text = re.sub(r"/\*.*?\*/", "", match.group(1))
    values = []
    for token in text.replace("\n", " ").split(","):
        token = token.strip()
        if not token:
            continue
        if re.fullmatch(r"0x[0-9a-f]{8}", token):
            values.append(int(token, 16))
        else:
            values.append(float.fromhex(token) if "p" in token else float(token))
    return values


def section(dump, name):
    """A disassembly from the dump, its lines."""
    match = re.search(r"={12} %s DISASSEMBLY BEGIN =+ \*/\n(.*?)/\* =+ %s DISASSEMBLY END" % (name, name), dump, re.S)
    return match.group(1).rstrip().splitlines() if match else []


def halves(words):
    """Half floats, two a word, low first, as floats."""
    import struct
    values = []
    for word in words:
        for half in (word & 0xFFFF, word >> 16):
            values.append(struct.unpack("<e", struct.pack("<H", half))[0])
    return values


def find(values, base, floats):
    """Where the run base, base + 1, ... of `floats` starts in `values`, or None."""
    for start in range(len(values) - floats + 1):
        if all(values[start + i] == base + i for i in range(floats)):
            return start
    return None


def compile_pair(vertex):
    stem = os.path.splitext(vertex)[0]
    fragment = stem + ".frag"
    name = os.path.basename(stem)
    if not re.fullmatch(r"[a-z][a-z0-9_]*", name):
        fail("%s: name the files in lower case, letters, digits and _" % vertex)
    if not os.path.exists(fragment):
        fail("%s: no %s beside it" % (vertex, fragment))
    lines, dump = run(vertex, fragment)

    vs = block(dump, "add vs at va")
    fs = block(dump, "add fs at va")
    rsw = block(dump, "add render state at va")
    uniforms_gp = block(dump, "update gp uniform at va")
    varying_info = block(dump, "update varying info at va")
    if not (vs and fs and rsw and uniforms_gp and varying_info):
        fail("%s: the dump lacks a part: shaders, render state, uniforms or varyings" % vertex)
    prefetch = re.search(r"SHADER_INFO: prefetch: (\d+)", dump)
    if not prefetch:
        fail("%s: no SHADER_INFO in the dump" % vertex)

    # The vertex uniforms end where the viewport starts; the constants follow it.
    sentinels = [line.split() for line in lines if line.startswith("uniform ")]
    lowest = 0
    for _, _, floats, base in sentinels:
        at = find(uniforms_gp, int(base), int(floats))
        if at is not None:
            lowest = max(lowest, at + int(floats))
    viewport = next((at for at in range(lowest - lowest % 4, len(uniforms_gp) - 7, 4)
                     if uniforms_gp[at:at + 8] == VIEWPORT), None)
    if viewport is None:
        fail("%s: no viewport in the GP's uniforms" % vertex)
    constants = uniforms_gp[viewport + 8:]
    pp_words = block(dump, "add pp uniform data at va") or []
    pp = halves(pp_words)

    uniforms = []
    for _, uniform, floats, base in sentinels:
        floats, base = int(floats), int(base)
        found = False
        at = find(uniforms_gp[:viewport], base, floats)
        if at is not None:
            uniforms.append((uniform, "Vertex", at, floats))
            found = True
        at = find(pp, base, floats)
        if at is not None:
            uniforms.append((uniform, "Fragment", at, floats))
            found = True
        if not found:
            fail("%s: uniform %s is in neither stage's uniforms" % (vertex, uniform))

    # The varyings: past the position, each at its offset from the first's address, which the
    # render state's word 15 holds.
    base = rsw[15] & ~0xF
    varyings, stride = [], 0
    for address, info in zip(varying_info[0::2], varying_info[1::2]):
        if info == POSITION_VARYING:
            continue
        if info == 0x2021:
            fail("%s: gl_PointSize is not handled" % vertex)
        varyings.append((address - base, info & 0x7FF))
        stride = info >> 11

    # Addresses out of the render state, their low fields kept.
    state = list(rsw)
    for word, keep in ((9, 0x1F), (11, 0xF), (12, 0xF), (15, 0xF)):
        state[word] &= keep

    return {
        "name": name,
        "vertex_path": vertex,
        "fragment_path": fragment,
        "vs": vs,
        "prefetch": int(prefetch.group(1)),
        "fs": fs,
        "attributes": [(line.split()[1], int(line.split()[2])) for line in lines if line.startswith("attribute ")],
        "uniforms": uniforms,
        "vertex_uniform_floats": viewport,
        "constants": constants,
        "fragment_halves": len(pp),
        "varyings": varyings,
        "stride": stride,
        "samplers": [line.split()[1] for line in lines if line.startswith("sampler ")],
        "render_state": state,
        "fs_text": section(dump, "FS"),
    }


def hex32(value):
    return "0x%04x_%04x" % (value >> 16, value & 0xFFFF)


def words(values, per_line, indent):
    lines = []
    for start in range(0, len(values), per_line):
        lines.append(indent + ", ".join(hex32(v) for v in values[start:start + per_line]) + ",")
    return "\n".join(lines)


def f32(value):
    """The shortest literal that reads back as the same f32."""
    import math
    import struct
    if not math.isfinite(value):
        fail("a constant is %r" % value)
    bits = struct.pack("<f", value)
    for digits in range(1, 10):
        text = "%.*g" % (digits, value)
        if struct.pack("<f", float(text)) == bits:
            break
    if "e" not in text and "." not in text:
        text += ".0"
    return text


def rust(program):
    p = program
    out = []
    with open(p["vertex_path"]) as file:
        vertex_source = file.read().rstrip()
    with open(p["fragment_path"]) as file:
        fragment_source = file.read().rstrip()
    out.append("/// `%s`: %s and %s." % (p["name"], os.path.basename(p["vertex_path"]), os.path.basename(p["fragment_path"])))
    out.append("///")
    out.append("/// ```glsl")
    out += ["/// " + line if line else "///" for line in vertex_source.splitlines()]
    out.append("/// // fragment")
    out += ["/// " + line if line else "///" for line in fragment_source.splitlines()]
    out.append("/// ```")
    out.append("pub const %s: Program = Program {" % p["name"].upper())
    out.append('    name: "%s",' % p["name"])
    out.append("    // %d instructions; LIMA_DEBUG=dump disassembles them." % (len(p["vs"]) // 4))
    out.append("    vertex: &[")
    out.append(words(p["vs"], 4, "        "))
    out.append("    ],")
    out.append("    vertex_prefetch: %d," % p["prefetch"])
    out.append("    // %s" % "\n    // ".join(line.replace("\t", " ") for line in p["fs_text"]))
    out.append("    fragment: &[")
    out.append(words(p["fs"], 4, "        "))
    out.append("    ],")
    out.append("    attributes: &[")
    for attribute, components in p["attributes"]:
        out.append('        Attribute { name: "%s", components: %d },' % (attribute, components))
    out.append("    ],")
    if p["uniforms"]:
        out.append("    uniforms: &[")
        for uniform, stage, offset, floats in p["uniforms"]:
            out.append('        Uniform { name: "%s", stage: Stage::%s, offset: %d, floats: %d },' % (uniform, stage, offset, floats))
        out.append("    ],")
    else:
        out.append("    uniforms: &[],")
    out.append("    vertex_uniform_floats: %d," % p["vertex_uniform_floats"])
    out.append("    vertex_constants: &[%s]," % ", ".join(f32(v) for v in p["constants"]))
    out.append("    fragment_uniform_halves: %d," % p["fragment_halves"])
    out.append("    varyings: &[%s]," % ", ".join("Varying { offset: %d, format: %#x }" % v for v in p["varyings"]))
    out.append("    varying_stride: %d," % p["stride"])
    out.append("    samplers: &[%s]," % ", ".join('"%s"' % s for s in p["samplers"]))
    out.append("    render_state: [")
    out.append(words(p["render_state"], 4, "        "))
    out.append("    ],")
    out.append("};")
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description="GLSL pairs to exynos_4412_rs::gpu::Program constants")
    parser.add_argument("-o", "--output", required=True, help="the Rust file to write")
    parser.add_argument("vertex", nargs="+", help="NAME.vert files, each with NAME.frag beside it")
    args = parser.parse_args()
    programs = [compile_pair(path) for path in sorted(args.vertex)]
    # Paths from the application's directory, the output's parent's parent.
    root = os.path.dirname(os.path.dirname(os.path.abspath(args.output)))
    sources = ", ".join(os.path.relpath(os.path.abspath(p["vertex_path"]), root) for p in programs)
    text = [
        "//! Shaders for the Mali-400, generated by tools/shaderc/shaderc.py: do not edit. From",
        "//! %s, each with the `.frag` beside it; change those and run" % sources,
        "//! `make shaders APP=...`. Compiled by Mesa 25.3.3's lima compiler (MIT).",
        "",
    ]
    # Only what the programs use: the others would warn.
    used = ["Attribute", "Program"]
    if any(p["uniforms"] for p in programs):
        used += ["Stage", "Uniform"]
    if any(p["varyings"] for p in programs):
        used.append("Varying")
    text += ["use exynos_4412_rs::gpu::{%s};" % ", ".join(sorted(used)), ""]
    text.append("\n\n".join(rust(p) for p in programs))
    output = "\n".join(text) + "\n"
    with open(args.output, "w") as file:
        file.write(output)
    for p in programs:
        print("shaderc: %s: vertex %d instructions, fragment %d words, %d uniforms, %d varyings, %d samplers"
              % (p["name"], len(p["vs"]) // 4, len(p["fs"]), len(p["uniforms"]), len(p["varyings"]), len(p["samplers"])))


if __name__ == "__main__":
    main()
