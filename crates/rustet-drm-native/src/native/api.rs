//! The functions of Apple's Android libraries the session calls, each with the C++ signature
//! it really has (the forwarding stubs of `native/hybris_stubs.c`, declared in
//! `native/import.h`).
//!
//! `this` is the first argument. Functions that return an object by value return
//! [`SharedPtrRet`], [`StdString`], [`StdVector`] or [`Ignored88`] (see `cxx`). References
//! (`T const&`) are pointers. Objects passed by value are non-trivial in libc++, so the Itanium
//! ABI passes them as a pointer to a caller-owned copy as well.
//!
//! The C resolved each symbol on first use and aborted when one was missing. Here the set the
//! session can reach is resolved when FairPlay starts, and a missing symbol fails the start.
//! The C stubs for unreachable paths (offline download, music-video flavours, itun decrypt)
//! are not ported.

use std::ffi::{c_char, c_int, c_long, c_void};

use super::Error;
use super::cxx::{Ignored88, NativeErrorCode, SharedPtr, SharedPtrRet, StdString, StdVector};
use super::loader::Resolver;

type Obj = *mut c_void;

/// `void (*)(long, shared_ptr<ProtocolDialog>, shared_ptr<AndroidProtocolDialogResponseHandler>)`.
pub(crate) type DialogHandler = unsafe extern "C" fn(i64, *mut SharedPtr, *mut SharedPtr);
/// `void (*)(shared_ptr<CredentialsRequest>, shared_ptr<AndroidCredentialsResponseHandler>)`.
pub(crate) type CredentialsHandler = unsafe extern "C" fn(*mut SharedPtr, *mut SharedPtr);

