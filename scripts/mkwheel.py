#!/usr/bin/env python3
"""Package a pre-built binary into a PEP 427 wheel."""

import argparse
import base64
import hashlib
import os
import stat
import zipfile


def record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
    return f"sha256={digest},{len(data)}"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--name", required=True, help="Distribution name (underscore form)")
    p.add_argument("--version", required=True)
    p.add_argument("--binary", required=True, help="Path to the pre-built binary")
    p.add_argument("--bin-name", required=True, help="Name for the installed script")
    p.add_argument("--tag", required=True, help="Full wheel tag, e.g. py3-none-musllinux_1_2_x86_64")
    p.add_argument("--summary", default="")
    p.add_argument("--license", default="MIT")
    p.add_argument("--homepage", default="")
    p.add_argument("--repository", default="")
    p.add_argument("--output", required=True, help="Output .whl path")
    args = p.parse_args()

    dist_info = f"{args.name}-{args.version}.dist-info"
    data_scripts = f"{args.name}.data/scripts"

    binary_data = open(args.binary, "rb").read()

    metadata_lines = [
        "Metadata-Version: 2.1",
        f"Name: {args.name.replace('_', '-')}",
        f"Version: {args.version}",
    ]
    if args.summary:
        metadata_lines.append(f"Summary: {args.summary}")
    if args.license:
        metadata_lines.append(f"License: {args.license}")
    if args.homepage:
        metadata_lines.append(f"Home-page: {args.homepage}")
    if args.repository:
        metadata_lines.append(f"Project-URL: Repository, {args.repository}")
    metadata_lines.append("Requires-Python: >=3.8")
    metadata = "\n".join(metadata_lines).encode()

    wheel_info = (
        f"Wheel-Version: 1.0\n"
        f"Generator: mkwheel\n"
        f"Root-Is-Purelib: false\n"
        f"Tag: {args.tag}\n"
    ).encode()

    records = [
        f"{data_scripts}/{args.bin_name},{record_hash(binary_data)}",
        f"{dist_info}/METADATA,{record_hash(metadata)}",
        f"{dist_info}/WHEEL,{record_hash(wheel_info)}",
        f"{dist_info}/RECORD,,",
    ]
    record_data = "\n".join(records).encode()

    os.makedirs(os.path.dirname(args.output) or ".", exist_ok=True)
    with zipfile.ZipFile(args.output, "w", zipfile.ZIP_DEFLATED) as zf:
        info = zipfile.ZipInfo(f"{data_scripts}/{args.bin_name}")
        info.external_attr = (
            stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH
        ) << 16
        zf.writestr(info, binary_data)
        zf.writestr(f"{dist_info}/METADATA", metadata)
        zf.writestr(f"{dist_info}/WHEEL", wheel_info)
        zf.writestr(f"{dist_info}/RECORD", record_data)

    print(f"Created {args.output}")


if __name__ == "__main__":
    main()
