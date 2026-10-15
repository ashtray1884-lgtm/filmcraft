// FilmCraft's bridge to RED's R3D SDK: a small C interface over the SDK's C++ classes, so the
// Rust side (crates/platform/src/r3d) needs no C++. Compiled only when the R3D SDK is available
// at build time (FILMCRAFT_R3D_SDK, see build.rs); none of RED's files are part of FilmCraft.
//
// Every entry point catches C++ exceptions: nothing unwinds into Rust.
//
// Copyright the FilmCraft contributors. MIT OR Apache-2.0 (this file only; the R3D SDK it is
// built against is RED's, under the R3D SDK license agreement).

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <new>
#include <string>

#include "R3DSDK.h"

using namespace R3DSDK;

extern "C" {

struct FcR3dInfo {
    uint32_t width;
    uint32_t height;
    uint64_t frames;
    float fps;
    uint32_t fps_num;
    uint32_t fps_den;
    uint32_t audio_channels;
    uint32_t audio_rate;
    uint64_t audio_samples;
    int32_t default_version;
    int32_t minimum_version;
    char camera[64];
    char timecode[32];
};

// Image processing, as plain numbers (see ImageProcessingSettings in R3DSDKDefinitions.h).
struct FcR3dSettings {
    int32_t version;
    float kelvin;
    float tint;
    float exposure_adjust;
    float exposure_compensation;
    uint32_t iso;
    int32_t gamma;
    int32_t color_space;
    int32_t pipeline;
    int32_t tone_map;
    int32_t roll_off;
    uint32_t hdr_peak_nits;
    float cdl_saturation;
    float cdl[9];  // slope R G B, offset R G B, power R G B
    int32_t cdl_enabled;
    int32_t lens_correction;
    int32_t chroma_nr;
    int32_t maskline;
    int32_t flashing_pixel;
    // legacy (ColorVersion2) look
    float saturation;
    float contrast;
    float brightness;
    float drx;
    float shadow;
    float flut;
};

struct FcR3dLimits {
    float kelvin_min, kelvin_max;
    float tint_min, tint_max;
    float exposure_adjust_min, exposure_adjust_max;
    float exposure_min, exposure_max;
    uint32_t hdr_nits_min, hdr_nits_max;
    float cdl_slope_min, cdl_slope_max, cdl_offset_min, cdl_offset_max, cdl_power_min, cdl_power_max;
    float saturation_min, saturation_max, contrast_min, contrast_max, brightness_min, brightness_max;
    float drx_min, drx_max, shadow_min, shadow_max, flut_min, flut_max;
};

struct FcClip {
    Clip* clip;
    std::mutex audio;
};

int fc_r3d_init(const char* lib_dir, unsigned flags) {
    try {
        return static_cast<int>(InitializeSdk(lib_dir, flags));
    } catch (...) {
        return -1;
    }
}

void fc_r3d_finalize() {
    try {
        FinalizeSdk();
    } catch (...) {
    }
}

const char* fc_r3d_version() {
    try {
        const char* v = GetSdkVersion();
        return v ? v : "";
    } catch (...) {
        return "";
    }
}

static void copy_str(char* dst, size_t n, const std::string& s) {
    if (n == 0) return;
    size_t k = s.size() < n - 1 ? s.size() : n - 1;
    std::memcpy(dst, s.data(), k);
    dst[k] = 0;
}

void* fc_r3d_open(const char* path, int* status) {
    try {
        Clip* c = new (std::nothrow) Clip(path);
        if (!c) {
            if (status) *status = LSOutOfMemory;
            return nullptr;
        }
        LoadStatus s = c->Status();
        if (status) *status = static_cast<int>(s);
        if (s != LSClipLoaded) {
            delete c;
            return nullptr;
        }
        FcClip* fc = new (std::nothrow) FcClip();
        if (!fc) {
            delete c;
            if (status) *status = LSOutOfMemory;
            return nullptr;
        }
        fc->clip = c;
        return fc;
    } catch (...) {
        if (status) *status = LSUnknownError;
        return nullptr;
    }
}

void fc_r3d_close(void* h) {
    if (!h) return;
    try {
        FcClip* fc = static_cast<FcClip*>(h);
        delete fc->clip;
        delete fc;
    } catch (...) {
    }
}

int fc_r3d_info(void* h, FcR3dInfo* out) {
    if (!h || !out) return 0;
    try {
        Clip* c = static_cast<FcClip*>(h)->clip;
        std::memset(out, 0, sizeof(*out));
        out->width = static_cast<uint32_t>(c->Width());
        out->height = static_cast<uint32_t>(c->Height());
        out->frames = c->VideoFrameCount();
        out->fps = c->VideoAudioFramerate();
        out->fps_num = c->MetadataItemAsInt(RMD_FRAMERATE_NUMERATOR);
        out->fps_den = c->MetadataItemAsInt(RMD_FRAMERATE_DENOMINATOR);
        out->audio_channels = static_cast<uint32_t>(c->AudioChannelCount());
        if (out->audio_channels > 0) {
            out->audio_rate = c->MetadataItemAsInt(RMD_SAMPLERATE);
            out->audio_samples = c->AudioSampleCount();
        }
        out->default_version = static_cast<int32_t>(c->DefaultColorVersion());
        out->minimum_version = static_cast<int32_t>(c->MinimumColorVersion());
        if (c->MetadataExists(RMD_CAMERA_MODEL)) copy_str(out->camera, sizeof(out->camera), c->MetadataItemAsString(RMD_CAMERA_MODEL));
        const char* tc = c->AbsoluteTimecode(0);
        if (!tc) tc = c->Timecode(0);
        if (tc) copy_str(out->timecode, sizeof(out->timecode), std::string(tc));
        return 1;
    } catch (...) {
        return 0;
    }
}

static void to_c(const ImageProcessingSettings& s, FcR3dSettings* o) {
    std::memset(o, 0, sizeof(*o));
    o->version = static_cast<int32_t>(s.Version);
    o->kelvin = s.Kelvin;
    o->tint = s.Tint;
    o->exposure_adjust = s.ExposureAdjust;
    o->exposure_compensation = s.ExposureCompensation;
    o->iso = static_cast<uint32_t>(s.ISO);
    o->gamma = static_cast<int32_t>(s.GammaCurve);
    o->color_space = static_cast<int32_t>(s.ColorSpace);
    o->pipeline = static_cast<int32_t>(s.ImagePipelineMode);
    o->tone_map = static_cast<int32_t>(s.OutputToneMap);
    o->roll_off = static_cast<int32_t>(s.HighlightRollOff);
    o->hdr_peak_nits = s.HdrPeakNits;
    o->cdl_saturation = s.CdlSaturation;
    o->cdl[0] = s.CdlRed.Slope;
    o->cdl[1] = s.CdlGreen.Slope;
    o->cdl[2] = s.CdlBlue.Slope;
    o->cdl[3] = s.CdlRed.Offset;
    o->cdl[4] = s.CdlGreen.Offset;
    o->cdl[5] = s.CdlBlue.Offset;
    o->cdl[6] = s.CdlRed.Power;
    o->cdl[7] = s.CdlGreen.Power;
    o->cdl[8] = s.CdlBlue.Power;
    o->cdl_enabled = s.CdlEnabled ? 1 : 0;
    o->lens_correction = static_cast<int32_t>(s.LensDistortionCorrection);
    o->chroma_nr = s.ChromaNoiseReduction ? 1 : 0;
    o->maskline = s.MasklineAdjust ? 1 : 0;
    o->flashing_pixel = static_cast<int32_t>(s.FlashingPixelAdjustment);
    o->saturation = s.Saturation;
    o->contrast = s.Contrast;
    o->brightness = s.Brightness;
    o->drx = s.DRX;
    o->shadow = s.Shadow;
    o->flut = s.FLUT;
}

static void from_c(const FcR3dSettings* o, ImageProcessingSettings& s) {
    s.Version = static_cast<ColorVersion>(o->version);
    s.Kelvin = o->kelvin;
    s.Tint = o->tint;
    s.ExposureAdjust = o->exposure_adjust;
    s.ExposureCompensation = o->exposure_compensation;
    s.ISO = o->iso;
    s.GammaCurve = static_cast<ImageGammaCurve>(o->gamma);
    s.ColorSpace = static_cast<ImageColorSpace>(o->color_space);
    s.ImagePipelineMode = static_cast<ImagePipeline>(o->pipeline);
    s.OutputToneMap = static_cast<ToneMap>(o->tone_map);
    s.HighlightRollOff = static_cast<RollOff>(o->roll_off);
    s.HdrPeakNits = o->hdr_peak_nits;
    s.CdlSaturation = o->cdl_saturation;
    s.CdlRed.Slope = o->cdl[0];
    s.CdlGreen.Slope = o->cdl[1];
    s.CdlBlue.Slope = o->cdl[2];
    s.CdlRed.Offset = o->cdl[3];
    s.CdlGreen.Offset = o->cdl[4];
    s.CdlBlue.Offset = o->cdl[5];
    s.CdlRed.Power = o->cdl[6];
    s.CdlGreen.Power = o->cdl[7];
    s.CdlBlue.Power = o->cdl[8];
    s.CdlEnabled = o->cdl_enabled != 0;
    s.LensDistortionCorrection = static_cast<LensCorrection>(o->lens_correction);
    s.ChromaNoiseReduction = o->chroma_nr != 0;
    s.MasklineAdjust = o->maskline != 0;
    s.FlashingPixelAdjustment = static_cast<FlashingPixelAdjust>(o->flashing_pixel);
    s.Saturation = o->saturation;
    s.Contrast = o->contrast;
    s.Brightness = o->brightness;
    s.DRX = o->drx;
    s.Shadow = o->shadow;
    s.FLUT = o->flut;
    s.CheckBounds();
}

// The clip's settings: as recorded (or from its RMD sidecar) when `recorded` is 0, as shot in
// camera when 1.
int fc_r3d_settings(void* h, int recorded, FcR3dSettings* out) {
    if (!h || !out) return 0;
    try {
        Clip* c = static_cast<FcClip*>(h)->clip;
        ImageProcessingSettings s;
        if (recorded)
            c->GetClipImageProcessingSettings(s);
        else
            c->GetDefaultImageProcessingSettings(s);
        to_c(s, out);
        return 1;
    } catch (...) {
        return 0;
    }
}

// Choices: kind 0 gamma, 1 color space, 2 pipeline, 3 tone map, 4 roll-off, 5 ISO (label NULL).
// Returns the number of entries; fills entry `index` when in range.
size_t fc_r3d_choice(int kind, size_t index, int32_t* value, const char** label) {
    try {
        size_t n = 0;
        switch (kind) {
            case 0:
                n = ImageProcessingLimits::GammaCurveCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::GammaCurveMap[index]);
                    if (label) *label = ImageProcessingLimits::GammaCurveLabels[index];
                }
                break;
            case 1:
                n = ImageProcessingLimits::ColorSpaceCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::ColorSpaceMap[index]);
                    if (label) *label = ImageProcessingLimits::ColorSpaceLabels[index];
                }
                break;
            case 2:
                n = ImageProcessingLimits::ImagePipelineModeCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::ImagePipelineModeMap[index]);
                    if (label) *label = ImageProcessingLimits::ImagePipelineModeLabels[index];
                }
                break;
            case 3:
                n = ImageProcessingLimits::OutputToneMapCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::OutputToneMapMap[index]);
                    if (label) *label = ImageProcessingLimits::OutputToneMapLabels[index];
                }
                break;
            case 4:
                n = ImageProcessingLimits::HighlightRollOffCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::HighlightRollOffMap[index]);
                    if (label) *label = ImageProcessingLimits::HighlightRollOffLabels[index];
                }
                break;
            case 5:
                n = ImageProcessingLimits::ISOCount;
                if (index < n) {
                    if (value) *value = static_cast<int32_t>(ImageProcessingLimits::ISOList[index]);
                    if (label) *label = nullptr;
                }
                break;
            default:
                break;
        }
        return n;
    } catch (...) {
        return 0;
    }
}

