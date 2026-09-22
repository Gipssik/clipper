// Scale + tone map + NV12 pack. See convert.rs for why this is two pixel shaders and not one
// compute dispatch.

cbuffer Params : register(b0)
{
    float2 dstSize;     // destination luma size, in pixels
    float2 srcSize;     // capture size, in pixels
    float  whiteScale;  // scRGB units that the desktop's white is worth (1.0 for an SDR source)
    float  peak;        // brightest the source can go, once white is 1.0
    uint   hdr;         // 1 = linear scRGB source, needs tone mapping
    uint   resampling;  // 1 = the source is larger than the target, so run the real filter
};

Texture2D<float4> src  : register(t0);
SamplerState      samp : register(s0);

struct VSOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

// One oversized triangle covers the target with no vertex buffer and no input layout.
VSOut VSMain(uint id : SV_VertexID)
{
    VSOut o;
    float2 t = float2((id << 1) & 2, id & 2);
    o.uv = t;
    o.pos = float4(t * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// ffmpeg's mobius operator, the one Clipper's export path already settled on. Below the transition
// point it is the identity, so midtones pass through untouched; above it, highlights are bent down
// toward 1.0. Chosen over hable by measurement: across real clips hable consistently under-exposes
// while mobius holds brightness with the same highlight recovery.
float mobius(float x, float p)
{
    const float j = 0.3;
    if (x <= j)
        return x;

    float a = -j * j * (p - 1.0) / (j * j - 2.0 * j + p);
    float b = (j * j - 2.0 * j * p + p) / max(p - 1.0, 1e-6);
    return (b * b + 2.0 * b * j + j * j) / (b - a) * (x + a) / (x + b);
}

float oetf1(float c)
{
    return c < 0.018 ? c * 4.5 : 1.099 * pow(max(c, 1e-6), 0.45) - 0.099;
}

float3 bt709_oetf(float3 c)
{
    return float3(oetf1(c.r), oetf1(c.g), oetf1(c.b));
}

// The raw texture read, and nothing else.
//
// Filtering happens on these values and the tone map runs once per *output* pixel rather than once
// per tap. With a sixty-four tap kernel that is the difference between one `pow` per pixel and
// sixty-four, and on an HDR display the tone map is the expensive half of this shader. The cost is
// that on HDR the filter then works in linear light rather than after the transfer curve, which is
// the more physically defensible of the two anyway; on SDR the source is already sRGB-encoded, so
// it is exactly the gamma-space filtering every other video scaler does.
float3 fetch(float2 uv)
{
    return src.SampleLevel(samp, uv, 0).rgb;
}

float3 toDisplay(float3 c)
{
    if (hdr == 0)
        return c;   // already sRGB-encoded; the colour conversion is all that is left

    // scRGB is linear with bt709 primaries and 1.0 fixed at 80 nits, but the desktop's white on an
    // HDR display is whatever the SDR brightness slider says. Undo that first or everything lands
    // whiteScale times too bright.
    c /= whiteScale;

    // scRGB expresses colours outside bt709 as negatives. bt709 cannot carry them.
    c = max(c, 0.0);

    // Tone map the brightest channel and scale the triplet by the same ratio. A per-channel curve
    // would pull bright saturated colours toward white; this keeps the hue and only loses
    // luminance, which is the trade that looks right on real footage.
    // peak <= 1 means nothing in the source is brighter than white — HDR is off and the capture
    // is just a linear version of ordinary sRGB content. Tone mapping it would only darken
    // midtones, and the curve is numerically awkward at peak == 1 anyway.
    float sig = max(max(c.r, c.g), c.b);
    if (peak > 1.001 && sig > 1e-6)
        c *= mobius(sig, peak) / sig;

    return bt709_oetf(saturate(c));
}

// ── The downscale ────────────────────────────────────────────────────────────────────────────
//
// The first version read one bilinear tap at the destination resolution and let the sampler do the
// scaling. That is exactly a box filter at 2x and under-filters at every other ratio, and a box is
// the softest useful filter there is — most of why a 720p capture off a 1440p panel looked mushy
// beside one taken by hardware that resamples properly.
//
// This is a Catmull-Rom resample: the kernel is stretched to the destination pixel pitch, which is
// what makes it anti-alias, and it is evaluated at every source texel inside its support rather
// than at a handful of guessed positions. That last part is not a detail. An earlier attempt here
// took four bilinear taps a destination pixel apart, which *looks* like a four-tap Catmull-Rom and
// is not: at 2x the two inner taps each straddle a texel boundary, so their weights land flat
// across four source texels instead of peaking over two, and the result measured blurrier than the
// box filter it replaced. Sampling the kernel at the source grid is the only version that is
// actually the filter it claims to be.
//
// MAX_TAPS bounds the cost at 8 per axis, which is the exact support at 2x — the usual case, since
// a 1440p panel at 720p is exactly 2x and 1080p is 1.33x. Beyond 2x the kernel is narrowed to stay
// inside the budget rather than being sampled sparsely across its full width, because a sparsely
// sampled wide kernel aliases, which is worse than a slightly narrow one.
#define MAX_TAPS 8

float crWeight(float x)
{
    x = abs(x);
    if (x < 1.0) return 1.5 * x * x * x - 2.5 * x * x + 1.0;
    if (x < 2.0) return -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0;
    return 0.0;
}

float3 resample(float2 uv, float2 plane)
{
    if (resampling == 0)
        return toDisplay(fetch(uv));

    float2 scale = max(srcSize / plane, 1.0);       // source texels per destination pixel
    float2 c     = uv * srcSize - 0.5;              // this pixel's centre, as a source texel index
    float2 r     = min(2.0 * scale, float(MAX_TAPS) * 0.5);  // support, in source texels
    int2   first = int2(floor(c - r)) + 1;          // first source texel inside the support

    float wx[MAX_TAPS], wy[MAX_TAPS];
    float sx = 0.0, sy = 0.0;
    [unroll] for (int k = 0; k < MAX_TAPS; ++k)
    {
        float dx = (float(first.x + k) - c.x);
        float dy = (float(first.y + k) - c.y);
        wx[k] = abs(dx) <= r.x ? crWeight(dx * 2.0 / r.x) : 0.0;
        wy[k] = abs(dy) <= r.y ? crWeight(dy * 2.0 / r.y) : 0.0;
        sx += wx[k];
        sy += wy[k];
    }

    float3 acc = 0.0;
    [unroll] for (int y = 0; y < MAX_TAPS; ++y)
    {
        if (wy[y] == 0.0)
            continue;
        [unroll] for (int x = 0; x < MAX_TAPS; ++x)
        {
            if (wx[x] == 0.0)
                continue;
            float2 at = (float2(first + int2(x, y)) + 0.5) / srcSize;
            acc += (wx[x] * wy[y]) * fetch(at);
        }
    }

    // Truncating the outer lobes leaves the weights summing to something near but not exactly one,
    // and a kernel that does not sum to one shifts the whole picture's brightness.
    return saturate(toDisplay(acc / max(sx * sy, 1e-6)));
}

// Limited-range BT.709. The output is tagged bt709 downstream to match; without that tag a player
// reads the source's colour metadata and still believes the clip is HDR.
float3 rgbToYuv(float3 c)
{
    float y  = dot(c, float3(0.2126, 0.7152, 0.0722));
    float cb = (c.b - y) / 1.8556;
    float cr = (c.r - y) / 1.5748;
    return float3(16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr) / 255.0;
}

float PSLuma(VSOut i) : SV_Target
{
    return rgbToYuv(resample(i.uv, dstSize)).x;
}

float2 PSChroma(VSOut i) : SV_Target
{
    // Chroma is half resolution in both directions, so one chroma texel stands for a 2x2 luma quad
    // and its filter footprint is twice as wide. Same kernel, half the plane.
    if (resampling != 0)
        return rgbToYuv(resample(i.uv, dstSize * 0.5)).yz;

    // Nothing to resample, but still not a point sample: averaging the four luma-grid positions is
    // what keeps colour edges from aliasing when the two planes disagree about where a pixel is.
    float2 off = 0.5 / dstSize;
    float3 a = fetch(i.uv + float2(-off.x, -off.y));
    float3 b = fetch(i.uv + float2( off.x, -off.y));
    float3 c = fetch(i.uv + float2(-off.x,  off.y));
    float3 d = fetch(i.uv + float2( off.x,  off.y));
    return rgbToYuv(toDisplay((a + b + c + d) * 0.25)).yz;
}
