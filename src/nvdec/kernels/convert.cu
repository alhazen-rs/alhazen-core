// NV12/P016 → BGRA: matrix and range, scaling (bilinear taps averaged over each output pixel's
// footprint when shrinking; at 1:1, chroma is repeated as the CPU path does), and HDR → SDR tone
// mapping for PQ/HLG sources.
//
// Mirrors src/color/tonemap.rs; keep the two in step. After editing, regenerate convert.ptx with
// scripts/build-ptx.sh (a test checks the PTX was built from this file).
#include <stdint.h>

struct Params {
    int src_w, src_h, pitch, chroma_offset, ten_bit;
    int dst_w, dst_h;
    float kr_v, kg_u, kg_v, kb_u;   // Y'CbCr → R'G'B' coefficients of the source matrix
    float y_off, y_scale;           // y' = (Y - y_off) * y_scale, on samples normalized to 0–1
    float c_off, c_scale;           // c  = (C - c_off) * c_scale
    int transfer;                   // 0 SDR, 1 PQ, 2 HLG
    int bt2020;                     // apply the BT.2020 → BT.709 gamut matrix
    float src_peak_pq, sdr_white_pq, peak; // EETF inputs (PQ signal of the peak and of SDR white; peak in nits)
};

__device__ float load(const uint8_t* base, int pitch, int x, int y, int stride_px, int comp, int ten_bit) {
    const uint8_t* row = base + (size_t)y * pitch;
    return ten_bit ? (float)((const uint16_t*)row)[x * stride_px + comp] / 65535.f
                   : (float)row[x * stride_px + comp] / 255.f;
}

// Bilinear sample of component `comp` of a plane with `stride_px` components per pixel.
__device__ float bilinear(const uint8_t* base, int pitch, int w, int h, float x, float y, int stride_px, int comp, int ten_bit) {
    x = fminf(fmaxf(x, 0.f), w - 1.f);
    y = fminf(fmaxf(y, 0.f), h - 1.f);
    int x0 = (int)x, y0 = (int)y, x1 = min(x0 + 1, w - 1), y1 = min(y0 + 1, h - 1);
    float fx = x - x0, fy = y - y0;
    float a = load(base, pitch, x0, y0, stride_px, comp, ten_bit) * (1.f - fx) + load(base, pitch, x1, y0, stride_px, comp, ten_bit) * fx;
    float b = load(base, pitch, x0, y1, stride_px, comp, ten_bit) * (1.f - fx) + load(base, pitch, x1, y1, stride_px, comp, ten_bit) * fx;
    return a * (1.f - fy) + b * fy;
}

// The average of bilinear taps spread over output pixel (x, y)'s footprint in a plane of w×h
// pixels that is `scale` times (sx, sy) the output: up to 4×4 taps when shrinking, one otherwise.
__device__ float area(const uint8_t* base, int pitch, int w, int h, int x, int y, float sx, float sy, int stride_px, int comp, int ten_bit) {
    int nx = min(max((int)ceilf(sx), 1), 4), ny = min(max((int)ceilf(sy), 1), 4);
    float sum = 0.f;
    for (int j = 0; j < ny; j++) {
        for (int i = 0; i < nx; i++) {
            float px = (x + (i + 0.5f) / nx) * sx - 0.5f, py = (y + (j + 0.5f) / ny) * sy - 0.5f;
            sum += bilinear(base, pitch, w, h, px, py, stride_px, comp, ten_bit);
        }
    }
    return sum / (nx * ny);
}

__device__ float pq_eotf(float v) {  // PQ signal → nits
    const float m1 = 0.1593017578125f, m2 = 78.84375f, c1 = 0.8359375f, c2 = 18.8515625f, c3 = 18.6875f;
    float e = powf(fminf(fmaxf(v, 0.f), 1.f), 1.f / m2);
    return 10000.f * powf(fmaxf(e - c1, 0.f) / (c2 - c3 * e), 1.f / m1);
}

__device__ float pq_inv(float nits) {  // nits → PQ signal
    const float m1 = 0.1593017578125f, m2 = 78.84375f, c1 = 0.8359375f, c2 = 18.8515625f, c3 = 18.6875f;
    float y = powf(fminf(fmaxf(nits / 10000.f, 0.f), 1.f), m1);
    return powf((c1 + c2 * y) / (1.f + c3 * y), m2);
}