void fc_r3d_limits(FcR3dLimits* o) {
    if (!o) return;
    try {
        typedef ImageProcessingLimits L;
        o->kelvin_min = L::KelvinMin;
        o->kelvin_max = L::KelvinMax;
        o->tint_min = L::TintMin;
        o->tint_max = L::TintMax;
        o->exposure_adjust_min = L::ExposureAdjustMin;
        o->exposure_adjust_max = L::ExposureAdjustMax;
        o->exposure_min = L::ExposureMin;
        o->exposure_max = L::ExposureMax;
        o->hdr_nits_min = L::HdrPeakNitsMin;
        o->hdr_nits_max = L::HdrPeakNitsMax;
        o->cdl_slope_min = L::CdlSlopeMin;
        o->cdl_slope_max = L::CdlSlopeMax;
        o->cdl_offset_min = L::CdlOffsetMin;
        o->cdl_offset_max = L::CdlOffsetMax;
        o->cdl_power_min = L::CdlPowerMin;
        o->cdl_power_max = L::CdlPowerMax;
        o->saturation_min = L::SaturationMin;
        o->saturation_max = L::SaturationMax;
        o->contrast_min = L::ContrastMin;
        o->contrast_max = L::ContrastMax;
        o->brightness_min = L::BrightnessMin;
        o->brightness_max = L::BrightnessMax;
        o->drx_min = L::DRXMin;
        o->drx_max = L::DRXMax;
        o->shadow_min = L::ShadowMin;
        o->shadow_max = L::ShadowMax;
        o->flut_min = L::FLUTMin;
        o->flut_max = L::FLUTMax;
    } catch (...) {
    }
}

