//! The OpenFX C API, as far as FilmCraft hosts it: the structs plug-ins and hosts exchange and the
//! names of actions, properties and types. Transcribed from the OpenFX 1.4/1.5 headers
//! (`openfx/include`, BSD-3-Clause, Copyright OpenFX and contributors to the OpenFX project); only
//! names, values and layouts are taken, no code.

#![allow(non_upper_case_globals, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub type OfxStatus = c_int;
pub type Raw = *mut c_void;

pub const OK: OfxStatus = 0;
pub const FAILED: OfxStatus = 1;
pub const ERR_FATAL: OfxStatus = 2;
pub const ERR_UNKNOWN: OfxStatus = 3;
pub const ERR_MISSING_HOST_FEATURE: OfxStatus = 4;
pub const ERR_UNSUPPORTED: OfxStatus = 5;
pub const ERR_EXISTS: OfxStatus = 6;
pub const ERR_FORMAT: OfxStatus = 7;
pub const ERR_MEMORY: OfxStatus = 8;
pub const ERR_BAD_HANDLE: OfxStatus = 9;
pub const ERR_BAD_INDEX: OfxStatus = 10;
pub const ERR_VALUE: OfxStatus = 11;
pub const REPLY_YES: OfxStatus = 12;
pub const REPLY_NO: OfxStatus = 13;
pub const REPLY_DEFAULT: OfxStatus = 14;

/// Whether an action's status means it ran (OK) or was not implemented (the default applies).
pub fn ran(st: OfxStatus) -> bool {
    st == OK
}
pub fn not_failed(st: OfxStatus) -> bool {
    st == OK || st == REPLY_DEFAULT
}

#[repr(C)]
pub struct OfxHost {
    pub host: Raw,
    pub fetch_suite: unsafe extern "C" fn(host: Raw, name: *const c_char, version: c_int) -> *const c_void,
}

pub type MainEntry = unsafe extern "C" fn(action: *const c_char, handle: *const c_void, in_args: Raw, out_args: Raw) -> OfxStatus;

#[repr(C)]
pub struct OfxPlugin {
    pub plugin_api: *const c_char,
    pub api_version: c_int,
    pub plugin_identifier: *const c_char,
    pub version_major: c_uint,
    pub version_minor: c_uint,
    pub set_host: Option<unsafe extern "C" fn(host: *mut OfxHost)>,
    pub main_entry: Option<MainEntry>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OfxRectD {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OfxRangeD {
    pub min: f64,
    pub max: f64,
}

#[repr(C)]
pub struct PropertySuiteV1 {
    pub set_pointer: unsafe extern "C" fn(Raw, *const c_char, c_int, Raw) -> OfxStatus,
    pub set_string: unsafe extern "C" fn(Raw, *const c_char, c_int, *const c_char) -> OfxStatus,
    pub set_double: unsafe extern "C" fn(Raw, *const c_char, c_int, f64) -> OfxStatus,
    pub set_int: unsafe extern "C" fn(Raw, *const c_char, c_int, c_int) -> OfxStatus,
    pub set_pointer_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *const Raw) -> OfxStatus,
    pub set_string_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *const *const c_char) -> OfxStatus,
    pub set_double_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *const f64) -> OfxStatus,
    pub set_int_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *const c_int) -> OfxStatus,
    pub get_pointer: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut Raw) -> OfxStatus,
    pub get_string: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut *const c_char) -> OfxStatus,
    pub get_double: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut f64) -> OfxStatus,
    pub get_int: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut c_int) -> OfxStatus,
    pub get_pointer_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut Raw) -> OfxStatus,
    pub get_string_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut *const c_char) -> OfxStatus,
    pub get_double_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut f64) -> OfxStatus,
    pub get_int_n: unsafe extern "C" fn(Raw, *const c_char, c_int, *mut c_int) -> OfxStatus,
    pub reset: unsafe extern "C" fn(Raw, *const c_char) -> OfxStatus,
    pub get_dimension: unsafe extern "C" fn(Raw, *const c_char, *mut c_int) -> OfxStatus,
}

