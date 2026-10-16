#!/usr/bin/env bash
# Regenerates src/nvdec/kernels/convert.ptx from convert.cu with CUDA 11.8: PTX that drivers
# from 520 on can load, on Maxwell and newer GPUs. Needs podman or docker (the image is pulled
# once, about 3 GB).
set -euo pipefail
cd "$(dirname "$0")/../src/nvdec/kernels"
engine=$(command -v podman || command -v docker)
hash=$(python3 -c '
h = 0xcbf29ce484222325
for b in open("convert.cu", "rb").read():
    h = ((h ^ b) * 0x100000001b3) & 0xffffffffffffffff
print(f"{h:016x}")')
"$engine" run --rm -v "$PWD:/k:Z" -w /k docker.io/nvidia/cuda:11.8.0-devel-ubuntu22.04 \
    nvcc -ptx -arch=compute_52 -O3 --use_fast_math -o convert.ptx.tmp convert.cu
{ echo "// alhazen-source-fnv1a64: $hash"; cat convert.ptx.tmp; } > convert.ptx
rm -f convert.ptx.tmp
echo "convert.ptx regenerated ($hash)"