macro_rules! guest_api {
    ($( $(#[$doc:meta])* $field:ident = $sym:literal : fn($($arg:ty),* $(,)?) $(-> $ret:ty)?; )*) => {
        pub(crate) struct Api {
            $( $(#[$doc])* pub(crate) $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl Api {
            pub(crate) fn resolve(r: &Resolver) -> Result<Self, Error> {
                Ok(Self {
                    $( $field: {
                        let p = r.require($sym)?;
                        // SAFETY: `p` is the non-null address of `$sym`, whose C++ signature
                        // the field type spells out (see the module docs).
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    }, )*
                })
            }
        }
    };
}

guest_api! {
    // ── drm_init_internal ──
    /// `storeservicescore::DeviceGUID::instance()`
    device_guid_instance = "_ZN17storeservicescore10DeviceGUID8instanceEv": fn() -> SharedPtrRet;
    /// `DeviceGUID::configure(string const&, string const&, unsigned const&, bool const&)`
    device_guid_configure = "_ZN17storeservicescore10DeviceGUID9configureERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEES9_RKjRKb":
        fn(Obj, *const StdString, *const StdString, *const u32, *const u8) -> Ignored88;
    /// `DeviceGUID::guid()`, a `shared_ptr<mediaplatform::Data>`
    device_guid_guid = "_ZN17storeservicescore10DeviceGUID4guidEv": fn(Obj) -> SharedPtrRet;
    /// `mediaplatform::Data::bytes() const` (not NUL-terminated)
    data_bytes = "_ZNK13mediaplatform4Data5bytesEv": fn(Obj) -> *const c_char;
    /// `mediaplatform::Data::length() const`
    data_length = "_ZNK13mediaplatform4Data6lengthEv": fn(Obj) -> usize;

    // ── drm_init_ctx ──
    /// `RequestContextConfig::RequestContextConfig()`
    config_ctor = "_ZN17storeservicescore20RequestContextConfigC2Ev": fn(Obj);
    set_base_directory_path = "_ZN17storeservicescore20RequestContextConfig20setBaseDirectoryPathERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_client_identifier = "_ZN17storeservicescore20RequestContextConfig19setClientIdentifierERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_version_identifier = "_ZN17storeservicescore20RequestContextConfig20setVersionIdentifierERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_platform_identifier = "_ZN17storeservicescore20RequestContextConfig21setPlatformIdentifierERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_product_version = "_ZN17storeservicescore20RequestContextConfig17setProductVersionERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_device_model = "_ZN17storeservicescore20RequestContextConfig14setDeviceModelERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_build_version = "_ZN17storeservicescore20RequestContextConfig15setBuildVersionERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_locale_identifier = "_ZN17storeservicescore20RequestContextConfig19setLocaleIdentifierERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_language_identifier = "_ZN17storeservicescore20RequestContextConfig21setLanguageIdentifierERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    /// `RequestContextManager::configure(shared_ptr<RequestContext> const&)`
    request_context_manager_configure = "_ZN21RequestContextManager9configureERKNSt6__ndk110shared_ptrIN17storeservicescore14RequestContextEEE": fn(*const SharedPtr);
    /// `RequestContext::init(shared_ptr<RequestContextConfig> const&)`
    request_context_init = "_ZN17storeservicescore14RequestContext4initERKNSt6__ndk110shared_ptrINS_20RequestContextConfigEEE": fn(Obj, *const SharedPtr) -> Ignored88;
    set_fair_play_directory_path = "_ZN17storeservicescore14RequestContext24setFairPlayDirectoryPathERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    set_dialog_handler = "_ZN20androidstoreservices28AndroidPresentationInterface16setDialogHandlerEPFvlNSt6__ndk110shared_ptrIN17storeservicescore14ProtocolDialogEEENS2_INS_36AndroidProtocolDialogResponseHandlerEEEE": fn(Obj, DialogHandler);
    set_credentials_handler = "_ZN20androidstoreservices28AndroidPresentationInterface21setCredentialsHandlerEPFvNSt6__ndk110shared_ptrIN17storeservicescore18CredentialsRequestEEENS2_INS_33AndroidCredentialsResponseHandlerEEEE": fn(Obj, CredentialsHandler);
    set_presentation_interface = "_ZN17storeservicescore14RequestContext24setPresentationInterfaceERKNSt6__ndk110shared_ptrINS_21PresentationInterfaceEEE": fn(Obj, *const SharedPtr);

    // ── login ──
    authenticate_flow_run = "_ZN17storeservicescore16AuthenticateFlow3runEv": fn(Obj);
    /// `AuthenticateFlow::response() const`, a `shared_ptr<AuthenticateResponse> const&`
    authenticate_flow_response = "_ZNK17storeservicescore16AuthenticateFlow8responseEv": fn(Obj) -> *const SharedPtr;
    authenticate_response_type = "_ZNK17storeservicescore20AuthenticateResponse12responseTypeEv": fn(Obj) -> c_int;
    authenticate_customer_message = "_ZNK17storeservicescore20AuthenticateResponse15customerMessageEv": fn(Obj) -> *const StdString;
    authenticate_error = "_ZNK17storeservicescore20AuthenticateResponse5errorEv": fn(Obj) -> *const SharedPtr;
    /// Returns a reference to the ErrorCode inside the condition (the Linux C stub declared an
    /// `int` return and so read the low half of that pointer through a mismatched prototype).
    store_error_code = "_ZNK17storeservicescore19StoreErrorCondition9errorCodeEv": fn(Obj) -> *const NativeErrorCode;
    store_error_what = "_ZNK17storeservicescore19StoreErrorCondition4whatEv": fn(Obj) -> *const c_char;

    // ── dialogHandler ──
    dialog_title = "_ZNK17storeservicescore14ProtocolDialog5titleEv": fn(Obj) -> *const StdString;
    dialog_message = "_ZNK17storeservicescore14ProtocolDialog7messageEv": fn(Obj) -> *const StdString;
    /// `vector<shared_ptr<ProtocolButton>> const&`
    dialog_buttons = "_ZNK17storeservicescore14ProtocolDialog7buttonsEv": fn(Obj) -> *const StdVector;
    button_title = "_ZNK17storeservicescore14ProtocolButton5titleEv": fn(Obj) -> *const StdString;
    dialog_response_ctor = "_ZN17storeservicescore22ProtocolDialogResponseC1Ev": fn(Obj);
    dialog_response_set_selected_button = "_ZN17storeservicescore22ProtocolDialogResponse17setSelectedButtonERKNSt6__ndk110shared_ptrINS_14ProtocolButtonEEE": fn(Obj, *const SharedPtr);
    handle_protocol_dialog_response = "_ZN20androidstoreservices28AndroidPresentationInterface28handleProtocolDialogResponseERKlRKNSt6__ndk110shared_ptrIN17storeservicescore22ProtocolDialogResponseEEE": fn(Obj, *const i64, *const SharedPtr);

    // ── credentialHandler ──
    credentials_title = "_ZNK17storeservicescore18CredentialsRequest5titleEv": fn(Obj) -> *const StdString;
    credentials_message = "_ZNK17storeservicescore18CredentialsRequest7messageEv": fn(Obj) -> *const StdString;
    /// A C++ `bool`, read as a byte.
    credentials_requires_code = "_ZNK17storeservicescore18CredentialsRequest28requiresHSA2VerificationCodeEv": fn(Obj) -> u8;
    credentials_response_ctor = "_ZN17storeservicescore19CredentialsResponseC1Ev": fn(Obj);
    credentials_set_user_name = "_ZN17storeservicescore19CredentialsResponse11setUserNameERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    credentials_set_password = "_ZN17storeservicescore19CredentialsResponse11setPasswordERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE": fn(Obj, *const StdString);
    credentials_set_response_type = "_ZN17storeservicescore19CredentialsResponse15setResponseTypeENS0_12ResponseTypeE": fn(Obj, c_int);
    handle_credentials_response = "_ZN20androidstoreservices28AndroidPresentationInterface25handleCredentialsResponseERKNSt6__ndk110shared_ptrIN17storeservicescore19CredentialsResponseEEE": fn(Obj, *const SharedPtr);

    // ── lease ──
    /// `SVPlaybackLeaseManager(function<void(int const&)> const&, function<void(shared_ptr<StoreErrorCondition> const&)> const&)`
    lease_manager_ctor = "_ZN22SVPlaybackLeaseManagerC2ERKNSt6__ndk18functionIFvRKiEEERKNS1_IFvRKNS0_10shared_ptrIN17storeservicescore19StoreErrorConditionEEEEEE": fn(Obj, *const c_void, *const c_void);
    refresh_lease_automatically = "_ZN22SVPlaybackLeaseManager25refreshLeaseAutomaticallyERKb": fn(Obj, *const u8);
    request_lease = "_ZN22SVPlaybackLeaseManager12requestLeaseERKb": fn(Obj, *const u8);

    // ── account ──
    /// `RequestContext::fairPlay()`, a `shared_ptr<FairPlay>`
    request_context_fair_play = "_ZN17storeservicescore14RequestContext8fairPlayEv": fn(Obj) -> SharedPtrRet;
    /// `FairPlay::getSubscriptionStatus()`, a vector of 16-byte records
    get_subscription_status = "_ZN17storeservicescore8FairPlay21getSubscriptionStatusEv": fn(Obj) -> StdVector;
    /// `RequestContext::storeFrontIdentifier(shared_ptr<URLBag> const&) const`
    store_front_identifier = "_ZNK17storeservicescore14RequestContext20storeFrontIdentifierERKNSt6__ndk110shared_ptrINS_6URLBagEEE": fn(Obj, *const SharedPtr) -> StdString;
    /// `HTTPMessage(string url, string method)`: both by value
    http_message_ctor = "_ZN13mediaplatform11HTTPMessageC2ENSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEES7_": fn(Obj, *mut StdString, *mut StdString);
    http_message_set_header = "_ZN13mediaplatform11HTTPMessage9setHeaderERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEES9_": fn(Obj, *const StdString, *const StdString);
    http_message_set_body_data = "_ZN13mediaplatform11HTTPMessage11setBodyDataEPcm": fn(Obj, *mut c_char, usize);
    url_request_ctor = "_ZN17storeservicescore10URLRequestC2ERKNSt6__ndk110shared_ptrIN13mediaplatform11HTTPMessageEEERKNS2_INS_14RequestContextEEE": fn(Obj, *const SharedPtr, *const SharedPtr);
    url_request_set_parameter = "_ZN17storeservicescore10URLRequest19setRequestParameterERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEES9_": fn(Obj, *const StdString, *const StdString);
    url_request_run = "_ZN17storeservicescore10URLRequest3runEv": fn(Obj);
    url_request_error = "_ZNK17storeservicescore10URLRequest5errorEv": fn(Obj) -> *const SharedPtr;
    url_request_response = "_ZNK17storeservicescore10URLRequest8responseEv": fn(Obj) -> *const SharedPtr;
    url_response_underlying = "_ZNK17storeservicescore11URLResponse18underlyingResponseEv": fn(Obj) -> *const SharedPtr;

    // ── key contexts and decryption ──
    /// `SVFootHillSessionCtrl::instance()`
    foothill_instance = "_ZN21SVFootHillSessionCtrl8instanceEv": fn() -> Obj;
    /// `getPersistentKey(string const& ×8)`, a `shared_ptr<SVFootHillPKey>`
    get_persistent_key = "_ZN21SVFootHillSessionCtrl16getPersistentKeyERKNSt6__ndk112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEES8_S8_S8_S8_S8_S8_S8_":
        fn(Obj, *const StdString, *const StdString, *const StdString, *const StdString,
           *const StdString, *const StdString, *const StdString, *const StdString) -> SharedPtrRet;
    /// `decryptContext(string const&, SVDecryptor::SVDecryptorType const&, bool const&)`, a
    /// `shared_ptr<SVFootHillPContext>`. The C passed only the string; the two references
    /// were left-over registers, which Apple's code does not read.
    decrypt_context = "_ZN21SVFootHillSessionCtrl14decryptContextERKNSt6__ndk112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEERKN11SVDecryptor15SVDecryptorTypeERKb":
        fn(Obj, *const StdString, *const c_int, *const u8) -> SharedPtrRet;
    /// `SVFootHillPContext::kdContext() const`: the address of the member, which itself
    /// points at the slot holding the live context.
    kd_context = "_ZNK18SVFootHillPContext9kdContextEv": fn(Obj) -> *const *mut *mut c_void;
    reset_all_contexts = "_ZN21SVFootHillSessionCtrl16resetAllContextsEv": fn(Obj);
    /// `NfcRKVnxuKZy04KWbdFu71Ou(ctx, selector, in, out, len)`: decrypts whole 16-byte blocks.
    nfc_decrypt = "NfcRKVnxuKZy04KWbdFu71Ou": fn(Obj, u32, *mut c_void, *mut c_void, usize) -> c_long;
}

/// Casts a resolved symbol to its function-pointer type.
///
/// # Safety
/// `F` must be a function-pointer type matching the symbol's real signature.
unsafe fn cast<F: Copy>(p: *mut c_void) -> F {
    debug_assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    // SAFETY: same size; the caller guarantees the signature.
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) }
}

/// The `__shared_ptr_emplace<T>` vtables of the control blocks the wrapper builds itself. The
/// C copied the first 8 slots of each into a local array and pointed the blocks there; the
/// blocks here point into the real vtable, which holds the same entries.
pub(crate) struct Vtables {
    pub dialog_response: *mut c_void,
    pub credentials_response: *mut c_void,
    pub request_context_config: *mut c_void,
    pub http_message: *mut c_void,
}

// SAFETY: vtable addresses inside Apple's libraries, which are never unloaded or written.
unsafe impl Send for Vtables {}
// SAFETY: as above.
unsafe impl Sync for Vtables {}

impl Vtables {
    pub(crate) fn resolve(r: &Resolver) -> Result<Self, Error> {
        Ok(Self {
            dialog_response: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN17storeservicescore22ProtocolDialogResponseENS_9allocatorIS2_EEEE")?,
            credentials_response: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN17storeservicescore19CredentialsResponseENS_9allocatorIS2_EEEE")?,
            request_context_config: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN17storeservicescore20RequestContextConfigENS_9allocatorIS2_EEEE")?,
            http_message: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN13mediaplatform11HTTPMessageENS_9allocatorIS2_EEEE")?,
        })
    }
}

/// `FootHillConfig::config`: older libraries take the string by `const&`, 4.9.x/5.x by value.
pub(crate) enum FootHillConfig {
    ByRef(unsafe extern "C" fn(*const StdString)),
    /// A by-value string is still passed as a pointer to the caller's object, but the newer
    /// code reads it as a properly formed libc++ string.
    ByValue(unsafe extern "C" fn(*mut StdString)),
}

/// A `make_shared<T>` the older libraries export and the newer ones inline: then the wrapper
/// builds the control block itself (see `cxx::make_shared_manual`) and runs T's constructor.
pub(crate) enum MakeShared<E, C> {
    Export(E),
    Manual { vtable: *mut c_void, ctor: C },
}

/// The symbols whose presence depends on the library version.
pub(crate) struct Variants {
    pub foothill_config: FootHillConfig,
    /// `make_shared<RequestContext>(string&)`, or the emplace vtable and `RequestContext(string const&)`.
    pub make_request_context: MakeShared<
        unsafe extern "C" fn(*mut StdString) -> SharedPtrRet,
        unsafe extern "C" fn(Obj, *const StdString),
    >,
    /// `make_shared<AndroidPresentationInterface>()`, or the emplace vtable and its constructor.
    pub make_presentation:
        MakeShared<unsafe extern "C" fn() -> SharedPtrRet, unsafe extern "C" fn(Obj)>,
    /// `make_shared<AuthenticateFlow>(shared_ptr<RequestContext>&)`, or the emplace vtable and
    /// `AuthenticateFlow(shared_ptr<RequestContext> const&)`.
    pub make_authenticate_flow: MakeShared<
        unsafe extern "C" fn(*const SharedPtr) -> SharedPtrRet,
        unsafe extern "C" fn(Obj, *const SharedPtr),
    >,
}

// SAFETY: function and vtable addresses inside Apple's libraries, never unloaded or written.
unsafe impl Send for Variants {}
// SAFETY: as above.
unsafe impl Sync for Variants {}

impl Variants {
    pub(crate) fn resolve(r: &Resolver) -> Result<Self, Error> {
        const FOOTHILL_BY_REF: &str = "_ZN14FootHillConfig6configERKNSt6__ndk112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEE";
        let foothill_config = if let Some(p) = r.find_in_appmusic(c"_ZN14FootHillConfig6configERKNSt6__ndk112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEE") {
            // SAFETY: `FootHillConfig::config(string const&)`.
            FootHillConfig::ByRef(unsafe { cast(p) })
        } else if let Some(p) = r.find_in_appmusic(c"_ZN14FootHillConfig6configENSt6__ndk112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEE") {
            // SAFETY: `FootHillConfig::config(string)`.
            FootHillConfig::ByValue(unsafe { cast(p) })
        } else {
            // SAFETY: as the first branch, found in another library.
            FootHillConfig::ByRef(unsafe { cast(r.require(FOOTHILL_BY_REF)?) })
        };
        let make_request_context = match r.find("_ZNSt6__ndk110shared_ptrIN17storeservicescore14RequestContextEE11make_sharedIJRNS_12basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEEEEES3_DpOT_") {
            // SAFETY: the export has the signature of the field (see the mangled name).
            Some(p) => MakeShared::Export(unsafe { cast(p) }),
            None => MakeShared::Manual {
                vtable: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN17storeservicescore14RequestContextENS_9allocatorIS2_EEEE")?,
                // SAFETY: the constructor has the signature of the field (see the mangled name).
                ctor: unsafe { cast(r.require("_ZN17storeservicescore14RequestContextC1ERKNSt6__ndk112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEE")?) },
            },
        };
        let make_presentation = match r.find("_ZNSt6__ndk110shared_ptrIN20androidstoreservices28AndroidPresentationInterfaceEE11make_sharedIJEEES3_DpOT_") {
            // SAFETY: the export has the signature of the field (see the mangled name).
            Some(p) => MakeShared::Export(unsafe { cast(p) }),
            None => MakeShared::Manual {
                vtable: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN20androidstoreservices28AndroidPresentationInterfaceENS_9allocatorIS2_EEEE")?,
                // SAFETY: the constructor has the signature of the field (see the mangled name).
                ctor: unsafe { cast(r.require("_ZN20androidstoreservices28AndroidPresentationInterfaceC1Ev")?) },
            },
        };
        let make_authenticate_flow = match r.find("_ZNSt6__ndk110shared_ptrIN17storeservicescore16AuthenticateFlowEE11make_sharedIJRNS0_INS1_14RequestContextEEEEEES3_DpOT_") {
            // SAFETY: the export has the signature of the field (see the mangled name).
            Some(p) => MakeShared::Export(unsafe { cast(p) }),
            None => MakeShared::Manual {
                vtable: r.require("_ZTVNSt6__ndk120__shared_ptr_emplaceIN17storeservicescore16AuthenticateFlowENS_9allocatorIS2_EEEE")?,
                // SAFETY: the constructor has the signature of the field (see the mangled name).
                ctor: unsafe { cast(r.require("_ZN17storeservicescore16AuthenticateFlowC1ERKNSt6__ndk110shared_ptrINS_14RequestContextEEE")?) },
            },
        };
        Ok(Self {
            foothill_config,
            make_request_context,
            make_presentation,
            make_authenticate_flow,
        })
    }
}