#[repr(C)]
pub struct ImageEffectSuiteV1 {
    pub get_property_set: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub get_param_set: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub clip_define: unsafe extern "C" fn(Raw, *const c_char, *mut Raw) -> OfxStatus,
    pub clip_get_handle: unsafe extern "C" fn(Raw, *const c_char, *mut Raw, *mut Raw) -> OfxStatus,
    pub clip_get_property_set: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub clip_get_image: unsafe extern "C" fn(Raw, f64, *const OfxRectD, *mut Raw) -> OfxStatus,
    pub clip_release_image: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub clip_get_region_of_definition: unsafe extern "C" fn(Raw, f64, *mut OfxRectD) -> OfxStatus,
    pub abort: unsafe extern "C" fn(Raw) -> c_int,
    pub image_memory_alloc: unsafe extern "C" fn(Raw, usize, *mut Raw) -> OfxStatus,
    pub image_memory_free: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub image_memory_lock: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub image_memory_unlock: unsafe extern "C" fn(Raw) -> OfxStatus,
}

/// The parameter suite. The value functions are C varargs (`...`); FilmCraft implements them as
/// fixed-arity functions taking the most arguments any parameter type passes (see `params.rs`).
#[repr(C)]
pub struct ParameterSuiteV1 {
    pub param_define: unsafe extern "C" fn(Raw, *const c_char, *const c_char, *mut Raw) -> OfxStatus,
    pub param_get_handle: unsafe extern "C" fn(Raw, *const c_char, *mut Raw, *mut Raw) -> OfxStatus,
    pub param_set_get_property_set: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub param_get_property_set: unsafe extern "C" fn(Raw, *mut Raw) -> OfxStatus,
    pub param_get_value: *const c_void,
    pub param_get_value_at_time: *const c_void,
    pub param_get_derivative: *const c_void,
    pub param_get_integral: *const c_void,
    pub param_set_value: *const c_void,
    pub param_set_value_at_time: *const c_void,
    pub param_get_num_keys: unsafe extern "C" fn(Raw, *mut c_uint) -> OfxStatus,
    pub param_get_key_time: unsafe extern "C" fn(Raw, c_uint, *mut f64) -> OfxStatus,
    pub param_get_key_index: unsafe extern "C" fn(Raw, f64, c_int, *mut c_int) -> OfxStatus,
    pub param_delete_key: unsafe extern "C" fn(Raw, f64) -> OfxStatus,
    pub param_delete_all_keys: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub param_copy: unsafe extern "C" fn(Raw, Raw, f64, *const OfxRangeD) -> OfxStatus,
    pub param_edit_begin: unsafe extern "C" fn(Raw, *const c_char) -> OfxStatus,
    pub param_edit_end: unsafe extern "C" fn(Raw) -> OfxStatus,
}

#[repr(C)]
pub struct MemorySuiteV1 {
    pub alloc: unsafe extern "C" fn(Raw, usize, *mut Raw) -> OfxStatus,
    pub free: unsafe extern "C" fn(Raw) -> OfxStatus,
}

pub type ThreadFunction = unsafe extern "C" fn(c_uint, c_uint, Raw);

#[repr(C)]
pub struct MultiThreadSuiteV1 {
    pub multi_thread: unsafe extern "C" fn(Option<ThreadFunction>, c_uint, Raw) -> OfxStatus,
    pub num_cpus: unsafe extern "C" fn(*mut c_uint) -> OfxStatus,
    pub index: unsafe extern "C" fn(*mut c_uint) -> OfxStatus,
    pub is_spawned: unsafe extern "C" fn() -> c_int,
    pub mutex_create: unsafe extern "C" fn(*mut Raw, c_int) -> OfxStatus,
    pub mutex_destroy: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub mutex_lock: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub mutex_unlock: unsafe extern "C" fn(Raw) -> OfxStatus,
    pub mutex_try_lock: unsafe extern "C" fn(Raw) -> OfxStatus,
}

/// `message(handle, type, id, format, ...)`: the printf arguments after `format` are ignored
/// (a callee may ignore trailing arguments in both x64 conventions).
#[repr(C)]
pub struct MessageSuiteV2 {
    pub message: unsafe extern "C" fn(Raw, *const c_char, *const c_char, *const c_char) -> OfxStatus,
    pub set_persistent: unsafe extern "C" fn(Raw, *const c_char, *const c_char, *const c_char) -> OfxStatus,
    pub clear_persistent: unsafe extern "C" fn(Raw) -> OfxStatus,
}