// ITU-R BT.2390 EETF: the source peak to SDR white, Hermite roll-off above the knee.
__device__ float eetf(float nits, const Params& p) {
    float e1 = fminf(fmaxf(pq_inv(nits) / p.src_peak_pq, 0.f), 1.f);
    float max_lum = p.sdr_white_pq / p.src_peak_pq, ks = 1.5f * max_lum - 0.5f, e2 = e1;
    if (e1 >= ks) {
        float t = (e1 - ks) / (1.f - ks), t2 = t * t, t3 = t2 * t;
        e2 = (2.f * t3 - 3.f * t2 + 1.f) * ks + (t3 - 2.f * t2 + t) * (1.f - ks) + (-2.f * t3 + 3.f * t2) * max_lum;
    }
    return pq_eotf(e2 * p.src_peak_pq);
}

__device__ float hlg_inv(float e) {
    const float a = 0.17883277f, b = 0.28466892f, c = 0.5599107f;
    return e <= 0.5f ? e * e / 3.f : (expf((e - c) / a) + b) / 12.f;
}

__device__ float srgb(float l) {
    l = fminf(fmaxf(l, 0.f), 1.f);
    return l <= 0.0031308f ? 12.92f * l : 1.055f * powf(l, 1.f / 2.4f) - 0.055f;
}

extern "C" __global__ void convert(const uint8_t* src, uint8_t* dst, Params p) {
    int x = blockIdx.x * blockDim.x + threadIdx.x, y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= p.dst_w || y >= p.dst_h) return;
    int cw = (p.src_w + 1) / 2, ch = (p.src_h + 1) / 2;
    const uint8_t* uv = src + p.chroma_offset;
    float Y, U, V;
    if (p.src_w == p.dst_w && p.src_h == p.dst_h) {
        Y = load(src, p.pitch, x, y, 1, 0, p.ten_bit);
        int cx = min(x >> 1, cw - 1), cy = min(y >> 1, ch - 1);
        U = load(uv, p.pitch, cx, cy, 2, 0, p.ten_bit);
        V = load(uv, p.pitch, cx, cy, 2, 1, p.ten_bit);
    } else {
        float sx = (float)p.src_w / p.dst_w, sy = (float)p.src_h / p.dst_h;
        Y = area(src, p.pitch, p.src_w, p.src_h, x, y, sx, sy, 1, 0, p.ten_bit);
        float csx = (float)cw / p.dst_w, csy = (float)ch / p.dst_h;
        U = area(uv, p.pitch, cw, ch, x, y, csx, csy, 2, 0, p.ten_bit);
        V = area(uv, p.pitch, cw, ch, x, y, csx, csy, 2, 1, p.ten_bit);
    }
    float yy = (Y - p.y_off) * p.y_scale, u = (U - p.c_off) * p.c_scale, v = (V - p.c_off) * p.c_scale;
    float r = fminf(fmaxf(yy + p.kr_v * v, 0.f), 1.f);
    float g = fminf(fmaxf(yy - p.kg_u * u - p.kg_v * v, 0.f), 1.f);
    float b = fminf(fmaxf(yy + p.kb_u * u, 0.f), 1.f);
    if (p.transfer != 0) {
        float nr, ng, nb;
        if (p.transfer == 1) {
            nr = pq_eotf(r); ng = pq_eotf(g); nb = pq_eotf(b);
        } else {
            float lr = hlg_inv(r), lg = hlg_inv(g), lb = hlg_inv(b);
            float ys = 0.2627f * lr + 0.6780f * lg + 0.0593f * lb;
            float gamma = 1.2f * powf(1.111f, log2f(p.peak / 1000.f));
            float gain = ys > 0.f ? p.peak * powf(ys, gamma - 1.f) : 0.f;
            nr = lr * gain; ng = lg * gain; nb = lb * gain;
        }
        float m = fmaxf(nr, fmaxf(ng, nb));
        float s = m > 0.f ? eetf(m, p) / m / 203.f : 0.f;
        nr *= s; ng *= s; nb *= s;
        if (p.bt2020) {
            float r7 = 1.660491f * nr - 0.587641f * ng - 0.072850f * nb;
            float g7 = -0.124550f * nr + 1.132900f * ng - 0.008349f * nb;
            float b7 = -0.018151f * nr - 0.100579f * ng + 1.118730f * nb;
            nr = r7; ng = g7; nb = b7;
        }
        r = srgb(nr); g = srgb(ng); b = srgb(nb);
    }
    uint8_t* o = dst + ((size_t)y * p.dst_w + x) * 4;
    o[0] = (uint8_t)(b * 255.f + 0.5f);
    o[1] = (uint8_t)(g * 255.f + 0.5f);
    o[2] = (uint8_t)(r * 255.f + 0.5f);
    o[3] = 255;
}