static VideoDecodeMode mode_for(int divisor) {
    switch (divisor) {
        case 1: return DECODE_FULL_RES_PREMIUM;
        case 2: return DECODE_HALF_RES_GOOD;
        case 4: return DECODE_QUARTER_RES_GOOD;
        case 8: return DECODE_EIGHT_RES_GOOD;
        default: return DECODE_SIXTEENTH_RES_GOOD;
    }
}

// Decode frame `frame` at 1/`divisor` size as 16-bit interleaved RGB into `out` (16-byte aligned,
// `out_bytes` long; the size needed is ceil(w/d) * ceil(h/d) * 6). `settings` may be NULL (the
// clip's own). Returns the R3D SDK DecodeStatus (0 = OK).
int fc_r3d_decode(void* h, uint64_t frame, int divisor, const FcR3dSettings* settings, void* out, size_t out_bytes) {
    if (!h || !out) return DSOutputBufferInvalid;
    try {
        Clip* c = static_cast<FcClip*>(h)->clip;
        VideoDecodeJob job;
        job.Mode = mode_for(divisor);
        job.PixelType = PixelType_16Bit_RGB_Interleaved;
        job.OutputBuffer = out;
        job.OutputBufferSize = out_bytes;
        ImageProcessingSettings s;
        if (settings) {
            c->GetDefaultImageProcessingSettings(s);
            from_c(settings, s);
            job.ImageProcessing = &s;
        } else {
            job.ImageProcessing = nullptr;
        }
        job.HdrProcessing = nullptr;
        job.OutputFrameMetadata = nullptr;
        return static_cast<int>(c->DecodeVideoFrame(static_cast<size_t>(frame), job));
    } catch (...) {
        return DSUnknownError;
    }
}