#[repr(C)]
pub struct ProgressSuiteV1 {
    pub start: unsafe extern "C" fn(Raw, *const c_char) -> OfxStatus,
    pub update: unsafe extern "C" fn(Raw, f64) -> OfxStatus,
    pub end: unsafe extern "C" fn(Raw) -> OfxStatus,
}

#[repr(C)]
pub struct ProgressSuiteV2 {
    pub start: unsafe extern "C" fn(Raw, *const c_char, *const c_char) -> OfxStatus,
    pub update: unsafe extern "C" fn(Raw, f64) -> OfxStatus,
    pub end: unsafe extern "C" fn(Raw) -> OfxStatus,
}

#[repr(C)]
pub struct TimeLineSuiteV1 {
    pub get_time: unsafe extern "C" fn(Raw, *mut f64) -> OfxStatus,
    pub goto_time: unsafe extern "C" fn(Raw, f64) -> OfxStatus,
    pub get_bounds: unsafe extern "C" fn(Raw, *mut f64, *mut f64) -> OfxStatus,
}

// ---- names ----------------------------------------------------------------------------------

pub const IMAGE_EFFECT_API: &str = "OfxImageEffectPluginAPI";

pub const ActionLoad: &str = "OfxActionLoad";
pub const ActionDescribe: &str = "OfxActionDescribe";
pub const ActionUnload: &str = "OfxActionUnload";
pub const ActionCreateInstance: &str = "OfxActionCreateInstance";
pub const ActionDestroyInstance: &str = "OfxActionDestroyInstance";
pub const ActionInstanceChanged: &str = "OfxActionInstanceChanged";
pub const ActionBeginInstanceChanged: &str = "OfxActionBeginInstanceChanged";
pub const ActionEndInstanceChanged: &str = "OfxActionEndInstanceChanged";
pub const ActionDescribeInContext: &str = "OfxImageEffectActionDescribeInContext";
pub const ActionGetClipPreferences: &str = "OfxImageEffectActionGetClipPreferences";
pub const ActionIsIdentity: &str = "OfxImageEffectActionIsIdentity";
pub const ActionRender: &str = "OfxImageEffectActionRender";
pub const ActionBeginSequenceRender: &str = "OfxImageEffectActionBeginSequenceRender";
pub const ActionEndSequenceRender: &str = "OfxImageEffectActionEndSequenceRender";

pub const PropAPIVersion: &str = "OfxPropAPIVersion";
pub const PropTime: &str = "OfxPropTime";
pub const PropIsInteractive: &str = "OfxPropIsInteractive";
pub const PluginPropFilePath: &str = "OfxPluginPropFilePath";
pub const PropInstanceData: &str = "OfxPropInstanceData";
pub const PropType: &str = "OfxPropType";
pub const PropName: &str = "OfxPropName";
pub const PropVersion: &str = "OfxPropVersion";
pub const PropVersionLabel: &str = "OfxPropVersionLabel";
pub const PropPluginDescription: &str = "OfxPropPluginDescription";
pub const PropLabel: &str = "OfxPropLabel";
pub const PropShortLabel: &str = "OfxPropShortLabel";
pub const PropLongLabel: &str = "OfxPropLongLabel";
pub const PropChangeReason: &str = "OfxPropChangeReason";
pub const PropHostOSHandle: &str = "OfxPropHostOSHandle";
pub const ChangeUserEdited: &str = "OfxChangeUserEdited";

pub const BitDepthNone: &str = "OfxBitDepthNone";
pub const BitDepthByte: &str = "OfxBitDepthByte";
pub const BitDepthShort: &str = "OfxBitDepthShort";
pub const BitDepthHalf: &str = "OfxBitDepthHalf";
pub const BitDepthFloat: &str = "OfxBitDepthFloat";
pub const ComponentRGBA: &str = "OfxImageComponentRGBA";
pub const ComponentRGB: &str = "OfxImageComponentRGB";
pub const ComponentAlpha: &str = "OfxImageComponentAlpha";

pub const ContextGenerator: &str = "OfxImageEffectContextGenerator";
pub const ContextFilter: &str = "OfxImageEffectContextFilter";
pub const ContextTransition: &str = "OfxImageEffectContextTransition";
pub const ContextGeneral: &str = "OfxImageEffectContextGeneral";

pub const TypeImageEffectHost: &str = "OfxTypeImageEffectHost";
pub const TypeImageEffect: &str = "OfxTypeImageEffect";
pub const TypeImageEffectInstance: &str = "OfxTypeImageEffectInstance";
pub const TypeClip: &str = "OfxTypeClip";
pub const TypeImage: &str = "OfxTypeImage";
pub const TypeParameter: &str = "OfxTypeParameter";
pub const TypeParameterInstance: &str = "OfxTypeParameterInstance";

pub const IEPropSupportedContexts: &str = "OfxImageEffectPropSupportedContexts";
pub const IEPropPluginHandle: &str = "OfxImageEffectPropPluginHandle";
pub const IEHostPropIsBackground: &str = "OfxImageEffectHostPropIsBackground";
pub const IEPluginPropSingleInstance: &str = "OfxImageEffectPluginPropSingleInstance";
pub const IEPluginRenderThreadSafety: &str = "OfxImageEffectPluginRenderThreadSafety";
pub const RenderUnsafe: &str = "OfxImageEffectRenderUnsafe";
pub const RenderInstanceSafe: &str = "OfxImageEffectRenderInstanceSafe";
pub const RenderFullySafe: &str = "OfxImageEffectRenderFullySafe";
pub const IEPluginPropHostFrameThreading: &str = "OfxImageEffectPluginPropHostFrameThreading";
pub const IEPropSupportsMultipleClipDepths: &str = "OfxImageEffectPropMultipleClipDepths";
pub const IEPropSupportsMultipleClipPARs: &str = "OfxImageEffectPropSupportsMultipleClipPARs";
pub const IEPropSetableFrameRate: &str = "OfxImageEffectPropSetableFrameRate";
pub const IEPropSetableFielding: &str = "OfxImageEffectPropSetableFielding";
pub const IEInstancePropSequentialRender: &str = "OfxImageEffectInstancePropSequentialRender";
pub const IEPropSequentialRenderStatus: &str = "OfxImageEffectPropSequentialRenderStatus";
pub const IEPropInteractiveRenderStatus: &str = "OfxImageEffectPropInteractiveRenderStatus";
pub const IEPluginPropGrouping: &str = "OfxImageEffectPluginPropGrouping";
pub const IEPropSupportsOverlays: &str = "OfxImageEffectPropSupportsOverlays";
pub const IEPropSupportsMultiResolution: &str = "OfxImageEffectPropSupportsMultiResolution";
pub const IEPropSupportsTiles: &str = "OfxImageEffectPropSupportsTiles";
pub const IEPropTemporalClipAccess: &str = "OfxImageEffectPropTemporalClipAccess";
pub const IEPropContext: &str = "OfxImageEffectPropContext";
pub const IEPropPixelDepth: &str = "OfxImageEffectPropPixelDepth";
pub const IEPropComponents: &str = "OfxImageEffectPropComponents";
pub const ImagePropUniqueIdentifier: &str = "OfxImagePropUniqueIdentifier";
pub const ClipPropContinuousSamples: &str = "OfxImageClipPropContinuousSamples";
pub const ClipPropUnmappedPixelDepth: &str = "OfxImageClipPropUnmappedPixelDepth";
pub const ClipPropUnmappedComponents: &str = "OfxImageClipPropUnmappedComponents";
pub const IEPropPreMultiplication: &str = "OfxImageEffectPropPreMultiplication";
pub const ImagePreMultiplied: &str = "OfxImageAlphaPremultiplied";
pub const IEPropSupportedPixelDepths: &str = "OfxImageEffectPropSupportedPixelDepths";
pub const IEPropSupportedComponents: &str = "OfxImageEffectPropSupportedComponents";
pub const ClipPropOptional: &str = "OfxImageClipPropOptional";
pub const ClipPropIsMask: &str = "OfxImageClipPropIsMask";
pub const ImagePropPixelAspectRatio: &str = "OfxImagePropPixelAspectRatio";
pub const IEPropFrameRate: &str = "OfxImageEffectPropFrameRate";
pub const IEPropUnmappedFrameRate: &str = "OfxImageEffectPropUnmappedFrameRate";
pub const IEPropFrameStep: &str = "OfxImageEffectPropFrameStep";
pub const IEPropFrameRange: &str = "OfxImageEffectPropFrameRange";
pub const IEPropUnmappedFrameRange: &str = "OfxImageEffectPropUnmappedFrameRange";
pub const ClipPropConnected: &str = "OfxImageClipPropConnected";
pub const IEFrameVarying: &str = "OfxImageEffectFrameVarying";
pub const IEPropRenderScale: &str = "OfxImageEffectPropRenderScale";
pub const IEPropRenderQualityDraft: &str = "OfxImageEffectPropRenderQualityDraft";
pub const IEPropProjectExtent: &str = "OfxImageEffectPropProjectExtent";
pub const IEPropProjectSize: &str = "OfxImageEffectPropProjectSize";
pub const IEPropProjectOffset: &str = "OfxImageEffectPropProjectOffset";
pub const IEPropProjectPixelAspectRatio: &str = "OfxImageEffectPropPixelAspectRatio";
pub const IEInstancePropEffectDuration: &str = "OfxImageEffectInstancePropEffectDuration";
pub const ClipPropFieldOrder: &str = "OfxImageClipPropFieldOrder";
pub const ImagePropData: &str = "OfxImagePropData";
pub const ImagePropBounds: &str = "OfxImagePropBounds";
pub const ImagePropRegionOfDefinition: &str = "OfxImagePropRegionOfDefinition";
pub const ImagePropRowBytes: &str = "OfxImagePropRowBytes";
pub const ImagePropField: &str = "OfxImagePropField";
pub const IEPropFieldToRender: &str = "OfxImageEffectPropFieldToRender";
pub const IEPropRenderWindow: &str = "OfxImageEffectPropRenderWindow";
pub const IEPropOpenGLRenderSupported: &str = "OfxImageEffectPropOpenGLRenderSupported";
pub const IEHostPropNativeOrigin: &str = "OfxImageEffectHostPropNativeOrigin";
pub const HostNativeOriginBottomLeft: &str = "kOfxImageEffectHostPropNativeOriginBottomLeft";
pub const FieldNone: &str = "OfxFieldNone";
pub const OutputClip: &str = "Output";
pub const SourceClip: &str = "Source";
pub const SourceFromClip: &str = "SourceFrom";
pub const SourceToClip: &str = "SourceTo";
pub const TransitionParam: &str = "Transition";