// Audio: `n` sample frames from `start` as interleaved f32 into `out` (room for n * channels).
// Returns the frames written, or -1.
long long fc_r3d_audio(void* h, uint64_t start, size_t n, float* out) {
    if (!h || !out) return -1;
    try {
        FcClip* fc = static_cast<FcClip*>(h);
        std::lock_guard<std::mutex> lock(fc->audio);
        Clip* c = fc->clip;
        size_t ch = c->AudioChannelCount();
        if (ch == 0 || n == 0) return 0;
        size_t bytes = n * ch * 4;
        bytes = (bytes + 511) / 512 * 512;
        // 512-byte aligned buffer (the SDK's requirement)
        unsigned char* raw = static_cast<unsigned char*>(std::malloc(bytes + 511));
        if (!raw) return -1;
        unsigned char* buf = raw + ((512 - (reinterpret_cast<uintptr_t>(raw) % 512)) % 512);
        size_t got = n;
        DecodeStatus st = c->DecodeAudio(start, &got, buf, bytes);
        if (st != DSDecodeOK) {
            std::free(raw);
            return -1;
        }
        if (got > n) got = n;
        // 32-bit big-endian integers (24-bit audio MSB aligned)
        for (size_t i = 0; i < got * ch; i++) {
            const unsigned char* p = buf + i * 4;
            int32_t v = static_cast<int32_t>((uint32_t(p[0]) << 24) | (uint32_t(p[1]) << 16) | (uint32_t(p[2]) << 8) | uint32_t(p[3]));
            out[i] = static_cast<float>(v / 2147483648.0);
        }
        std::free(raw);
        return static_cast<long long>(got);
    } catch (...) {
        return -1;
    }
}

}  // extern "C"