pub const ParamTypeInteger: &str = "OfxParamTypeInteger";
pub const ParamTypeDouble: &str = "OfxParamTypeDouble";
pub const ParamTypeBoolean: &str = "OfxParamTypeBoolean";
pub const ParamTypeChoice: &str = "OfxParamTypeChoice";
pub const ParamTypeStrChoice: &str = "OfxParamTypeStrChoice";
pub const ParamTypeRGBA: &str = "OfxParamTypeRGBA";
pub const ParamTypeRGB: &str = "OfxParamTypeRGB";
pub const ParamTypeDouble2D: &str = "OfxParamTypeDouble2D";
pub const ParamTypeInteger2D: &str = "OfxParamTypeInteger2D";
pub const ParamTypeDouble3D: &str = "OfxParamTypeDouble3D";
pub const ParamTypeInteger3D: &str = "OfxParamTypeInteger3D";
pub const ParamTypeString: &str = "OfxParamTypeString";
pub const ParamTypeCustom: &str = "OfxParamTypeCustom";
pub const ParamTypeBytes: &str = "OfxParamTypeBytes";
pub const ParamTypeGroup: &str = "OfxParamTypeGroup";
pub const ParamTypePage: &str = "OfxParamTypePage";
pub const ParamTypePushButton: &str = "OfxParamTypePushButton";
pub const ParamTypeParametric: &str = "OfxParamTypeParametric";

pub const ParamHostPropSupportsCustomAnimation: &str = "OfxParamHostPropSupportsCustomAnimation";
pub const ParamHostPropSupportsStringAnimation: &str = "OfxParamHostPropSupportsStringAnimation";
pub const ParamHostPropSupportsBooleanAnimation: &str = "OfxParamHostPropSupportsBooleanAnimation";
pub const ParamHostPropSupportsChoiceAnimation: &str = "OfxParamHostPropSupportsChoiceAnimation";
pub const ParamHostPropSupportsCustomInteract: &str = "OfxParamHostPropSupportsCustomInteract";
pub const ParamHostPropMaxParameters: &str = "OfxParamHostPropMaxParameters";
pub const ParamHostPropMaxPages: &str = "OfxParamHostPropMaxPages";
pub const ParamHostPropPageRowColumnCount: &str = "OfxParamHostPropPageRowColumnCount";
pub const ParamHostPropSupportsStrChoice: &str = "OfxParamHostPropSupportsStrChoice";
pub const ParamHostPropSupportsParametricAnimation: &str = "OfxParamHostPropSupportsParametricAnimation";
pub const ParamPropType: &str = "OfxParamPropType";
pub const ParamPropAnimates: &str = "OfxParamPropAnimates";
pub const ParamPropCanUndo: &str = "OfxParamPropCanUndo";
pub const ParamPropIsAnimating: &str = "OfxParamPropIsAnimating";
pub const ParamPropPluginMayWrite: &str = "OfxParamPropPluginMayWrite";
pub const ParamPropPersistant: &str = "OfxParamPropPersistant";
pub const ParamPropEvaluateOnChange: &str = "OfxParamPropEvaluateOnChange";
pub const ParamPropSecret: &str = "OfxParamPropSecret";
pub const ParamPropScriptName: &str = "OfxParamPropScriptName";
pub const ParamPropHint: &str = "OfxParamPropHint";
pub const ParamPropDefault: &str = "OfxParamPropDefault";
pub const ParamPropDoubleType: &str = "OfxParamPropDoubleType";
pub const DoubleTypeAngle: &str = "OfxParamDoubleTypeAngle";
pub const DoubleTypeXY: &str = "OfxParamDoubleTypeXY";
pub const DoubleTypeXYAbsolute: &str = "OfxParamDoubleTypeXYAbsolute";
pub const DoubleTypeScale: &str = "OfxParamDoubleTypeScale";
pub const ParamPropDefaultCoordinateSystem: &str = "OfxParamPropDefaultCoordinateSystem";
pub const ParamCoordinatesCanonical: &str = "OfxParamCoordinatesCanonical";
pub const ParamPropParent: &str = "OfxParamPropParent";
pub const ParamPropGroupOpen: &str = "OfxParamPropGroupOpen";
pub const ParamPropEnabled: &str = "OfxParamPropEnabled";
pub const ParamPropDataPtr: &str = "OfxParamPropDataPtr";
pub const ParamPropChoiceOption: &str = "OfxParamPropChoiceOption";
pub const ParamPropChoiceOrder: &str = "OfxParamPropChoiceOrder";
pub const ParamPropChoiceEnum: &str = "OfxParamPropChoiceEnum";
pub const ParamPropMin: &str = "OfxParamPropMin";
pub const ParamPropMax: &str = "OfxParamPropMax";
pub const ParamPropDisplayMin: &str = "OfxParamPropDisplayMin";
pub const ParamPropDisplayMax: &str = "OfxParamPropDisplayMax";
pub const ParamPropIncrement: &str = "OfxParamPropIncrement";
pub const ParamPropDigits: &str = "OfxParamPropDigits";
pub const ParamPropDimensionLabel: &str = "OfxParamPropDimensionLabel";
pub const ParamPropStringMode: &str = "OfxParamPropStringMode";
pub const ParamStringIsLabel: &str = "OfxParamStringIsLabel";
pub const ParamPropPageChild: &str = "OfxParamPropPageChild";

pub const PropertySuite: &str = "OfxPropertySuite";
pub const ImageEffectSuite: &str = "OfxImageEffectSuite";
pub const ParameterSuite: &str = "OfxParameterSuite";
pub const MemorySuite: &str = "OfxMemorySuite";
pub const MultiThreadSuite: &str = "OfxMultiThreadSuite";
pub const MessageSuite: &str = "OfxMessageSuite";
pub const ProgressSuite: &str = "OfxProgressSuite";
pub const TimeLineSuite: &str = "OfxTimeLineSuite";
pub const MessageQuestion: &str = "OfxMessageQuestion";
pub const MessageError: &str = "OfxMessageError";
pub const MessageFatal: &str = "OfxMessageFatal";
